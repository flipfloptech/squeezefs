//! THE SMOKE (charter §11 "pjdfstest is the release gate" — this is its
//! rung-3/4 stand-in; `zig build smoke`, part of `zig build gate`): the
//! built `squeezefs` binary as a child process, a 256 MiB file volume on
//! the venue's ext4, a real POSIX mount at `/build/tmp/mnt-<pid>`, the
//! shell's own tools against it, `kill -9` mid-`tar -x`, remounts, the
//! `status` summary, and the must-stay-0 gauges read through the SIGUSR1
//! stats dump (the `.stats` inode's stand-in until rung 5).
//!
//! Rung 4 adds the data plane (charter §6, §13): a SET of two sparse
//! 2 GiB data volumes, with and without a cache path (staging + the NVMe
//! tier ON / OFF), files of any size through `dd`/`cp`/`truncate`,
//! random 4 KiB overwrites against a mirrored image, 1000 packed 64 KiB
//! files and their census after the dismount promotion, an 8-way `cp`
//! storm with every ledger closed, `rm` back to the `df` baseline, and
//! the kill matrix (`kill -9` at three points of a 200 MiB `dd` and
//! mid-storm): every file whose `fsync` returned is byte-exact, no file
//! is torn beyond its last durable publish (every 1 MiB piece inside the
//! claimed size is exact or a hole), the C8 oracle and the C2 reconcile
//! report zero drift on the remount.
//!
//! The binary is `./squeezefs` in the test's cwd: `build.zig`'s `smoke`
//! step runs the library's `smoke:` tests with the emitted-bin directory as
//! the cwd, so the smoke always drives the binary of the same build (the
//! unit run from the repository root finds no binary and skips). Root,
//! `/dev/fuse`, `CAP_SYS_ADMIN` and the 7.2.6-sqz venue are required;
//! anything less skips LOUDLY.
//!
//! The durability contract the kill matrix pins is the COALESCED one
//! (charter §5.2–§5.3, `meta/backend.zig` module doc): no `fsync` is
//! called, so an acked mutation is a LANDED journal entry — whole-
//! transaction atomic, torn-write immune — and after `kill -9` every file
//! present on the remount is either complete (its one WRITE transaction
//! landed) or empty (its CREATE landed, its WRITE never issued); nothing
//! is ever partial, nothing acked is lost. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const counters = @import("../core/counters.zig");
const handler = @import("../fuse/handler.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/smoke is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const testing = std.testing;

/// The volume: 256 MiB (the porting spec's worked example: journal 8 MiB
/// by the clamp, 990 heap extents at the default 256 KiB node).
const volume_len: u64 = 256 << 20;
/// Bring-up bound: the arm barrier's own bound (30 s) plus the backend's
/// open on a cold cache.
const ready_wait_ns: u64 = 60 * std.time.ns_per_s;
/// How long a child may take to exit after `umount` (the final checkpoint
/// and the lane's trailing barrier are sub-second; the drain's lease
/// budget 100 ms; the residency backstop 30 s only when something is owed).
const exit_wait_ns: u64 = 60 * std.time.ns_per_s;
/// The inline bound on a set WITHOUT data volumes: a file of
/// `inline_max + 1` bytes has nowhere to go and is refused (EFBIG).
const inline_max: usize = 4096;
/// The kill-matrix tree: enough entries that a `tar -x` is mid-flight
/// when the daemon dies (a few hundred creates per second through the
/// first mount's conveyor), each file ≤ 1 KiB (inline).
const kill_tree_files: usize = 1500;
const kill_tree_file_len: usize = 1000;
/// Files observed on the mount before the kill is sent: deep enough into
/// the extraction that creates, writes and setattrs are all in flight.
const kill_after_files: usize = 200;

const Rig = struct {
    gpa: std.mem.Allocator,
    io: Io,
    bin: [:0]u8,
    work: [:0]u8,
    img: [:0]u8,
    meta_uri: []u8,
    /// A second metadata member and the two-member set URI (the routing
    /// stage).
    img2: [:0]u8,
    set_uri: []u8,
    mp: [:0]u8,
    log_path: [:0]u8,
    /// The data set (rung 4): two sparse volumes, the cache path, the
    /// random image every stage draws from.
    d0: [:0]u8,
    d1: [:0]u8,
    cache: [:0]u8,
    data_uri: []u8,
    image: [:0]u8,
    log_file: ?Io.File = null,
    child: ?std.process.Child = null,
    child_pid: linux.pid_t = 0,
    env: std.process.Environ.Map,

    fn init(gpa: std.mem.Allocator, io: Io) !*Rig {
        const self = try gpa.create(Rig);
        errdefer gpa.destroy(self);
        const pid = linux.getpid();
        _ = linux.mkdir("/build/tmp", 0o755);
        var cwd_buf: [linux.PATH_MAX]u8 = undefined;
        const n = Io.Dir.cwd().realPath(io, &cwd_buf) catch return error.NoCwd;
        self.* = .{
            .gpa = gpa,
            .io = io,
            .bin = try std.fmt.allocPrintSentinel(gpa, "{s}/squeezefs", .{cwd_buf[0..n]}, 0),
            .work = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/smoke-{d}", .{pid}, 0),
            .img = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/smoke-{d}/vol.img", .{pid}, 0),
            .meta_uri = try std.fmt.allocPrint(gpa, "sqmeta:///build/tmp/smoke-{d}/vol.img", .{pid}),
            .img2 = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/smoke-{d}/vol2.img", .{pid}, 0),
            .set_uri = try std.fmt.allocPrint(gpa, "sqmeta:///build/tmp/smoke-{d}/vol.img,/build/tmp/smoke-{d}/vol2.img", .{ pid, pid }),
            .mp = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/mnt-{d}", .{pid}, 0),
            .log_path = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/smoke-{d}/daemon.log", .{pid}, 0),
            .d0 = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/smoke-{d}/d0.img", .{pid}, 0),
            .d1 = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/smoke-{d}/d1.img", .{pid}, 0),
            .cache = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/smoke-{d}/cache", .{pid}, 0),
            .data_uri = try std.fmt.allocPrint(gpa, "sqdata:///build/tmp/smoke-{d}/d0.img,/build/tmp/smoke-{d}/d1.img", .{ pid, pid }),
            .image = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/smoke-{d}/image", .{pid}, 0),
            .env = std.process.Environ.Map.init(gpa),
        };
        try self.env.put("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        _ = linux.mkdir(self.work.ptr, 0o755);
        _ = linux.mkdir(self.mp.ptr, 0o755);
        // The supervisor outlives the invoking process by design and is
        // reaped by init; the venue's init (`sleep infinity`) reaps nothing,
        // so the test adopts its orphans and reaps them itself.
        _ = linux.prctl(@backingInt(linux.PR.SET_CHILD_SUBREAPER), 1, 0, 0, 0);
        return self;
    }

    /// The daemon's error / warning / tripwire lines (a passing run prints
    /// nothing; a failing one names its cause here, since the log dies
    /// with the work dir).
    fn printDaemonAlerts(self: *Rig) void {
        const text = self.readLog() catch return;
        defer self.gpa.free(text);
        var it = std.mem.splitScalar(u8, text, '\n');
        var n: usize = 0;
        while (it.next()) |line| {
            if (std.mem.startsWith(u8, line, "error(") or std.mem.startsWith(u8, line, "warn(") or std.mem.indexOf(u8, line, "TRIPWIRE") != null) {
                if (n == 0) std.debug.print("\n--- daemon alerts ---\n", .{});
                std.debug.print("{s}\n", .{line[0..@min(line.len, 400)]});
                n += 1;
                if (n == 40) break;
            }
        }
    }

    /// Reap every orphan adopted as the subreaper (bounded wait).
    fn reapOrphans(self: *Rig) void {
        _ = self;
        const deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
        while (clock.monotonicNs() < deadline) {
            var status: i32 = 0;
            const rc = linux.waitpid(-1, &status, linux.W.NOHANG);
            switch (linux.errno(rc)) {
                .SUCCESS => if (rc == 0) sleepMs(20) else continue,
                else => return,
            }
        }
    }

    fn deinit(self: *Rig) void {
        if (self.child) |*c| {
            _ = linux.kill(self.child_pid, .KILL);
            _ = c.wait(self.io) catch {};
        }
        self.printDaemonAlerts();
        // Never leave a mount behind.
        _ = linux.umount2(self.mp.ptr, linux.MNT.DETACH);
        if (self.log_file) |f| f.close(self.io);
        _ = linux.rmdir(self.mp.ptr);
        Io.Dir.cwd().deleteTree(self.io, self.work) catch {};
        self.reapOrphans();
        self.env.deinit();
        self.gpa.free(self.bin);
        self.gpa.free(self.work);
        self.gpa.free(self.img);
        self.gpa.free(self.meta_uri);
        self.gpa.free(self.img2);
        self.gpa.free(self.set_uri);
        self.gpa.free(self.mp);
        self.gpa.free(self.log_path);
        self.gpa.free(self.d0);
        self.gpa.free(self.d1);
        self.gpa.free(self.cache);
        self.gpa.free(self.data_uri);
        self.gpa.free(self.image);
        self.gpa.destroy(self);
    }

    /// The meta volume file, sparse.
    fn createMetaVolume(self: *Rig) !void {
        const f = try Io.Dir.cwd().createFile(self.io, self.img, .{ .truncate = true });
        defer f.close(self.io);
        try f.setLength(self.io, volume_len);
    }

    /// The SET: the meta volume, two sparse data volumes, the format with
    /// or without the cache path (staging + the NVMe tier ON / OFF).
    fn formatSet(self: *Rig, cache: bool) !void {
        return self.formatSetWith(cache, &.{});
    }

    /// `formatSet` with extra format flags (`--block-size`).
    fn formatSetWith(self: *Rig, cache: bool, extra: []const []const u8) !void {
        try self.createMetaVolume();
        for ([_][]const u8{ self.d0, self.d1 }) |dp| {
            const f = try Io.Dir.cwd().createFile(self.io, dp, .{ .truncate = true });
            defer f.close(self.io);
            try f.setLength(self.io, data_len);
        }
        if (cache) _ = linux.mkdir(self.cache.ptr, 0o700);
        var argv: [12][]const u8 = undefined;
        var n: usize = 0;
        argv[n] = "format";
        n += 1;
        for (extra) |a| {
            argv[n] = a;
            n += 1;
        }
        if (cache) {
            argv[n] = "--disk-cache-paths";
            argv[n + 1] = self.cache;
            n += 2;
        }
        argv[n] = self.meta_uri;
        argv[n + 1] = self.data_uri;
        n += 2;
        const r = try self.squeezefs(argv[0..n]);
        defer self.freeRun(r);
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "data volumes 2") != null);
    }

    /// The random image on ext4 (`image_mib` MiB of urandom) every stage
    /// slices; plus a mirror the overwrite stage mutates in step.
    fn createImage(self: *Rig) !void {
        var script: std.Io.Writer.Allocating = .init(self.gpa);
        defer script.deinit();
        try script.writer.print("head -c {d}M /dev/urandom > image && test \"$(stat -c %s image)\" = {d}", .{ image_mib, image_mib << 20 });
        const out = try self.shOk(self.work, script.written());
        self.gpa.free(out);
    }

    /// SIGUSR1 → every stats family lands in the log; the tail from the
    /// mark (owned by the caller).
    fn statsTail(self: *Rig) ![]u8 {
        const mark = try self.logLen();
        _ = linux.kill(self.child_pid, .USR1);
        _ = try self.awaitLog("stats daemon: ", mark, 10 * std.time.ns_per_s);
        const text = try self.readLog();
        defer self.gpa.free(text);
        return self.gpa.dupe(u8, text[mark..]);
    }

    /// The stats of a QUIESCENT mount: polled (bounded) until the reclaim
    /// ledger has drained (`terminal ≡ queued + elided`, an empty queue)
    /// and the write pipeline holds nothing — the frees a truncate or an
    /// overwrite displaced are asynchronous (the reclaimer's park bound is
    /// 1 s). The last tail is returned either way; the checks judge it.
    fn quiescentStatsTail(self: *Rig) ![]u8 {
        const deadline = clock.monotonicNs() + 20 * std.time.ns_per_s;
        while (true) {
            const tail = try self.statsTail();
            const queued = numberAfter(tail, "\"block_free_reclaim_queued\":") orelse 0;
            const elided = numberAfter(tail, "\"block_free_reclaim_elided\":") orelse 0;
            const terminal = numberAfter(tail, "\"terminal_frees\":") orelse 0;
            const queue_bytes = numberAfter(tail, "\"block_free_reclaim_queue_bytes\":") orelse 0;
            const inflight = numberAfter(tail, "\"write_pipeline_inflight_bytes\":") orelse 0;
            if ((terminal == queued + elided and queue_bytes == 0 and inflight == 0) or clock.monotonicNs() >= deadline) return tail;
            self.gpa.free(tail);
            sleepMs(100);
        }
    }

    /// A `sh -e` script with `$1 = work dir` run with the mount as cwd;
    /// stdout on success, both streams printed on failure.
    fn onMount(self: *Rig, script: []const u8) ![]u8 {
        return self.onMountArgv(script, &.{ "sh", "-e", "-c", script, "smoke", self.work });
    }

    /// `onMount` under `timeout -s KILL <secs>`: a hung pass fails instead
    /// of hanging the suite (the looped repro's per-pass bound).
    fn onMountTimed(self: *Rig, script: []const u8, secs: u32) ![]u8 {
        var buf: [16]u8 = undefined;
        const s = try std.fmt.bufPrint(&buf, "{d}", .{secs});
        return self.onMountArgv(script, &.{ "timeout", "-s", "KILL", s, "sh", "-e", "-c", script, "smoke", self.work });
    }

    fn onMountArgv(self: *Rig, script: []const u8, argv: []const []const u8) ![]u8 {
        const r = try std.process.run(self.gpa, self.io, .{ .argv = argv, .cwd = .{ .path = self.mp }, .environ_map = &self.env });
        defer self.gpa.free(r.stderr);
        if (!r.term.success()) {
            std.debug.print("\n--- mount script failed ({f}) ---\n{s}\n--- stdout ---\n{s}\n--- stderr ---\n{s}\n", .{ r.term, script, r.stdout, r.stderr });
            self.gpa.free(r.stdout);
            return error.ScriptFailed;
        }
        return r.stdout;
    }

    /// `df -kP` on the mount: the used KiB the daemon reports (statfs).
    fn usedKib(self: *Rig) !u64 {
        const out = try self.onMount("df -kP . | tail -1 | awk '{print $3}'");
        defer self.gpa.free(out);
        return std.fmt.parseInt(u64, std.mem.trim(u8, out, " \n"), 10) catch error.DfUnparsed;
    }

    /// Bounded poll until `df` returns to `baseline` (the reclaimer's
    /// park bound is 1 s; the terminal frees of a `rm` drain within it).
    fn awaitUsedKib(self: *Rig, baseline: u64) !void {
        const deadline = clock.monotonicNs() + 20 * std.time.ns_per_s;
        while (clock.monotonicNs() < deadline) {
            if (try self.usedKib() == baseline) return;
            sleepMs(50);
        }
        std.debug.print("\n--- df did not return to the baseline: {d} KiB used vs {d} ---\n", .{ try self.usedKib(), baseline });
        const tail = try self.statsTail();
        defer self.gpa.free(tail);
        inline for (.{ "fuse_reclaim_destroys", "fuse_reclaim_drains", "fuse_reclaim_blocks_freed", "bindings_freed", "terminal_frees", "block_free_reclaim_queued", "block_free_reclaim_elided", "block_free_reclaim_skipped", "block_free_reclaim_queue_bytes", "meta_kv_block_refs_released", "meta_kv_block_refs_staged", "block_live_free_refusals", "block_double_frees" }) |k| {
            std.debug.print("    {s} = {d}\n", .{ k, numberAfter(tail, "\"" ++ k ++ "\":") orelse 0 });
        }
        if (std.mem.indexOf(u8, tail, "\"volumes\":[")) |i| std.debug.print("    {s}\n", .{tail[i..@min(tail.len, i + 600)]});
        return error.SpaceNotReclaimed;
    }

    /// The `status` report (owned).
    fn statusReport(self: *Rig) ![]u8 {
        const r = try self.squeezefs(&.{ "status", self.meta_uri });
        defer self.gpa.free(r.stderr);
        if (!r.term.success()) {
            std.debug.print("\nstatus: {s}\n", .{r.stderr});
            self.gpa.free(r.stdout);
            return error.StatusFailed;
        }
        return r.stdout;
    }

    /// Run the binary with `args`, collecting its output.
    fn squeezefs(self: *Rig, args: []const []const u8) !std.process.RunResult {
        var argv: [16][]const u8 = undefined;
        argv[0] = self.bin;
        for (args, 0..) |a, i| argv[i + 1] = a;
        return std.process.run(self.gpa, self.io, .{ .argv = argv[0 .. args.len + 1], .environ_map = &self.env });
    }

    /// `sh -c script` in `cwd`; the result's streams are the caller's.
    fn sh(self: *Rig, cwd: []const u8, script: []const u8) !std.process.RunResult {
        return std.process.run(self.gpa, self.io, .{ .argv = &.{ "sh", "-e", "-c", script }, .cwd = .{ .path = cwd }, .environ_map = &self.env });
    }

    /// `sh` that must succeed: a failure prints both streams.
    fn shOk(self: *Rig, cwd: []const u8, script: []const u8) ![]u8 {
        const r = try self.sh(cwd, script);
        defer self.gpa.free(r.stderr);
        if (!r.term.success()) {
            std.debug.print("\n--- script failed ({f}) ---\n{s}\n--- stdout ---\n{s}\n--- stderr ---\n{s}\n", .{ r.term, script, r.stdout, r.stderr });
            self.gpa.free(r.stdout);
            return error.ScriptFailed;
        }
        return r.stdout;
    }

    fn freeRun(self: *Rig, r: std.process.RunResult) void {
        self.gpa.free(r.stdout);
        self.gpa.free(r.stderr);
    }

    /// Spawn the foreground mount with its stderr appended to the log file.
    fn spawnMount(self: *Rig, extra: []const []const u8) !void {
        return self.spawnMountOf(self.meta_uri, extra);
    }

    fn spawnMountOf(self: *Rig, meta_uri: []const u8, extra: []const []const u8) !void {
        std.debug.assert(self.child == null);
        if (self.log_file == null) self.log_file = try Io.Dir.cwd().createFile(self.io, self.log_path, .{ .truncate = false });
        var argv: [16][]const u8 = undefined;
        var n: usize = 0;
        argv[n] = self.bin;
        n += 1;
        argv[n] = "mount";
        n += 1;
        for (extra) |a| {
            argv[n] = a;
            n += 1;
        }
        argv[n] = meta_uri;
        n += 1;
        argv[n] = self.mp;
        n += 1;
        self.child = try std.process.spawn(self.io, .{ .argv = argv[0..n], .environ_map = &self.env, .stdin = .ignore, .stdout = .{ .file = self.log_file.? }, .stderr = .{ .file = self.log_file.? } });
        self.child_pid = self.child.?.id.?;
    }

    fn readLog(self: *Rig) ![]u8 {
        return Io.Dir.cwd().readFileAlloc(self.io, self.log_path, self.gpa, .limited(16 << 20));
    }

    /// Bounded poll for a substring in the log (counted from `from`).
    fn awaitLog(self: *Rig, needle: []const u8, from: usize, deadline_ns: u64) !usize {
        const deadline = clock.monotonicNs() + deadline_ns;
        while (true) {
            const text = try self.readLog();
            defer self.gpa.free(text);
            if (text.len > from) if (std.mem.indexOf(u8, text[from..], needle)) |i| return from + i + needle.len;
            if (self.child) |*c| if (!alive(c.id.?)) {
                std.debug.print("\n--- the daemon exited while waiting for '{s}' ---\n{s}\n", .{ needle, text });
                return error.DaemonExited;
            };
            if (clock.monotonicNs() >= deadline) {
                std.debug.print("\n--- timed out waiting for '{s}' ---\n{s}\n", .{ needle, text });
                return error.Timeout;
            }
            sleepMs(20);
        }
    }

    fn awaitReady(self: *Rig, from: usize) !usize {
        return self.awaitLog("ready: ", from, ready_wait_ns);
    }

    /// Wait for the mount child to exit; its term.
    fn waitChild(self: *Rig) !std.process.Child.Term {
        var c = self.child.?;
        self.child = null;
        const deadline = clock.monotonicNs() + exit_wait_ns;
        while (alive(self.child_pid) and clock.monotonicNs() < deadline) sleepMs(20);
        if (alive(self.child_pid)) {
            std.debug.print("\n--- the daemon did not exit within {d} s; killing ---\n", .{exit_wait_ns / std.time.ns_per_s});
            _ = linux.kill(self.child_pid, .KILL);
        }
        return c.wait(self.io);
    }

    fn umount(self: *Rig) !void {
        const r = try self.squeezefs(&.{ "umount", self.mp });
        defer self.freeRun(r);
        if (!r.term.success()) std.debug.print("\numount: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }

    fn logLen(self: *Rig) !usize {
        const text = try self.readLog();
        defer self.gpa.free(text);
        return text.len;
    }
};

fn alive(pid: linux.pid_t) bool {
    // A zombie still answers kill(0); `waitpid(WNOHANG)` would reap it out
    // from under `Child.wait`, so the liveness read is `/proc/<pid>/stat`'s
    // state letter.
    var pb: [64]u8 = undefined;
    var buf: [512]u8 = undefined;
    const path = std.fmt.bufPrint(&pb, "/proc/{d}/stat", .{pid}) catch return false;
    const text = Io.Dir.cwd().readFile(testing.io, path, &buf) catch return false;
    const close = std.mem.lastIndexOfScalar(u8, text, ')') orelse return false;
    if (close + 2 >= text.len) return false;
    return text[close + 2] != 'Z' and text[close + 2] != 'X';
}

fn sleepMs(ms: u64) void {
    const ts: linux.timespec = .{ .sec = @intCast(ms / 1000), .nsec = @intCast((ms % 1000) * std.time.ns_per_ms) };
    _ = linux.nanosleep(&ts, null);
}

/// Root, `/dev/fuse`, `CAP_SYS_ADMIN`, the binary beside us.
fn venueAvailable(io: Io) bool {
    if (linux.getuid() != 0) return false;
    const rc = linux.open("/dev/fuse", .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return false;
    _ = linux.close(@intCast(rc));
    var buf: [4096]u8 = undefined;
    const text = Io.Dir.cwd().readFile(io, "/proc/self/status", &buf) catch return false;
    var it = std.mem.splitScalar(u8, text, '\n');
    var cap_ok = false;
    while (it.next()) |line| {
        if (!std.mem.startsWith(u8, line, "CapEff:")) continue;
        const caps = std.fmt.parseInt(u64, std.mem.trim(u8, line["CapEff:".len..], " \t"), 16) catch return false;
        cap_ok = caps & (@as(u64, 1) << 21) != 0;
    }
    if (!cap_ok) {
        log.warn("smoke: CAP_SYS_ADMIN is not in this container's effective set — mount(2) would be EPERM; recreate the venue with `scripts/venue up`. Skipping.", .{});
        return false;
    }
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, "squeezefs", 0, .{ .TYPE = true }, &st)) != .SUCCESS) {
        log.warn("smoke: no ./squeezefs binary in the cwd — run through `zig build smoke`. Skipping.", .{});
        return false;
    }
    return true;
}

fn fuseMountsOf(gpa: std.mem.Allocator, io: Io, mp: []const u8) !usize {
    // procfs reports size 0: a fixed buffer, never a size-driven read.
    const buf = try gpa.alloc(u8, 1 << 20);
    defer gpa.free(buf);
    const text = try Io.Dir.cwd().readFile(io, "/proc/self/mounts", buf);
    var n: usize = 0;
    var it = std.mem.splitScalar(u8, text, '\n');
    while (it.next()) |line| {
        if (std.mem.indexOf(u8, line, mp) != null and std.mem.indexOf(u8, line, " fuse") != null) n += 1;
    }
    return n;
}

fn countFiles(gpa: std.mem.Allocator, io: Io, dir: Io.Dir) !usize {
    _ = gpa;
    var n: usize = 0;
    var it = dir.iterate();
    while (try it.next(io)) |ent| {
        if (ent.kind == .file) n += 1;
    }
    return n;
}

/// The small tree the POSIX pass copies and tars: a dozen files ≤ 4 KiB
/// over two directory levels, plus a symlink.
const tree_script =
    \\mkdir -p src/a/b src/c
    \\printf 'alpha\n' > src/a/one.txt
    \\head -c 4096 /dev/urandom > src/a/b/full-page.bin
    \\head -c 1 /dev/urandom > src/a/b/one-byte.bin
    \\: > src/c/empty
    \\seq 1 300 > src/c/numbers.txt
    \\printf 'x%.0s' $(seq 1 2048) > src/c/x2048.txt
    \\ln -s a/one.txt src/link-to-one
    \\tar -cf small.tar -C src .
    \\tar -tf small.tar | wc -l
;

/// The POSIX pass on the mount: every verb of the task list, each with a
/// check the shell can state. `$W` is the work dir on ext4.
const posix_script =
    \\W="$1"
    \\mkdir d1
    \\mkdir -p d1/d2/d3
    \\touch f1
    \\echo hello > f2
    \\test "$(cat f2)" = hello
    \\ls -la | grep -q ' f2$'
    \\ls -la d1 | grep -q ' d2$'
    \\mv f2 d1/f2m
    \\test ! -e f2
    \\test "$(cat d1/f2m)" = hello
    \\ln d1/f2m hl
    \\test "$(stat -c %h hl)" = 2
    \\test "$(stat -c %i hl)" = "$(stat -c %i d1/f2m)"
    \\ln -s d1/f2m sl
    \\test "$(readlink sl)" = d1/f2m
    \\test "$(cat sl)" = hello
    \\stat -c '%F %s %h' f1 | grep -q '^regular empty file 0 1$'
    \\stat -c '%F' d1 | grep -q '^directory$'
    \\chmod 640 f1
    \\test "$(stat -c %a f1)" = 640
    \\if command -v setfattr >/dev/null 2>&1; then
    \\  setfattr -n user.smoke -v value1 f1
    \\  test "$(getfattr -n user.smoke --only-values f1)" = value1
    \\  setfattr -x user.smoke f1
    \\  echo xattr-checked
    \\else
    \\  echo xattr-skipped
    \\fi
    \\cp -r "$W/src" tree
    \\diff -r "$W/src" tree
    \\mkdir untar
    \\tar -xf "$W/small.tar" -C untar
    \\diff -r "$W/src" untar
    \\test "$(readlink untar/link-to-one)" = a/one.txt
    \\find . | sort > "$W/find.out"
    \\test "$(wc -l < "$W/find.out")" -ge 20
    \\if head -c 4097 /dev/zero > too-big 2> "$W/efbig.err"; then echo "EFBIG MISSING"; exit 1; fi
    \\grep -q 'File too large' "$W/efbig.err"
    \\test "$(stat -c %s too-big)" -le 4096
    \\head -c 4096 /dev/zero > exactly-a-page
    \\test "$(stat -c %s exactly-a-page)" = 4096
    \\rm too-big exactly-a-page
    \\rm -rf tree untar
    \\test ! -e tree
    \\echo posix-pass-ok
;

/// After the remount: the kept files are intact.
const verify_script =
    \\W="$1"
    \\test "$(cat d1/f2m)" = hello
    \\test "$(cat hl)" = hello
    \\test "$(stat -c %h hl)" = 2
    \\test "$(readlink sl)" = d1/f2m
    \\test "$(stat -c %a f1)" = 640
    \\test -d d1/d2/d3
    \\find . | sort > "$W/find2.out"
    \\echo remount-ok
;

test "smoke: format → mount → the POSIX pass through the shell → umount → remount intact → status; the gauges through SIGUSR1" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();

    // The volume.
    {
        const f = try Io.Dir.cwd().createFile(io, rig.img, .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, volume_len);
    }
    // format; a second format without --force refuses naming the flag.
    {
        const r = try rig.squeezefs(&.{ "format", "--single-writer", rig.meta_uri });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "formatted sqmeta://") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "set config generation 1") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "no data volumes") != null);
    }
    {
        const r = try rig.squeezefs(&.{ "format", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "already formatted") != null);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "--force") != null);
    }
    // status before any mount: not mounted, next_ino 2, the record read
    // offline from the control tree.
    {
        const r = try rig.squeezefs(&.{ "status", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "format v4 (one-member forest)") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "next_ino 2,") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "not mounted") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "set config generation 1; block 4194304 B; chunk 4194304 B") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "data volumes 0") != null);
    }
    // The small tree + tarball on ext4.
    {
        const out = try rig.shOk(rig.work, tree_script);
        defer gpa.free(out);
    }

    // mount (foreground, a child) → ready.
    try rig.spawnMount(&.{});
    const ready_at = try rig.awaitReady(0);
    try testing.expectEqual(@as(usize, 1), try fuseMountsOf(gpa, io, rig.mp));
    // A second mount of the same volume is refused by layer A (EBUSY).
    {
        var pb: [128:0]u8 = undefined;
        const mp2 = try std.fmt.bufPrintSentinel(&pb, "{s}-second", .{rig.mp}, 0);
        _ = linux.mkdir(mp2.ptr, 0o755);
        defer _ = linux.rmdir(mp2.ptr);
        const r = try rig.squeezefs(&.{ "mount", rig.meta_uri, mp2 });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "single-writer guard") != null);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "BUSY") != null);
    }
    // status while mounted: the guard is held.
    {
        const r = try rig.squeezefs(&.{ "status", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "MOUNTED by a live daemon") != null);
    }
    // The POSIX pass.
    {
        var script: std.Io.Writer.Allocating = .init(gpa);
        defer script.deinit();
        try script.writer.print("{s}\n", .{posix_script});
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "sh", "-e", "-c", script.written(), "smoke", rig.work }, .cwd = .{ .path = rig.mp }, .environ_map = &rig.env });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\n--- POSIX pass failed ({f}) ---\n{s}\n--- stderr ---\n{s}\n", .{ r.term, r.stdout, r.stderr });
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "posix-pass-ok") != null);
        std.debug.print("\nsmoke: POSIX pass ok ({s})\n", .{if (std.mem.indexOf(u8, r.stdout, "xattr-checked") != null) "setfattr/getfattr checked" else "setfattr/getfattr absent, skipped"});
    }
    // The gauges through SIGUSR1: the stats families land in the log.
    {
        const mark = try rig.logLen();
        _ = linux.kill(rig.child_pid, .USR1);
        _ = try rig.awaitLog("stats daemon: ", mark, 10 * std.time.ns_per_s);
        const text = try rig.readLog();
        defer gpa.free(text);
        const tail = text[mark..];
        try testing.expect(std.mem.indexOf(u8, tail, "stats transport: {") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "stats exec: {") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "stats mem_budget: {") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"transport_requests_abandoned\":0,") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"open_count_stranded\":0,") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"transport_park_tick_commit_rescues\":0,") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"transport_park_tick_cqe_rescues\":0,") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"exec_lane_tick_rescues\":0,") != null);
        try testing.expect((numberAfter(tail, "\"fuse_write_no_data_volume_refused\":") orelse 0) >= 1);
        try testing.expect(std.mem.indexOf(u8, tail, "\"volume_failed\":false") != null);
        const reqs = numberAfter(tail, "\"transport_ring_requests\":") orelse 0;
        const enters = numberAfter(tail, "\"transport_enters\":") orelse 0;
        const wakes = numberAfter(tail, "\"transport_wake_writes\":") orelse 0;
        const elided = numberAfter(tail, "\"transport_wakes_elided\":") orelse 0;
        const ops = numberAfter(tail, "\"fuse_ops\":") orelse 0;
        try testing.expect(reqs > 0 and ops > 0);
        std.debug.print("smoke: POSIX pass: {d} fuse ops, {d} ring requests, {d} enters ({d}.{d:0>2} per request), wake writes {d} / elided {d}\n", .{ ops, reqs, enters, enters / @max(reqs, 1), (enters * 100 / @max(reqs, 1)) % 100, wakes, elided });
    }
    // umount → the daemon exits 0 → remount → intact → status.
    try rig.umount();
    const term1 = try rig.waitChild();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, term1);
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[ready_at..], "clean unmount") != null);
        try testing.expect(std.mem.indexOf(u8, text[ready_at..], "open_count_stranded=0 transport_requests_abandoned=0 tick_rescues=0/0") != null);
    }
    const mark2 = try rig.logLen();
    try rig.spawnMount(&.{});
    const ready2 = try rig.awaitReady(mark2);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        // The backend's own verdict: the previous unmount left an empty window.
        try testing.expect(std.mem.indexOf(u8, text[mark2..ready2], ", clean unmount") != null);
        try testing.expect(std.mem.indexOf(u8, text[mark2..ready2], "0 torn dropped") != null);
    }
    {
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "sh", "-e", "-c", verify_script, "smoke", rig.work }, .cwd = .{ .path = rig.mp }, .environ_map = &rig.env });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\n--- remount verify failed ({f}) ---\n{s}\n{s}\n", .{ r.term, r.stdout, r.stderr });
        try testing.expect(r.term.success());
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    {
        const r = try rig.squeezefs(&.{ "status", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "summary: v4, 256 MiB, checkpoint seq ") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "not mounted") != null);
        std.debug.print("smoke: {s}", .{r.stdout[std.mem.lastIndexOf(u8, r.stdout, "summary:").?..]});
    }
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
}

// ---------------------------------------------------------------------------
// Dynamic routing (charter §5.4): a two-member set through the binary.
// ---------------------------------------------------------------------------

/// Files the routing stage creates over several directories: enough that
/// every rotor slot of both members takes mints (2 × 64 slots) many times
/// over, small enough to stay far under the gate's data budget.
const routing_dirs: usize = 8;
const routing_files_per_dir: usize = 250;

fn routingTreeScript(gpa: std.mem.Allocator) ![]u8 {
    var w: std.Io.Writer.Allocating = .init(gpa);
    errdefer w.deinit();
    try w.writer.print("for d in $(seq 0 {d}); do mkdir -p r$d; for i in $(seq 1 {d}); do echo \"$d-$i\" > r$d/f$i; done; done; find . -path ./.stats -prune -o -type f -print | wc -l", .{ routing_dirs - 1, routing_files_per_dir });
    return w.toOwnedSlice();
}

const routing_verify_script =
    \\n=$(find . -path ./.stats -prune -o -type f -print | wc -l); echo "files=$n"
    \\for d in $(seq 0 7); do for i in 1 125 250; do v=$(cat r$d/f$i); [ "$v" = "$d-$i" ] || { echo "MISMATCH r$d/f$i=[$v] ino=$(stat -c %i r$d/f$i)"; exit 1; }; done; done
    \\echo routing-verify-ok
;

test "smoke: a TWO-MEMBER metadata set through the binary — format sqmeta://a,b → mount → 2000 files over 8 dirs spread over both members (routing_* gauges) → remount in the other URI order, every file present → volume list → remove-meta offline → mount with one member, every file present" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    for ([_][:0]const u8{ rig.img, rig.img2 }) |p| {
        const f = try Io.Dir.cwd().createFile(io, p, .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, volume_len);
    }
    {
        const r = try rig.squeezefs(&.{ "format", rig.set_uri });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "2-member set") != null);
    }
    {
        const r = try rig.squeezefs(&.{ "volume", "list", rig.set_uri });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nvolume list: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "position 0/2") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "position 1/2") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "the native keyspace") != null);
    }
    // Mount the set, build the tree, read the gauges.
    try rig.spawnMountOf(rig.set_uri, &.{});
    _ = try rig.awaitReady(0);
    {
        const script = try routingTreeScript(gpa);
        defer gpa.free(script);
        const out = try rig.onMountTimed(script, 600);
        defer gpa.free(out);
        try testing.expectEqual(routing_dirs * routing_files_per_dir, try std.fmt.parseInt(usize, std.mem.trim(u8, out, " \n"), 10));
    }
    {
        const tail = try rig.statsTail();
        defer gpa.free(tail);
        try testing.expectEqual(@as(?u64, 2), numberAfter(tail, "\"routing_volumes\":"));
        try testing.expectEqual(@as(?u64, 65536), numberAfter(tail, "\"routing_width\":"));
        // Both members' rotors took mints: ≥ 64 guest trees on each
        // (member 0: 63 guests + the native slot; member 1: 64 guests).
        try testing.expect((numberAfter(tail, "\"routing_guest_trees_minted\":") orelse 0) >= 2 * 63);
        try testing.expect((numberAfter(tail, "\"routing_mint_volume_picks\":") orelse 0) > 0);
        try testing.expect((numberAfter(tail, "\"routing_split_commits\":") orelse 0) > 0);
        try testing.expect(std.mem.indexOf(u8, tail, "\"routing_same_epoch_dual_claim\":0") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"volume_failed\":false") != null);
        std.debug.print("\nsmoke: two-member set: guest trees {d}, cross-member mints {d}, split commits {d}, gate parks {d}\n", .{ numberAfter(tail, "\"routing_guest_trees_minted\":") orelse 0, numberAfter(tail, "\"routing_mint_volume_picks\":") orelse 0, numberAfter(tail, "\"routing_split_commits\":") orelse 0, numberAfter(tail, "\"routing_gate_parks\":") orelse 0 });
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    // Remount in the OTHER URI order: discovery orders by the stamps.
    const reversed = try std.fmt.allocPrint(gpa, "sqmeta://{s},{s}", .{ rig.img2, rig.img });
    defer gpa.free(reversed);
    const mark = try rig.logLen();
    try rig.spawnMountOf(reversed, &.{});
    _ = try rig.awaitReady(mark);
    {
        const out = try rig.onMount(routing_verify_script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "files=2000") != null);
        try testing.expect(std.mem.indexOf(u8, out, "routing-verify-ok") != null);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    // remove-meta (offline): member 1 retires, its slots move onto
    // member 0; the tombstone refuses discovery; one member serves all.
    {
        const r = try rig.squeezefs(&.{ "volume", "remove-meta", rig.set_uri, rig.img2 });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nremove-meta: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "retired at epoch 2") != null);
    }
    {
        const r = try rig.squeezefs(&.{ "volume", "list", rig.set_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "RETIRED") != null);
    }
    const mark3 = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark3);
    {
        const out = try rig.onMount(routing_verify_script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "files=2000") != null);
        try testing.expect(std.mem.indexOf(u8, out, "routing-verify-ok") != null);
        const tail = try rig.statsTail();
        defer gpa.free(tail);
        try testing.expectEqual(@as(?u64, 1), numberAfter(tail, "\"routing_volumes\":"));
    }
    // The dataset goes; the mount ends clean.
    {
        const out = try rig.onMount("rm -rf r0 r1 r2 r3 r4 r5 r6 r7; echo gone");
        defer gpa.free(out);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
}

fn numberAfter(text: []const u8, key: []const u8) ?u64 {
    const i = std.mem.indexOf(u8, text, key) orelse return null;
    var j = i + key.len;
    while (j < text.len and std.ascii.isDigit(text[j])) j += 1;
    return std.fmt.parseInt(u64, text[i + key.len .. j], 10) catch null;
}

test "smoke: kill -9 mid-tar -x → remount replays the window → every file present is whole or empty, none acked is lost, nothing torn after the crash; a clean cycle after it" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    {
        const f = try Io.Dir.cwd().createFile(io, rig.img, .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, volume_len);
    }
    {
        const r = try rig.squeezefs(&.{ "format", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
    }
    // The kill tree on ext4: `kill_tree_files` files of `kill_tree_file_len`
    // distinct bytes each (the content names the file, so a swapped body
    // would be caught).
    {
        var script: std.Io.Writer.Allocating = .init(gpa);
        defer script.deinit();
        try script.writer.print(
            \\mkdir -p ktree
            \\i=0
            \\while [ $i -lt {d} ]; do
            \\  printf 'file-%05d:' $i > ktree/f$i
            \\  head -c {d} /dev/zero | tr '\0' "$(printf '\\%03o' $((97 + i % 26)))" >> ktree/f$i
            \\  i=$((i + 1))
            \\done
            \\tar -cf ktree.tar -C ktree .
        , .{ kill_tree_files, kill_tree_file_len - 11 });
        const out = try rig.shOk(rig.work, script.written());
        defer gpa.free(out);
    }
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    // tar -x in a child; kill the daemon once enough files exist.
    var tar_argv_buf: [64]u8 = undefined;
    const tar_path = try std.fmt.bufPrint(&tar_argv_buf, "{s}/ktree.tar", .{rig.work});
    var tar = try std.process.spawn(io, .{ .argv = &.{ "tar", "-xf", tar_path, "-C", rig.mp }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .ignore });
    var seen: usize = 0;
    {
        const deadline = clock.monotonicNs() + 60 * std.time.ns_per_s;
        while (clock.monotonicNs() < deadline) {
            var dir = try Io.Dir.cwd().openDir(io, rig.mp, .{ .iterate = true });
            defer dir.close(io);
            seen = try countFiles(gpa, io, dir);
            if (seen >= kill_after_files) break;
            sleepMs(5);
        }
    }
    try testing.expect(seen >= kill_after_files);
    _ = linux.kill(rig.child_pid, .KILL);
    const killed = try rig.waitChild();
    try testing.expectEqual(std.process.Child.Term{ .signal = .KILL }, killed);
    // tar fails (ENOTCONN) and exits; the dead connection unmounts.
    const tar_term = try tar.wait(io);
    try testing.expect(!tar_term.success());
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(rig.mp.ptr, 0)));
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    std.debug.print("\nsmoke: kill -9 sent with {d} of {d} files visible\n", .{ seen, kill_tree_files });

    // Remount: the window replays (NOT a clean unmount), nothing torn is a
    // corruption alert only after a clean one — here it is counted.
    const mark = try rig.logLen();
    try rig.spawnMount(&.{});
    const ready2 = try rig.awaitReady(mark);
    var replayed: u64 = 0;
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        const seg = text[mark..ready2];
        try testing.expect(std.mem.indexOf(u8, seg, "mounted — ledger seq") != null);
        try testing.expect(std.mem.indexOf(u8, seg, ", clean unmount") == null);
        replayed = numberAfter(seg, "replayed ") orelse 0;
    }
    // Every file present is whole or empty; count both classes.
    var whole: usize = 0;
    var empty: usize = 0;
    var present: usize = 0;
    {
        var dir = try Io.Dir.cwd().openDir(io, rig.mp, .{ .iterate = true });
        defer dir.close(io);
        var it = dir.iterate();
        while (try it.next(io)) |ent| {
            if (ent.kind != .file) continue;
            present += 1;
            const got = try dir.readFileAlloc(io, ent.name, gpa, .limited(1 << 20));
            defer gpa.free(got);
            if (got.len == 0) {
                empty += 1;
                continue;
            }
            // The expected body from the name: `file-NNNNN:` + the letter.
            const idx = try std.fmt.parseInt(usize, ent.name[1..], 10);
            var want: [kill_tree_file_len]u8 = undefined;
            const head = try std.fmt.bufPrint(&want, "file-{d:0>5}:", .{idx});
            @memset(want[head.len..], 'a' + @as(u8, @intCast(idx % 26)));
            if (!std.mem.eql(u8, got, &want)) {
                std.debug.print("\n--- {s}: {d} bytes, not whole ---\n", .{ ent.name, got.len });
                return error.TornFile;
            }
            whole += 1;
        }
    }
    try testing.expect(present >= kill_after_files);
    try testing.expect(whole > 0);
    std.debug.print("smoke: after kill -9: replayed {d} journal entries; {d} files present ({d} whole, {d} empty — created, write not yet issued), {d} never created\n", .{ replayed, present, whole, empty, kill_tree_files - present });
    // The gauges on the recovered mount, then a clean cycle: the next
    // remount's window is empty and nothing torn.
    {
        const m = try rig.logLen();
        _ = linux.kill(rig.child_pid, .USR1);
        _ = try rig.awaitLog("stats daemon: ", m, 10 * std.time.ns_per_s);
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[m..], "\"open_count_stranded\":0,") != null);
        try testing.expect(std.mem.indexOf(u8, text[m..], "\"transport_requests_abandoned\":0,") != null);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    const mark3 = try rig.logLen();
    try rig.spawnMount(&.{});
    const ready3 = try rig.awaitReady(mark3);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[mark3..ready3], ", clean unmount") != null);
        try testing.expect(std.mem.indexOf(u8, text[mark3..ready3], "0 torn dropped") != null);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
}

test "smoke: --daemon --supervise --log-file: the invoking process returns 0 only after ready, the log file is 0600 and carries the ready line, umount ends the daemon and the supervisor; a refused daemon mount reports its reason to the invoker" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    {
        const f = try Io.Dir.cwd().createFile(io, rig.img, .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, volume_len);
    }
    // A daemon mount of an unformatted volume: the invoker gets the reason.
    {
        const r = try rig.squeezefs(&.{ "mount", "--daemon", "--log-file", rig.log_path, rig.meta_uri, rig.mp });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "not formatted") != null);
        try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    }
    {
        const r = try rig.squeezefs(&.{ "format", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
    }
    const r = try rig.squeezefs(&.{ "mount", "--daemon", "--supervise", "--log-file", rig.log_path, rig.meta_uri, rig.mp });
    defer rig.freeRun(r);
    if (!r.term.success()) std.debug.print("\ndaemon mount: {s}\n", .{r.stderr});
    try testing.expect(r.term.success());
    try testing.expectEqual(@as(usize, 1), try fuseMountsOf(gpa, io, rig.mp));
    // The log file: 0600, the ready line with the daemon's pid.
    var st: linux.Statx = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, rig.log_path.ptr, 0, linux.STATX.BASIC_STATS, &st)));
    try testing.expectEqual(@as(u16, 0o600), st.mode & 0o777);
    const text = try rig.readLog();
    defer gpa.free(text);
    const ready_i = std.mem.indexOf(u8, text, "ready: ") orelse return error.NoReadyLine;
    const pid = numberAfter(text[ready_i..], "(pid ") orelse return error.NoPid;
    try testing.expect(std.mem.indexOf(u8, text, "supervisor: watching daemon ") != null);
    // The mount works through the shell.
    {
        const out = try rig.shOk(rig.mp, "echo daemonized > f && test \"$(cat f)\" = daemonized && echo ok");
        defer gpa.free(out);
    }
    // umount: the daemon exits, the supervisor notices and exits too.
    try rig.umount();
    {
        const deadline = clock.monotonicNs() + exit_wait_ns;
        while (alive(@intCast(pid)) and clock.monotonicNs() < deadline) sleepMs(20);
        try testing.expect(!alive(@intCast(pid)));
    }
    _ = try rig.awaitLog("supervisor: daemon ", ready_i, 20 * std.time.ns_per_s);
    const after = try rig.readLog();
    defer gpa.free(after);
    try testing.expect(std.mem.indexOf(u8, after, "exited (wait status 0x0); done") != null);
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    // Nothing of ours survives: no squeezefs process holds the volume.
    try testing.expect(!@import("status.zig").writerGuardHeld(rig.img));
}

/// A data volume of three 4 MiB(+4 KiB) chunks: two reserved, one usable.
const data_volume_len: u64 = 3 * ((4 << 20) + 4096) + 4096;

test "smoke: a SET format — meta + 2 data files + lz4 + aes-256-gcm + a cache path → both data superblocks written and read back → status lists both tags → a re-format refuses on the meta AND on a data volume → a mount with no key refuses naming every source → with the key it mounts, logs the set config and the key id, serves, unmounts → renumbered or blank data devices refuse the mount by name" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    {
        const f = try Io.Dir.cwd().createFile(io, rig.img, .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, volume_len);
    }
    var d0: [128]u8 = undefined;
    var d1: [128]u8 = undefined;
    var key_path: [128]u8 = undefined;
    var cache_path: [128]u8 = undefined;
    const d0p = try std.fmt.bufPrint(&d0, "{s}/d0.img", .{rig.work});
    const d1p = try std.fmt.bufPrint(&d1, "{s}/d1.img", .{rig.work});
    const kp = try std.fmt.bufPrint(&key_path, "{s}/set.key", .{rig.work});
    const cp = try std.fmt.bufPrint(&cache_path, "{s}/cache", .{rig.work});
    for ([_][]const u8{ d0p, d1p }) |dp| {
        const f = try Io.Dir.cwd().createFile(io, dp, .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, data_volume_len);
    }
    // The key file: 0600, ≥ 32 B.
    {
        const out = try rig.shOk(rig.work, "umask 077; head -c 48 /dev/urandom | base64 > set.key; chmod 600 set.key; wc -c < set.key");
        defer gpa.free(out);
    }
    var data_uri: [320]u8 = undefined;
    const du = try std.fmt.bufPrint(&data_uri, "sqdata://{s},{s}", .{ d0p, d1p });
    var cache_flag: [160]u8 = undefined;
    const cf = try std.fmt.bufPrint(&cache_flag, "{s}", .{cp});
    // The set format.
    {
        const r = try rig.squeezefs(&.{ "format", "--compress", "lz4", "--encrypt", "aes-256-gcm", "--encrypt-key", kp, "--disk-cache-paths", cf, rig.meta_uri, du });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "compression lz4; encryption aes-256-gcm; key hkdf-sha256/aead-kw/v2 id ") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "data volumes 2 (VOLUME_LIFECYCLE") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "chunk 4198400 B") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, " 1 blocks (4 MiB)") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "disk cache paths 1:") != null);
        try testing.expectEqual(@as(usize, 2), std.mem.count(u8, r.stdout, "vol-"));
    }
    // Both data files carry the identity at sector 0 and the tail.
    for ([_][]const u8{ d0p, d1p }) |dp| {
        var buf: [8]u8 = undefined;
        const f = try Io.Dir.cwd().openFile(io, dp, .{});
        defer f.close(io);
        try testing.expectEqual(@as(usize, 8), try f.readPositionalAll(io, &buf, 0));
        try testing.expectEqualStrings("SQZDATLV", &buf);
        try testing.expectEqual(@as(usize, 8), try f.readPositionalAll(io, &buf, data_volume_len - 4096));
        try testing.expectEqualStrings("SQZDATLV", &buf);
    }
    // status lists both with their tags.
    {
        const r = try rig.squeezefs(&.{ "status", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "data volumes 2") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, d0p) != null and std.mem.indexOf(u8, r.stdout, d1p) != null);
        // Two volume rows and, since rung 4, their two reference-census rows.
        try testing.expectEqual(@as(usize, 2), std.mem.count(u8, r.stdout, " active "));
        try testing.expectEqual(@as(usize, 2), std.mem.count(u8, r.stdout, "  refs       vol-"));
        try testing.expectEqual(@as(usize, 4), std.mem.count(u8, r.stdout, "vol-"));
    }
    // A re-format refuses: the meta volume first; with the meta forced, a data volume.
    {
        const r = try rig.squeezefs(&.{ "format", rig.meta_uri, du });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "already formatted") != null);
    }
    {
        var data0_uri: [160]u8 = undefined;
        const du0 = try std.fmt.bufPrint(&data0_uri, "sqdata://{s}", .{d0p});
        // A second meta file so only the data gate is in the way.
        var m2: [128]u8 = undefined;
        const m2p = try std.fmt.bufPrint(&m2, "{s}/vol2.img", .{rig.work});
        {
            const f = try Io.Dir.cwd().createFile(io, m2p, .{ .truncate = true });
            defer f.close(io);
            try f.setLength(io, volume_len);
        }
        var m2u: [160]u8 = undefined;
        const m2uri = try std.fmt.bufPrint(&m2u, "sqmeta://{s}", .{m2p});
        const r = try rig.squeezefs(&.{ "format", m2uri, du0 });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "already a SqueezeFS data volume (vol-") != null);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "--force") != null);
        // The refused format wrote nothing: the second meta file is still Blank.
        const st = try rig.squeezefs(&.{ "status", m2uri });
        defer rig.freeRun(st);
        try testing.expect(std.mem.indexOf(u8, st.stdout, "not formatted") != null);
    }
    // A mount with no key source refuses naming all three sources.
    {
        const r = try rig.squeezefs(&.{ "mount", rig.meta_uri, rig.mp });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "no encryption key") != null);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "SQUEEZEFS_ENCRYPT_KEY_FILE") != null);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "/etc/squeezefs/keys/") != null);
        try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    }
    // A wrong key refuses by id.
    {
        const out = try rig.shOk(rig.work, "umask 077; head -c 48 /dev/urandom | base64 > wrong.key; chmod 600 wrong.key");
        defer gpa.free(out);
        var wk: [128]u8 = undefined;
        const wkp = try std.fmt.bufPrint(&wk, "{s}/wrong.key", .{rig.work});
        try rig.env.put("SQUEEZEFS_ENCRYPT_KEY_FILE", wkp);
        const r = try rig.squeezefs(&.{ "mount", rig.meta_uri, rig.mp });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "key id differs") != null);
    }
    // With the key: mount, the set config and the key id in the log, a write, umount.
    try rig.env.put("SQUEEZEFS_ENCRYPT_KEY_FILE", kp);
    try rig.spawnMount(&.{});
    const ready_at = try rig.awaitReady(0);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[0..ready_at], "encryption key verified (hkdf-sha256/aead-kw/v2, id ") != null);
        try testing.expect(std.mem.indexOf(u8, text[0..ready_at], "set config generation 1") != null);
        try testing.expect(std.mem.indexOf(u8, text[0..ready_at], "non-dumpable") != null);
    }
    {
        const out = try rig.shOk(rig.mp, "echo hello > f && test \"$(cat f)\" = hello && echo write-ok");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "write-ok") != null);
    }
    try rig.umount();
    const term = try rig.waitChild();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, term);
    // A renumbered device: swap the two data files; the mount refuses
    // naming the tag it found and the tag the set names.
    {
        const out = try rig.shOk(rig.work, "mv d0.img swap.img && mv d1.img d0.img && mv swap.img d1.img");
        defer gpa.free(out);
        const r = try rig.squeezefs(&.{ "mount", rig.meta_uri, rig.mp });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "the device was renumbered or belongs to another set") != null);
        try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    }
    // A missing data device is not a formatted data volume: refused by name.
    {
        const out = try rig.shOk(rig.work, "mv d1.img d0.img && truncate -s 13M d1.img");
        defer gpa.free(out);
        const r = try rig.squeezefs(&.{ "mount", rig.meta_uri, rig.mp });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "is not a formatted data volume") != null);
    }
    std.debug.print("\nsmoke: set format ok (2 data volumes, lz4 + aes-256-gcm, key gate, renumbered/blank refusals)\n", .{});
}

// ---------------------------------------------------------------------------
// Rung 4: the data plane bound to the mount (charter §6, §13 rung 4).
// ---------------------------------------------------------------------------

/// A data volume: 2 GiB sparse on the venue's ext4 = 510 usable 4 MiB
/// blocks past the superblock chunk; two of them hold the pass's peak
/// (200 MiB `dd` + 100 MiB copy + 8 × 50 MiB storm ≈ 175 blocks) and, on
/// the set WITHOUT a cache path, 100 one-block 64 KiB files.
const data_len: u64 = 2 << 30;
/// The random image on ext4 every stage slices (`image_mib` ≥ 200 for the
/// `dd`, 100 for the copy, 8 × 50 MiB at 25 MiB strides = 225, 1000 × 64 KiB
/// = 62.5).
const image_mib: u64 = 256;
const big_mib: u64 = 200;
const copy_mib: u64 = 100;
const storm_files: usize = 8;
const storm_mib: u64 = 50;
/// The storm's sources are overlapping slices of the image, one per file.
const storm_stride_mib: u64 = 25;
/// The packed candidates on the staging set: 64 KiB (> inline_max, ≪ the
/// 2 MiB max slot) × 1000 = 62.5 MiB.
const small_files_staged: usize = 1000;
/// Without a cache path a 64 KiB file is one striped block: 100 of them
/// = 400 MiB of the set's 4080 MiB.
const small_files_striped: usize = 100;
const small_len: u64 = 64 << 10;
/// The pack census bound (the task's): ceil(1000 × 64 KiB / 2 MiB) = 32
/// blocks, plus one open pack per data volume that the DISMOUNT seal may
/// close short.
const pack_census_bound: u64 = (small_files_staged * small_len + (2 << 20) - 1) / (2 << 20) + 2;
/// 4 KiB overwrites against the mirrored image: page-aligned (the W1
/// patch class on a striped file) and at arbitrary byte offsets (the
/// overlay / RMW classes).
const overwrites_aligned: usize = 64;
const overwrites_unaligned: usize = 16;
/// The overwrite stage's fixed seed (a pass is reproducible from the log).
const overwrite_seed: u64 = 0x5151_4a4a_0404_2026;
/// The looped single-stage repro's knob (`knobs.zig`, harness class).
const smoke_loops_knob = "SQUEEZEFS_TEST_SMOKE_STAGE_LOOPS";
/// One looped pass's wall bound: the overwrite stage runs ~5 s, the storm
/// ~15 s on the venue; a pass past this is a hang, killed and failed.
const stage_loop_timeout_s: u32 = 300;
/// No op of any class may take longer than this on the looped passes (the
/// phantom "5 s stall" was a watchdog artifact; the real tail is ≤ 256 ms).
const op_latency_bound = "<=1s";
/// The kill points of the mid-`dd` matrix: the MiB the inode claims when
/// SIGKILL is sent. The staged phase ends at one block (4 MiB), so every
/// point is deep in the striped write pipeline.
const kill_points_mib = [_]u64{ 50, 100, 150 };
/// The files the strict ack law covers: `fsync` returned before the kill.
const acked_mib: u64 = 8;
/// The storm kill fires once this many bytes are claimed across the eight
/// files: a quarter of the storm in flight.
const storm_kill_mib: u64 = 100;
/// The census grain: a FUSE page. Every page inside a file's claimed size
/// is exact or a hole (the write pipeline publishes whole blocks after
/// their DMA; the size claim lands after the pieces ack; a crash leaves
/// holes, never garbage).
const census_grain: usize = 4096;

/// Stage A: the 200 MiB `dd`, fsync, the sha round trip against the image.
const stage_dd_script =
    \\W="$1"
    \\s=$(date +%s%N)
    \\dd if="$W/image" of=big bs=1M count=200 status=none
    \\sync big
    \\e=$(date +%s%N)
    \\echo "dd_ns=$((e - s))"
    \\test "$(stat -c %s big)" = 209715200
    \\head -c 209715200 "$W/image" > "$W/expect"
    \\sha256sum big | cut -d' ' -f1 > "$W/big.sha"
    \\sha256sum "$W/expect" | cut -d' ' -f1 > "$W/expect.sha"
    \\cmp "$W/big.sha" "$W/expect.sha"
    \\echo stage-dd-ok
;

/// Stage B: `cp` 100 MiB + `cmp`, an O_DIRECT read pass (the daemon's
/// READ path; the page cache bypassed).
const stage_cp_script =
    \\W="$1"
    \\head -c 104857600 "$W/image" > "$W/hundred"
    \\s=$(date +%s%N)
    \\cp "$W/hundred" big100
    \\sync big100
    \\e=$(date +%s%N)
    \\echo "cp_ns=$((e - s))"
    \\cmp big100 "$W/hundred"
    \\s=$(date +%s%N)
    \\dd if=big100 of=/dev/null bs=1M iflag=direct status=none
    \\e=$(date +%s%N)
    \\echo "read_ns=$((e - s))"
    \\dd if=big100 of=/dev/null bs=1M iflag=direct status=none
    \\s=$(date +%s%N)
    \\dd if=big100 of=/dev/null bs=1M iflag=direct status=none
    \\e=$(date +%s%N)
    \\echo "reread_ns=$((e - s))"
    \\echo stage-cp-ok
;

/// Stage C: shrink to 150 MiB, grow to 220 MiB — mirrored on the image.
const stage_truncate_script =
    \\W="$1"
    \\truncate -s 150M big
    \\truncate -s 150M "$W/expect"
    \\test "$(stat -c %s big)" = 157286400
    \\truncate -s 220M big
    \\truncate -s 220M "$W/expect"
    \\test "$(stat -c %s big)" = 230686720
    \\sync big
    \\cmp big "$W/expect"
    \\echo stage-truncate-ok
;

/// Stage F: eight parallel `cp` of 50 MiB each, every file fsync'd, then
/// compared.
const stage_storm_script =
    \\W="$1"
    \\i=0
    \\while [ $i -lt 8 ]; do
    \\  dd if="$W/image" of="$W/s$i" bs=1M skip=$((i * 25)) count=50 status=none
    \\  i=$((i + 1))
    \\done
    \\s=$(date +%s%N)
    \\i=0
    \\while [ $i -lt 8 ]; do
    \\  ( cp "$W/s$i" storm$i && sync storm$i ) &
    \\  i=$((i + 1))
    \\done
    \\wait
    \\e=$(date +%s%N)
    \\echo "storm_ns=$((e - s))"
    \\i=0
    \\while [ $i -lt 8 ]; do
    \\  cmp storm$i "$W/s$i"
    \\  i=$((i + 1))
    \\done
    \\echo stage-storm-ok
;

/// Stage D's script: `n_aligned` page-aligned and `n_unaligned` arbitrary
/// 4 KiB overwrites of `big` (220 MiB after stage C), each mirrored on
/// the image copy with the same source page; deterministic offsets from
/// `seed` (the pass is reproducible from the log; a looped pass takes a
/// distinct seed so a lost write cannot hide behind the previous pass's
/// identical bytes).
fn overwriteScript(gpa: std.mem.Allocator, n_aligned: usize, n_unaligned: usize, seed: u64) ![]u8 {
    var script: std.Io.Writer.Allocating = .init(gpa);
    errdefer script.deinit();
    const w = &script.writer;
    try w.writeAll("W=\"$1\"\n");
    var prng = std.Random.DefaultPrng.init(seed);
    const rnd = prng.random();
    const file_len: u64 = 220 << 20;
    const pages = file_len / 4096;
    for (0..n_aligned + n_unaligned) |i| {
        const src_page = rnd.uintLessThan(u64, (image_mib << 20) / 4096);
        if (i < n_aligned) {
            const dst_page = rnd.uintLessThan(u64, pages);
            for ([_][]const u8{ "big", "\"$W/expect\"" }) |dst| {
                try w.print("dd if=\"$W/image\" of={s} bs=4096 count=1 skip={d} seek={d} conv=notrunc status=none\n", .{ dst, src_page, dst_page });
            }
        } else {
            const dst_off = rnd.uintLessThan(u64, file_len - 4096);
            for ([_][]const u8{ "big", "\"$W/expect\"" }) |dst| {
                try w.print("dd if=\"$W/image\" of={s} bs=4096 count=1 skip={d} seek={d} oflag=seek_bytes conv=notrunc status=none\n", .{ dst, src_page, dst_off });
            }
        }
    }
    try w.writeAll(
        \\sync big
        \\cmp big "$W/expect"
        \\sha256sum big | cut -d' ' -f1
        \\echo stage-overwrite-ok
        \\
    );
    return script.toOwnedSlice();
}

/// Stage E's script: `n` files of 64 KiB, each a distinct slice of the image.
fn smallFilesScript(gpa: std.mem.Allocator, n: usize) ![]u8 {
    return std.fmt.allocPrint(gpa,
        \\W="$1"
        \\mkdir -p p
        \\s=$(date +%s%N)
        \\i=0
        \\while [ $i -lt {d} ]; do
        \\  dd if="$W/image" of=p/f$i bs=65536 count=1 skip=$i status=none
        \\  i=$((i + 1))
        \\done
        \\sync p/f0 p/f{d}
        \\e=$(date +%s%N)
        \\echo "small_ns=$((e - s))"
        \\echo stage-small-ok
        \\
    , .{ n, n - 1 });
}

/// The small files after the remount: their concatenation is the image prefix.
fn smallFilesVerifyScript(gpa: std.mem.Allocator, n: usize) ![]u8 {
    return std.fmt.allocPrint(gpa,
        \\W="$1"
        \\i=0
        \\while [ $i -lt {d} ]; do
        \\  cat p/f$i
        \\  i=$((i + 1))
        \\done | sha256sum | cut -d' ' -f1 > "$W/small.sha"
        \\head -c {d} "$W/image" | sha256sum | cut -d' ' -f1 > "$W/small-expect.sha"
        \\cmp "$W/small.sha" "$W/small-expect.sha"
        \\rm -r p
        \\echo small-verify-ok
        \\
    , .{ n, n * small_len });
}

/// MB/s from a byte count and a `date +%s%N` difference parsed from `key=`.
fn mbPerS(out: []const u8, key: []const u8, bytes: u64) u64 {
    const ns = numberAfter(out, key) orelse return 0;
    if (ns == 0) return 0;
    return bytes * 1000 / ns;
}

/// The must-stay-0 gauges of a data-plane mount, read from one stats tail.
const must_stay_zero = [_][]const u8{
    "\"transport_requests_abandoned\":",
    "\"open_count_stranded\":",
    "\"transport_park_tick_commit_rescues\":",
    "\"transport_park_tick_cqe_rescues\":",
    "\"exec_lane_tick_rescues\":",
    "\"lock_ticked_reregisters\":",
    "\"channel_ticked_reregisters\":",
    "\"fuse_op_watchdog_overdue\":",
    "\"invariant_tripwires\":",
    "\"patch_edge_rmw_reads\":",
    "\"overlay_fence_drops\":",
    "\"data_unresolved_volume\":",
    "\"block_double_frees\":",
    "\"meta_kv_block_refs_drift\":",
    "\"pack_pin_refcount_drift\":",
    "\"staged_payload_lost_reads\":",
    "\"write_shared_class_violation\":",
    "\"read_dest_overruns\":",
    "\"layout_map_merge_failures\":",
    "\"promotions_failed\":",
    "\"held_store_failures\":",
    "\"transport_zc_direct_leg_recheck_withdrawn\":",
    "\"fuse_write_no_data_volume_refused\":",
    "\"fuse_write_u32_index_refused\":",
    "\"kvmap_resolve_failures\":",
    "\"kvmap_owner_mismatch\":",
    "\"kvmap_sweep_spawn_refusals\":",
    "\"writeback_errors_latched\":",
    "\"fsync_promote_failures\":",
};

fn expectMustStayZero(tail: []const u8) !void {
    var bad = false;
    for (must_stay_zero) |k| {
        const v = numberAfter(tail, k) orelse {
            std.debug.print("\n--- gauge missing from the stats dump: {s} ---\n", .{k});
            bad = true;
            continue;
        };
        if (v != 0) {
            std.debug.print("\n--- must-stay-0 gauge {s} = {d} ---\n", .{ k, v });
            bad = true;
        }
    }
    try testing.expect(!bad);
    try testing.expect(std.mem.indexOf(u8, tail, "\"volume_failed\":false") != null);
}

/// The ledgers that close on a quiescent mount: the admission ledger ≡
/// the transport's WRITE payload bytes (every user byte admitted ONCE);
/// the read copy ledger ≡ the read path's user bytes; the reclaim ledger
/// `queued + elided ≡ terminal frees`.
fn expectLedgersClosed(tail: []const u8) !void {
    const admit = (numberAfter(tail, "\"admit_write_overlay_bytes\":") orelse 0) + (numberAfter(tail, "\"admit_write_inplace_bytes\":") orelse 0) + (numberAfter(tail, "\"admit_write_accumulate_bytes\":") orelse 0) + (numberAfter(tail, "\"admit_write_inline_bytes\":") orelse 0);
    // + the truncate's own zero-complete of a kept partial block.
    const transport_write = (numberAfter(tail, "\"transport_zc_write_extract_bytes\":") orelse 0) + (numberAfter(tail, "\"transport_zc_write_direct_bytes\":") orelse 0) + (numberAfter(tail, "\"fuse_truncate_zero_tail_bytes\":") orelse 0);
    if (admit != transport_write) std.debug.print("\n--- admission ledger {d} ≠ transport WRITE payload bytes + zero tails {d} ---\n", .{ admit, transport_write });
    try testing.expectEqual(transport_write, admit);
    const user = numberAfter(tail, "\"read_user_bytes\":") orelse 0;
    const copies = (numberAfter(tail, "\"read_copy_dest_bytes\":") orelse 0) + (numberAfter(tail, "\"read_copy_bounce_bytes\":") orelse 0) + (numberAfter(tail, "\"read_dest_dma_bytes\":") orelse 0) + (numberAfter(tail, "\"read_zc_serve_bytes\":") orelse 0) + (numberAfter(tail, "\"read_zc_pool_serve_bytes\":") orelse 0);
    if (user != copies) std.debug.print("\n--- read copy ledger {d} ≠ read user bytes {d} ---\n", .{ copies, user });
    try testing.expectEqual(user, copies);
    // Every abandoned mint is a superseded write-through or an orphan's
    // discard on a healthy device (a failed DMA would be its own alert).
    const abandons = numberAfter(tail, "\"unpublished_mint_abandons\":") orelse 0;
    const legit = (numberAfter(tail, "\"write_pipeline_supersessions\":") orelse 0) + (numberAfter(tail, "\"write_pipeline_discards\":") orelse 0);
    // (a flush that finds its entry already gone discards BEFORE minting.)
    if (abandons > legit) std.debug.print("\n--- mint abandons {d} > supersessions + discards {d} ---\n", .{ abandons, legit });
    try testing.expect(abandons <= legit);
    const queued = numberAfter(tail, "\"block_free_reclaim_queued\":") orelse 0;
    const elided = numberAfter(tail, "\"block_free_reclaim_elided\":") orelse 0;
    const terminal = numberAfter(tail, "\"terminal_frees\":") orelse 0;
    if (terminal != queued + elided) std.debug.print("\n--- reclaim ledger: terminal {d} ≠ queued {d} + elided {d} ---\n", .{ terminal, queued, elided });
    try testing.expectEqual(queued + elided, terminal);
}

/// The ratios the evidence note records.
fn printRates(label: []const u8, tail: []const u8) void {
    const reqs = numberAfter(tail, "\"transport_ring_requests\":") orelse 0;
    const enters = numberAfter(tail, "\"transport_enters\":") orelse 0;
    const ops = numberAfter(tail, "\"fuse_ops\":") orelse 0;
    const copies = numberAfter(tail, "\"transport_zc_bounce_copies\":") orelse 0;
    const user_r = numberAfter(tail, "\"read_user_bytes\":") orelse 0;
    const dest = numberAfter(tail, "\"read_copy_dest_bytes\":") orelse 0;
    const dma = numberAfter(tail, "\"read_dest_dma_bytes\":") orelse 0;
    const zc = numberAfter(tail, "\"transport_zc_serve_bytes\":") orelse 0;
    const legs = numberAfter(tail, "\"transport_zc_direct_legs\":") orelse 0;
    const fd_src = numberAfter(tail, "\"fast_dispatch_fd_source_serves\":") orelse 0;
    const probe_serves = numberAfter(tail, "\"fast_dispatch_serves\":") orelse 0;
    const hot_hits = numberAfter(tail, "\"hot_block_hits\":") orelse 0;
    const held = numberAfter(tail, "\"held_stores\":") orelse 0;
    const admits = numberAfter(tail, "\"hold_gate_admits\":") orelse 0;
    const patches = numberAfter(tail, "\"patch_writes\":") orelse 0;
    const nt = numberAfter(tail, "\"nt_copy_bytes\":") orelse 0;
    const wt = numberAfter(tail, "\"write_through_bytes\":") orelse 0;
    const own = numberAfter(tail, "\"own_block_dmas\":") orelse 0;
    const merges = numberAfter(tail, "\"layout_map_merges\":") orelse 0;
    const abandons = numberAfter(tail, "\"unpublished_mint_abandons\":") orelse 0;
    const supersessions = numberAfter(tail, "\"write_pipeline_supersessions\":") orelse 0;
    const joins = numberAfter(tail, "\"write_pipeline_joins\":") orelse 0;
    const lock_ticks = numberAfter(tail, "\"lock_ticked_reregisters\":") orelse 0;
    const chan_ticks = numberAfter(tail, "\"channel_ticked_reregisters\":") orelse 0;
    const full_stalls = numberAfter(tail, "\"meta_kv_journal_full_stalls\":") orelse 0;
    const park_cycles = numberAfter(tail, "\"meta_kv_checkpoint_park_cycles\":") orelse 0;
    const cv_ticks = numberAfter(tail, "\"meta_conveyor_park_tick_rescues\":") orelse 0;
    const checkpoints = numberAfter(tail, "\"meta_kv_checkpoints\":") orelse 0;
    const meta_seg = tail[(std.mem.indexOf(u8, tail, "stats meta: ") orelse 0)..];
    const dev_seg = tail[(std.mem.indexOf(u8, tail, "\"nvme_dev\":") orelse 0)..];
    const dev_write_seg = dev_seg[(std.mem.indexOf(u8, dev_seg, "\"write_pipeline_phase_ns\":") orelse 0)..];
    const exec_seg = tail[(std.mem.indexOf(u8, tail, "stats exec: ") orelse 0)..];
    const wp_seg = tail[(std.mem.indexOf(u8, tail, "\"write_pipeline_phase_ns\":") orelse 0)..];
    std.debug.print("smoke[{s}]: waits: overlay teardown waits {d} (ack-early stores {d}, retries {d}, overlong {d}), bridge cancels {d}, direct-store failures {d}, staged identity retries {d}, settle lost {d}, hold ceremonies {d}, prefetch fibers {d}, pipeline admission waits {d}, parked gate waits {d}\n", .{ label, numberAfter(tail, "\"overlay_teardown_waits\":") orelse 0, numberAfter(tail, "\"overlay_ack_early_stores\":") orelse 0, numberAfter(tail, "\"overlay_ack_early_retries\":") orelse 0, numberAfter(tail, "\"overlay_teardown_wait_overlong\":") orelse 0, numberAfter(tail, "\"transport_zc_bridge_cancels\":") orelse 0, numberAfter(tail, "\"transport_zc_write_direct_failures\":") orelse 0, numberAfter(tail, "\"staged_identity_retries\":") orelse 0, numberAfter(tail, "\"read_settle_lost_serialized\":") orelse 0, numberAfter(tail, "\"hold_ceremony_fibers\":") orelse 0, numberAfter(tail, "\"prefetch_fibers\":") orelse 0, numberAfter(tail, "\"write_pipeline_admission_waits\":") orelse 0, numberAfter(tail, "\"parked_gate_waits\":") orelse 0 });
    std.debug.print("smoke[{s}]: write pipeline legs' slowest buckets: admit_wait {s}, detach_lag {s}, lock_wait {s}, dma {s}, publish {s}, total {s}\n", .{ label, histMaxBucket(wp_seg, "\"admit_wait\":"), histMaxBucket(wp_seg, "\"detach_lag\":"), histMaxBucket(wp_seg, "\"lock_wait\":"), histMaxBucket(wp_seg, "\"dma\":"), histMaxBucket(wp_seg, "\"publish\":"), histMaxBucket(wp_seg, "\"total\":") });
    std.debug.print("smoke[{s}]: device legs' slowest buckets: read fill queue {s} / service {s}, write queue {s} / service {s}; read serve total {s}; the longest fiber poll {s}\n", .{ label, histMaxBucket(dev_seg, "\"dev_queue\":"), histMaxBucket(dev_seg, "\"dev_service\":"), histMaxBucket(dev_write_seg, "\"dev_queue\":"), histMaxBucket(dev_write_seg, "\"dev_service\":"), histMaxBucket(tail, "\"read_serve_phase_ns\":{\"total\":"), histMaxBucket(exec_seg, "\"exec_poll_ns\":") });
    std.debug.print("smoke[{s}]: fsync legs' slowest buckets: data_flush {s}, data_barrier {s}, meta_barrier {s}; conveyor tx queue wait {s}, pass {s}, window {s}; the checkpoint's device barrier {s}; staging backing msync {s}, punch+madvise {s}\n", .{ label, histMaxBucket(tail, "\"data_flush\":"), histMaxBucket(tail, "\"data_barrier\":"), histMaxBucket(tail, "\"meta_barrier\":"), histMaxBucket(meta_seg, "\"meta_tx_queue_wait\":"), histMaxBucket(meta_seg, "\"meta_pass_total\":"), histMaxBucket(meta_seg, "\"meta_window_total\":"), histMaxBucket(meta_seg, "\"meta_barrier\":"), histMaxBucket(tail, "\"backing_sync\":"), histMaxBucket(tail, "\"backing_reclaim\":") });
    std.debug.print("smoke[{s}]: {d} fuse ops, {d} ring requests, {d} enters ({d}.{d:0>2}/request), {d} bounce copies; READ user {d} B: dest memcpy {d} B, dest DMA {d} B, zc serve {d} B ({d} direct legs, {d} fd-source serves, {d} probe serves, {d} hot-tier hits); WRITE: {d} held stores / {d} hold admits / {d} W1 patches, NT copy {d} B of {d} B write-through, {d} own-block DMAs, {d} map merges, {d} mint abandons ({d} supersessions, {d} joins); ticked parks lock {d} / channel {d}; journal full stalls {d}, park-kicked checkpoint cycles {d} of {d}, conveyor tick rescues {d}\n", .{ label, ops, reqs, enters, enters / @max(reqs, 1), (enters * 100 / @max(reqs, 1)) % 100, copies, user_r, dest, dma, zc, legs, fd_src, probe_serves, hot_hits, held, admits, patches, nt, wt, own, merges, abandons, supersessions, joins, lock_ticks, chan_ticks, full_stalls, park_cycles, checkpoints, cv_ticks });
}

/// Every op class's slowest latency bucket (`stats ops:`) is at or under
/// `bound` (a bucket label: the histogram's log2 grid); the offending class
/// is printed.
fn expectOpLatencyWithin(tail: []const u8, bound: []const u8) !void {
    const seg = tail[(std.mem.indexOf(u8, tail, "\"fuse_op_latency_ns\":{") orelse return error.OpLatencyMissing)..];
    const bound_rank = bucketRank(bound) orelse return error.UnknownBucket;
    var bad = false;
    inline for (@typeInfo(handler.op_trace.OpClass).@"enum".field_names) |name| {
        const slowest = histMaxBucket(seg, "\"" ++ name ++ "\":");
        if (bucketRank(slowest)) |rank| if (rank > bound_rank) {
            std.debug.print("\n--- op class {s}: slowest bucket {s} past the {s} bound ---\n", .{ name, slowest, bound });
            bad = true;
        };
    }
    try testing.expect(!bad);
}

/// A histogram bucket label's rank (`counters.bucket_labels` order); null
/// for `none`/`absent`.
fn bucketRank(label: []const u8) ?usize {
    for (counters.bucket_labels, 0..) |l, i| if (std.mem.eql(u8, l, label)) return i;
    return null;
}

/// The highest non-empty bucket of a latency histogram in the stats dump
/// (`"<key>":{"buckets":{"<=1us":n,...}`): the stall's order of magnitude.
fn histMaxBucket(text: []const u8, key: []const u8) []const u8 {
    const i = std.mem.indexOf(u8, text, key) orelse return "absent";
    const b = std.mem.indexOfPos(u8, text, i, "\"buckets\":{") orelse return "absent";
    const end = std.mem.indexOfScalarPos(u8, text, b, '}') orelse return "absent";
    var best: []const u8 = "none";
    var it = std.mem.splitScalar(u8, text[b + "\"buckets\":{".len .. end], ',');
    while (it.next()) |pair| {
        const colon = std.mem.indexOfScalar(u8, pair, ':') orelse continue;
        const count = std.fmt.parseInt(u64, pair[colon + 1 ..], 10) catch continue;
        if (count != 0) best = std.mem.trim(u8, pair[0..colon], "\"");
    }
    return best;
}

/// The pack census from `status`: the blocks referenced on both volumes.
const RefCensus = struct { references: u64, blocks: u64 };

fn refCensus(report: []const u8) RefCensus {
    var out: RefCensus = .{ .references = 0, .blocks = 0 };
    var it = std.mem.splitScalar(u8, report, '\n');
    while (it.next()) |line| {
        const i = std.mem.indexOf(u8, line, "  refs       vol-") orelse continue;
        out.references += numberAfter(line[i..], ": ") orelse 0;
        out.blocks += numberAfter(line[i..], "over ") orelse 0;
    }
    return out;
}

/// THE DATA-PLANE PASS on a fresh set (with or without a cache path).
fn dataPlanePass(rig: *Rig, cache: bool) !void {
    const gpa = rig.gpa;
    const io = rig.io;
    const label: []const u8 = if (cache) "cache" else "no-cache";
    try rig.formatSet(cache);
    try rig.createImage();
    try rig.spawnMount(&.{});
    const ready_at = try rig.awaitReady(0);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        const seg = text[0..ready_at];
        try testing.expect(std.mem.indexOf(u8, seg, "data plane: 2 data volume(s)") != null);
        if (cache) {
            try testing.expect(std.mem.indexOf(u8, seg, "staging ON, NVMe tier ON") != null);
            try testing.expect(std.mem.indexOf(u8, seg, "held-slot direct store ON, direct device leg ON, fd-source probe serve ON") != null);
        } else {
            try testing.expect(std.mem.indexOf(u8, seg, "the set declares NO cache paths — staging and the NVMe read tier are OFF") != null);
        }
    }
    const baseline = try rig.usedKib();
    // A: dd 200 MiB → fsync → sha round trip.
    {
        const out = try rig.onMount(stage_dd_script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-dd-ok") != null);
        std.debug.print("\nsmoke[{s}]: dd 200 MiB + fsync: {d} MB/s\n", .{ label, mbPerS(out, "dd_ns=", big_mib << 20) });
    }
    // B: cp 100 MiB + cmp; an O_DIRECT read pass.
    {
        const out = try rig.onMount(stage_cp_script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-cp-ok") != null);
        std.debug.print("smoke[{s}]: cp 100 MiB + fsync: {d} MB/s; O_DIRECT read back: {d} MB/s cold, {d} MB/s on the third pass\n", .{ label, mbPerS(out, "cp_ns=", copy_mib << 20), mbPerS(out, "read_ns=", copy_mib << 20), mbPerS(out, "reread_ns=", copy_mib << 20) });
    }
    // C: truncate shrink / grow against the mirror.
    {
        const out = try rig.onMount(stage_truncate_script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-truncate-ok") != null);
    }
    // D: random 4 KiB overwrites against the mirror.
    {
        const script = try overwriteScript(gpa, overwrites_aligned, overwrites_unaligned, overwrite_seed);
        defer gpa.free(script);
        const out = try rig.onMount(script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-overwrite-ok") != null);
        std.debug.print("smoke[{s}]: truncate 200→150→220 MiB and {d}+{d} 4 KiB overwrites: image-exact, sha {s}\n", .{ label, overwrites_aligned, overwrites_unaligned, std.mem.trim(u8, out[0..@min(out.len, 64)], " \n") });
    }
    // E: the small files (packed candidates on the staging set).
    const small_n: usize = if (cache) small_files_staged else small_files_striped;
    {
        const script = try smallFilesScript(gpa, small_n);
        defer gpa.free(script);
        const out = try rig.onMount(script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-small-ok") != null);
        std.debug.print("smoke[{s}]: {d} × 64 KiB files: {d} MB/s\n", .{ label, small_n, mbPerS(out, "small_ns=", small_n * small_len) });
    }
    // F: the storm.
    {
        const out = try rig.onMount(stage_storm_script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-storm-ok") != null);
        std.debug.print("smoke[{s}]: 8 × 50 MiB cp storm + fsync each: {d} MB/s aggregate\n", .{ label, mbPerS(out, "storm_ns=", storm_files * (storm_mib << 20)) });
    }
    // The gauges and ledgers after the storm.
    {
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        try expectMustStayZero(tail);
        try expectLedgersClosed(tail);
        printRates(label, tail);
        const striped = numberAfter(tail, "\"fuse_write_striped\":") orelse 0;
        const staged = numberAfter(tail, "\"fuse_write_staged\":") orelse 0;
        const promotions = numberAfter(tail, "\"fuse_write_promotions_to_striped\":") orelse 0;
        const clips = numberAfter(tail, "\"fuse_truncate_striped_clips\":") orelse 0;
        try testing.expect(striped > 0 and promotions >= 1 + 1 + storm_files and clips == 1);
        if (cache) {
            try testing.expect(staged >= small_files_staged);
            try testing.expect((numberAfter(tail, "\"held_stores\":") orelse 0) > 0);
            try testing.expect((numberAfter(tail, "\"transport_zc_direct_legs\":") orelse 0) > 0);
        } else {
            try testing.expectEqual(@as(u64, 0), staged);
        }
        try testing.expect((numberAfter(tail, "\"patch_writes\":") orelse 0) > 0);
    }
    // G: rm the big files → df back to the baseline (+ the striped small
    // files on the set without staging).
    {
        const out = try rig.onMount("rm -f big big100 storm0 storm1 storm2 storm3 storm4 storm5 storm6 storm7 && echo rm-ok");
        defer gpa.free(out);
    }
    const small_kib: u64 = if (cache) 0 else small_n * (4 << 10);
    try rig.awaitUsedKib(baseline + small_kib);
    // umount: the dismount promotion; the census through `status`.
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    var census: RefCensus = undefined;
    {
        const report = try rig.statusReport();
        defer gpa.free(report);
        census = refCensus(report);
        try testing.expect(census.references >= small_n);
        if (cache) {
            try testing.expect(census.blocks <= pack_census_bound);
            try testing.expect(census.blocks >= 1);
        } else {
            try testing.expectEqual(@as(u64, small_n), census.blocks);
        }
        std.debug.print("smoke[{s}]: after dismount: {d} references over {d} blocks for {d} × 64 KiB files (bound {d})\n", .{ label, census.references, census.blocks, small_n, if (cache) pack_census_bound else small_n });
    }
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        const seg = text[ready_at..];
        try testing.expect(std.mem.indexOf(u8, seg, "clean unmount") != null);
        if (cache) try testing.expect((numberAfter(seg, "dismount promotion: ") orelse 0) >= small_files_staged);
    }
    // Remount: the small files intact (packed reads), rm → the baseline,
    // the reclaim ledger closed, a clean unmount.
    const mark2 = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark2);
    {
        const script = try smallFilesVerifyScript(gpa, small_n);
        defer gpa.free(script);
        const out = try rig.onMount(script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "small-verify-ok") != null);
    }
    try rig.awaitUsedKib(baseline);
    {
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        try expectMustStayZero(tail);
        try expectLedgersClosed(tail);
        const terminal = numberAfter(tail, "\"terminal_frees\":") orelse 0;
        const destroys = numberAfter(tail, "\"fuse_reclaim_destroys\":") orelse 0;
        try testing.expect(destroys >= small_n and terminal >= census.blocks);
        // Every small file was read once after the remount: a packed
        // tenant is one ranged window read (or the direct device leg).
        // Every small file was read once after the remount: the read
        // path's user bytes plus the zc legs' DMA bytes cover them (a
        // packed tenant's block is filled once and serves its siblings
        // from the hot tier; a striped one is one ranged read).
        const read_user = numberAfter(tail, "\"read_user_bytes\":") orelse 0;
        const zc_serve = numberAfter(tail, "\"transport_zc_serve_bytes\":") orelse 0;
        try testing.expect(read_user + zc_serve >= small_n * small_len);
        const ranged = numberAfter(tail, "\"ranged_reads\":") orelse 0;
        const hot = numberAfter(tail, "\"hot_block_hits\":") orelse 0;
        const fills = numberAfter(tail, "\"read_fill_dma_bytes\":") orelse 0;
        std.debug.print("smoke[{s}]: rm all → df at the baseline ({d} KiB used); {d} destroys, {d} terminal frees; the {d} small files read back: {d} ranged reads, {d} hot-tier hits, {d} B filled\n", .{ label, baseline, destroys, terminal, small_n, ranged, hot, fills });
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    {
        const report = try rig.statusReport();
        defer gpa.free(report);
        const c = refCensus(report);
        try testing.expectEqual(@as(u64, 0), c.blocks);
        std.debug.print("smoke[{s}]: {s}", .{ label, report[std.mem.lastIndexOf(u8, report, "summary:").?..] });
    }
}

test "smoke: the data plane WITH a cache path — dd 200 MiB sha round trip, cp 100 MiB + cmp, truncate, 4 KiB overwrites, 1000 packed 64 KiB files and their census, the 8-way storm with every ledger closed, rm → df baseline" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const rig = try Rig.init(testing.allocator, io);
    defer rig.deinit();
    try dataPlanePass(rig, true);
}

test "smoke: the data plane WITHOUT a cache path — staging and the NVMe tier OFF, every write beyond inline striped; the same pass with 100 one-block files" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const rig = try Rig.init(testing.allocator, io);
    defer rig.deinit();
    try dataPlanePass(rig, false);
}

// The looped single-stage repro (`SQUEEZEFS_TEST_SMOKE_STAGE_LOOPS=N`):
// the cache-path set, `big` laid down and shaped once, then N passes of
// the 4 KiB overwrite stage (distinct source pages each pass, `cmp`
// against the mirror) and N passes of the 8-way storm, every pass under
// a wall bound and followed by the quiescent must-stay-0 census (the
// drift and double-free tripwires through `invariant_tripwires`, the
// ticked channel parks, the watchdog's overdue observations). The
// 2026-10-07 lost overwrite showed in ~1 run of 12 of the single pass.
test "smoke: the cache-path overwrite and storm stages LOOPED — 0 lost overwrites, 0 drift, 0 ticked channel parks, 0 watchdog reports over every pass" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    var env = try testing.environ.createMap(gpa);
    defer env.deinit();
    const raw = env.get(smoke_loops_knob) orelse return error.SkipZigTest;
    const loops = std.fmt.parseInt(u32, std.mem.trim(u8, raw, " \t"), 10) catch return error.SkipZigTest;
    if (loops == 0) return error.SkipZigTest;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(true);
    try rig.createImage();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    {
        const out = try rig.onMountTimed(stage_dd_script, stage_loop_timeout_s);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-dd-ok") != null);
    }
    {
        const out = try rig.onMountTimed(stage_truncate_script, stage_loop_timeout_s);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-truncate-ok") != null);
    }
    var pass: u32 = 0;
    while (pass < loops) : (pass += 1) {
        const script = try overwriteScript(gpa, overwrites_aligned, overwrites_unaligned, overwrite_seed +% pass);
        defer gpa.free(script);
        const t0 = clock.monotonicNs();
        const out = try rig.onMountTimed(script, stage_loop_timeout_s);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-overwrite-ok") != null);
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        try expectMustStayZero(tail);
        try expectOpLatencyWithin(tail, op_latency_bound);
        std.debug.print("smoke[loop]: overwrite pass {d}/{d}: image-exact in {d} ms; drift {d}, untracked frees {d}, channel ticks {d}, watchdog overdue {d}\n", .{ pass + 1, loops, (clock.monotonicNs() - t0) / std.time.ns_per_ms, numberAfter(tail, "\"rewrite_shadow_displaced_drift\":") orelse 0, numberAfter(tail, "\"block_untracked_free_refusals\":") orelse 0, numberAfter(tail, "\"channel_ticked_reregisters\":") orelse 0, numberAfter(tail, "\"fuse_op_watchdog_overdue\":") orelse 0 });
    }
    pass = 0;
    while (pass < loops) : (pass += 1) {
        const t0 = clock.monotonicNs();
        const out = try rig.onMountTimed(stage_storm_script, stage_loop_timeout_s);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-storm-ok") != null);
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        try expectMustStayZero(tail);
        try expectLedgersClosed(tail);
        try expectOpLatencyWithin(tail, op_latency_bound);
        std.debug.print("smoke[loop]: storm pass {d}/{d}: {d} MB/s aggregate in {d} ms; channel ticks {d}, watchdog overdue {d}, joins {d}, close waits {d}\n", .{ pass + 1, loops, mbPerS(out, "storm_ns=", storm_files * (storm_mib << 20)), (clock.monotonicNs() - t0) / std.time.ns_per_ms, numberAfter(tail, "\"channel_ticked_reregisters\":") orelse 0, numberAfter(tail, "\"fuse_op_watchdog_overdue\":") orelse 0, numberAfter(tail, "\"write_pipeline_joins\":") orelse 0, numberAfter(tail, "\"rewrite_shadow_close_waits\":") orelse 0 });
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

// ---- the kill matrix ----

/// The page census of a file against its image slice after a crash.
const Census = struct { size: u64, exact: u64, holes: u64 };

/// Every `census_grain` page inside the claimed size is byte-exact against
/// the image at `src_off + page` or all zeros; anything else is TORN.
fn pageCensus(gpa: std.mem.Allocator, io: Io, dir: Io.Dir, name: []const u8, image: Io.File, src_off: u64, written_cap: u64) !Census {
    const f = try dir.openFile(io, name, .{});
    defer f.close(io);
    const size = (try f.stat(io)).size;
    if (size > written_cap) {
        std.debug.print("\n--- {s}: {d} B claimed, {d} B written ---\n", .{ name, size, written_cap });
        return error.SizeBeyondWritten;
    }
    const piece: usize = 1 << 20;
    const got = try gpa.alloc(u8, piece);
    defer gpa.free(got);
    const want = try gpa.alloc(u8, piece);
    defer gpa.free(want);
    var out: Census = .{ .size = size, .exact = 0, .holes = 0 };
    var off: u64 = 0;
    while (off < size) : (off += piece) {
        const n: usize = @intCast(@min(piece, size - off));
        if (try f.readPositionalAll(io, got[0..n], off) != n) return error.ShortRead;
        if (try image.readPositionalAll(io, want[0..n], src_off + off) != n) return error.ShortRead;
        var p: usize = 0;
        while (p < n) : (p += census_grain) {
            const e = @min(p + census_grain, n);
            if (std.mem.eql(u8, got[p..e], want[p..e])) {
                out.exact += 1;
            } else if (std.mem.allEqual(u8, got[p..e], 0)) {
                out.holes += 1;
            } else {
                std.debug.print("\n--- {s}: page at {d} is neither the image nor a hole ---\n", .{ name, off + p });
                return error.TornFile;
            }
        }
    }
    return out;
}

fn fileSize(path: [:0]const u8) u64 {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, path.ptr, 0, .{ .SIZE = true }, &st)) != .SUCCESS) return 0;
    return st.size;
}

/// What a remount after a kill reports.
const Recovery = struct { replayed: u64, torn: u64, ring_records: u64, ring_torn: u64 };

/// Remount with the C8 oracle armed; the journal replay, the ring
/// recovery, zero drift (C8) and nothing leaked or lost (C2) on both
/// volumes.
fn remountAfterKill(rig: *Rig) !Recovery {
    const mark = try rig.logLen();
    try rig.spawnMount(&.{});
    const ready = try rig.awaitReady(mark);
    const text = try rig.readLog();
    defer rig.gpa.free(text);
    const seg = text[mark..ready];
    try testing.expect(std.mem.indexOf(u8, seg, ", clean unmount") == null);
    if (std.mem.count(u8, seg, " 0 drift row(s)") != 2 or std.mem.count(u8, seg, ": 0 leaked block(s) freed, 0 lost block(s)") != 2) {
        std.debug.print("\n--- the oracles after the kill ---\n{s}\n", .{seg});
        return error.DriftAfterKill;
    }
    const torn_at = std.mem.indexOf(u8, seg, "content records, ") orelse return error.NoReplayLine;
    return .{
        .replayed = numberAfter(seg, "replayed ") orelse 0,
        .torn = numberAfter(seg[torn_at..], "content records, ") orelse 0,
        .ring_records = numberAfter(seg, "recovered ") orelse 0,
        .ring_torn = if (std.mem.indexOf(u8, seg, "record(s) (")) |i| (numberAfter(seg[i..], "), ") orelse 0) else 0,
    };
}

test "smoke: THE KILL MATRIX — kill -9 at 50/100/150 MiB of a 200 MiB dd and mid 8-way cp storm → remount → every fsync'd file byte-exact, every page of an in-flight file exact or a hole, C8 zero drift, C2 nothing leaked or lost, rm → df baseline → status" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(true);
    try rig.createImage();
    // The C8 oracle at every mount of this test (the A/B lever).
    try rig.env.put("SQUEEZEFS_BLOCK_REFS_VERIFY", "1");
    const image = try Io.Dir.cwd().openFile(io, rig.image, .{});
    defer image.close(io);
    const storm_sources = try rig.shOk(rig.work, "i=0; while [ $i -lt 8 ]; do dd if=image of=s$i bs=1M skip=$((i * 25)) count=50 status=none; i=$((i + 1)); done; echo ok");
    gpa.free(storm_sources);

    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const baseline = try rig.usedKib();
    var acked: usize = 0;
    var pb: [256:0]u8 = undefined;
    // Three mid-dd kills.
    for (kill_points_mib, 0..) |point, k| {
        // The acked file: 8 MiB from the image at a distinct offset, fsync returned.
        {
            var script: std.Io.Writer.Allocating = .init(gpa);
            defer script.deinit();
            try script.writer.print("dd if=\"$1/image\" of=acked{d} bs=1M skip={d} count={d} conv=fsync status=none && echo acked-ok", .{ k, 8 * k, acked_mib });
            const out = try rig.onMount(script.written());
            defer gpa.free(out);
            try testing.expect(std.mem.indexOf(u8, out, "acked-ok") != null);
            acked += 1;
        }
        const inflight = try std.fmt.bufPrintSentinel(&pb, "{s}/inflight{d}", .{ rig.mp, k }, 0);
        var dd_script: [512]u8 = undefined;
        const dd_cmd = try std.fmt.bufPrint(&dd_script, "dd if={s} of={s} bs=1M count={d} status=none", .{ rig.image, inflight, big_mib });
        var dd = try std.process.spawn(io, .{ .argv = &.{ "sh", "-c", dd_cmd }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .ignore });
        var claimed: u64 = 0;
        {
            const deadline = clock.monotonicNs() + 60 * std.time.ns_per_s;
            while (clock.monotonicNs() < deadline) {
                claimed = fileSize(inflight);
                if (claimed >= point << 20) break;
                sleepMs(1);
            }
        }
        try testing.expect(claimed >= point << 20);
        _ = linux.kill(rig.child_pid, .KILL);
        try testing.expectEqual(std.process.Child.Term{ .signal = .KILL }, try rig.waitChild());
        try testing.expect(!(try dd.wait(io)).success());
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(rig.mp.ptr, 0)));
        const rec = try remountAfterKill(rig);
        // Every acked file byte-exact; the in-flight file exact or holes.
        var dir = try Io.Dir.cwd().openDir(io, rig.mp, .{});
        defer dir.close(io);
        for (0..acked) |a| {
            var nb: [32]u8 = undefined;
            const name = try std.fmt.bufPrint(&nb, "acked{d}", .{a});
            const c = try pageCensus(gpa, io, dir, name, image, (8 * a) << 20, acked_mib << 20);
            try testing.expectEqual(acked_mib << 20, c.size);
            try testing.expectEqual(@as(u64, 0), c.holes);
        }
        var nb: [32]u8 = undefined;
        const in_name = try std.fmt.bufPrint(&nb, "inflight{d}", .{k});
        // The claim grew between the last poll and the kill: the law is
        // `observed ≤ size ≤ bytes dd could have written`.
        const c = try pageCensus(gpa, io, dir, in_name, image, 0, big_mib << 20);
        try testing.expect(c.size >= claimed);
        std.debug.print("\nsmoke[kill]: dd killed at {d} MiB claimed (point {d} MiB): replayed {d} entries ({d} torn dropped), ring {d} record(s) ({d} torn); inflight{d} claims {d} MiB: {d} pages exact, {d} holes, 0 torn; {d} acked file(s) byte-exact; C8 0 drift, C2 0/0\n", .{ claimed >> 20, point, rec.replayed, rec.torn, rec.ring_records, rec.ring_torn, k, c.size >> 20, c.exact, c.holes, acked });
        {
            const out = try rig.onMount("rm -f inflight* && echo ok");
            defer gpa.free(out);
        }
    }
    // The storm kill.
    {
        var storm_cmd: [1024]u8 = undefined;
        const cmd = try std.fmt.bufPrint(&storm_cmd, "i=0; while [ $i -lt 8 ]; do cp {s}/s$i {s}/storm$i & i=$((i + 1)); done; wait", .{ rig.work, rig.mp });
        var storm = try std.process.spawn(io, .{ .argv = &.{ "sh", "-c", cmd }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .ignore });
        var claimed: u64 = 0;
        {
            const deadline = clock.monotonicNs() + 60 * std.time.ns_per_s;
            while (clock.monotonicNs() < deadline) {
                claimed = 0;
                for (0..storm_files) |i| {
                    const p = try std.fmt.bufPrintSentinel(&pb, "{s}/storm{d}", .{ rig.mp, i }, 0);
                    claimed += fileSize(p);
                }
                if (claimed >= storm_kill_mib << 20) break;
                sleepMs(1);
            }
        }
        try testing.expect(claimed >= storm_kill_mib << 20);
        _ = linux.kill(rig.child_pid, .KILL);
        try testing.expectEqual(std.process.Child.Term{ .signal = .KILL }, try rig.waitChild());
        _ = try storm.wait(io);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(rig.mp.ptr, 0)));
        const rec = try remountAfterKill(rig);
        var dir = try Io.Dir.cwd().openDir(io, rig.mp, .{});
        defer dir.close(io);
        var present: usize = 0;
        var exact: u64 = 0;
        var holes: u64 = 0;
        var sizes: u64 = 0;
        for (0..storm_files) |i| {
            var nb: [32]u8 = undefined;
            const name = try std.fmt.bufPrint(&nb, "storm{d}", .{i});
            const c = pageCensus(gpa, io, dir, name, image, (i * storm_stride_mib) << 20, storm_mib << 20) catch |e| switch (e) {
                error.FileNotFound => continue,
                else => return e,
            };
            present += 1;
            exact += c.exact;
            holes += c.holes;
            sizes += c.size;
        }
        for (0..acked) |a| {
            var nb: [32]u8 = undefined;
            const name = try std.fmt.bufPrint(&nb, "acked{d}", .{a});
            const c = try pageCensus(gpa, io, dir, name, image, (8 * a) << 20, acked_mib << 20);
            try testing.expectEqual(acked_mib << 20, c.size);
            try testing.expectEqual(@as(u64, 0), c.holes);
        }
        std.debug.print("smoke[kill]: storm killed at {d} MiB claimed: replayed {d} entries ({d} torn dropped), ring {d} record(s) ({d} torn); {d} of 8 files present claiming {d} MiB: {d} pages exact, {d} holes, 0 torn; {d} acked file(s) byte-exact; C8 0 drift, C2 0/0\n", .{ claimed >> 20, rec.replayed, rec.torn, rec.ring_records, rec.ring_torn, present, sizes >> 20, exact, holes, acked });
    }
    // rm everything → the baseline; the gauges; a clean cycle; status.
    {
        const out = try rig.onMount("rm -f acked* storm* inflight* && echo ok");
        defer gpa.free(out);
    }
    try rig.awaitUsedKib(baseline);
    {
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        try expectMustStayZero(tail);
        try expectLedgersClosed(tail);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    {
        const report = try rig.statusReport();
        defer gpa.free(report);
        try testing.expectEqual(@as(u64, 0), refCensus(report).blocks);
        try testing.expect(std.mem.indexOf(u8, report, "not mounted") != null);
        std.debug.print("smoke[kill]: {s}", .{report[std.mem.lastIndexOf(u8, report, "summary:").?..]});
    }
}

// ---- the block-map tree ----

/// The tree-mapped file: 1 MiB blocks (`--block-size 1048576`) put the
/// inline map's cap (1,364 entries at the 256 KiB node) at 1,364 MiB, so a
/// 1.5 GiB file crosses with ≤ 2 GiB of real disk; the image is written
/// six times over (the expected byte at `x` is `image[x mod 256 MiB]`).
const kvmap_block: u64 = 1 << 20;
const kvmap_file_mib: u64 = 6 * image_mib;
const kvmap_inline_cap_mib: u64 = 1364;
/// The shrink: 100 MiB keeps 100 blocks; 1,436 past the cut are swept.
const kvmap_keep_mib: u64 = 100;
/// Random 4 KiB overwrites on the tree-mapped file (the W1 patch class).
const kvmap_overwrites: usize = 24;

/// `dd` the image `reps` times, appending (1 MiB writes, every size claim
/// block-aligned); `|| true`: the crash arm's dd dies with the daemon.
fn kvmapWriteScript(gpa: std.mem.Allocator, from_mib: u64, reps: u64, tolerate: bool) ![]u8 {
    var script: std.Io.Writer.Allocating = .init(gpa);
    errdefer script.deinit();
    const w = &script.writer;
    try w.writeAll("W=\"$1\"\n");
    var pos = from_mib;
    const end = reps * image_mib;
    while (pos < end) {
        const n = @min(image_mib - pos % image_mib, end - pos);
        try w.print("dd if=\"$W/image\" of=big bs=1M skip={d} seek={d} count={d} conv=notrunc status=none{s}\n", .{ pos % image_mib, pos, n, if (tolerate) " || true" else "" });
        pos += n;
    }
    if (!tolerate) try w.writeAll("sync big\necho kvmap-write-ok\n");
    return script.toOwnedSlice();
}

/// `big` is the image repeated: its sha equals the concatenation's (a
/// stream, never a second 1.5 GiB on disk).
const kvmap_sha_script =
    \\W="$1"
    \\test "$(stat -c %s big)" = 1610612736
    \\sha256sum big | cut -d' ' -f1 > "$W/big.sha"
    \\cat "$W/image" "$W/image" "$W/image" "$W/image" "$W/image" "$W/image" | sha256sum | cut -d' ' -f1 > "$W/expect.sha"
    \\cmp "$W/big.sha" "$W/expect.sha"
    \\echo kvmap-sha-ok
;

/// `n` page-aligned 4 KiB overwrites at random offsets of the 1.5 GiB
/// tree-mapped file from random image pages, each read back and compared.
fn kvmapOverwriteScript(gpa: std.mem.Allocator, n: usize, seed: u64) ![]u8 {
    var script: std.Io.Writer.Allocating = .init(gpa);
    errdefer script.deinit();
    const w = &script.writer;
    try w.writeAll("W=\"$1\"\n");
    var prng = std.Random.DefaultPrng.init(seed);
    const rnd = prng.random();
    for (0..n) |_| {
        const src_page = rnd.uintLessThan(u64, (image_mib << 20) / 4096);
        const dst_page = rnd.uintLessThan(u64, (kvmap_file_mib << 20) / 4096);
        try w.print("dd if=\"$W/image\" of=big bs=4096 count=1 skip={d} seek={d} conv=notrunc status=none\n", .{ src_page, dst_page });
        try w.print("dd if=big of=\"$W/got\" bs=4096 count=1 skip={d} status=none\n", .{dst_page});
        try w.print("dd if=\"$W/image\" of=\"$W/want\" bs=4096 count=1 skip={d} status=none\n", .{src_page});
        try w.writeAll("cmp \"$W/got\" \"$W/want\"\n");
    }
    try w.writeAll("sync big\necho kvmap-overwrite-ok\n");
    return script.toOwnedSlice();
}

/// After the shrink: the kept 100 MiB is exactly what the file served there
/// before the cut (the overwrite stage already touched that span).
const kvmap_truncate_script =
    \\W="$1"
    \\head -c 104857600 big | sha256sum | cut -d' ' -f1 > "$W/expect.sha"
    \\truncate -s 100M big
    \\test "$(stat -c %s big)" = 104857600
    \\sync big
    \\sha256sum big | cut -d' ' -f1 > "$W/big.sha"
    \\cmp "$W/big.sha" "$W/expect.sha"
    \\echo kvmap-truncate-ok
;

/// The crash census: every 1 MiB block inside the claimed size is the
/// image's block at `off mod 256 MiB` or a hole; anything else is TORN.
fn periodicCensus(gpa: std.mem.Allocator, io: Io, dir: Io.Dir, name: []const u8, image: Io.File) !Census {
    const f = try dir.openFile(io, name, .{});
    defer f.close(io);
    const size = (try f.stat(io)).size;
    if (size > kvmap_file_mib << 20) {
        std.debug.print("\n--- {s}: {d} B claimed after the crash, past the write ---\n", .{ name, size });
        return error.SizeBeyondWritten;
    }
    const piece: usize = @intCast(kvmap_block);
    const got = try gpa.alloc(u8, piece);
    defer gpa.free(got);
    const want = try gpa.alloc(u8, piece);
    defer gpa.free(want);
    var out: Census = .{ .size = size, .exact = 0, .holes = 0 };
    var off: u64 = 0;
    while (off < size) : (off += piece) {
        const n: usize = @intCast(@min(piece, size - off));
        if (try f.readPositionalAll(io, got[0..n], off) != n) return error.ShortRead;
        if (try image.readPositionalAll(io, want[0..n], off % (image_mib << 20)) != n) return error.ShortRead;
        if (std.mem.eql(u8, got[0..n], want[0..n])) {
            out.exact += 1;
        } else if (std.mem.allEqual(u8, got[0..n], 0)) {
            out.holes += 1;
        } else {
            std.debug.print("\n--- {s}: block at {d} is neither the image nor a hole ---\n", .{ name, off });
            return error.TornFile;
        }
    }
    return out;
}

test "smoke: THE BLOCK-MAP TREE — a 1.5 GiB file at 1 MiB blocks crosses the inline cap: kill -9 mid-crossing (the test seam) → remount → records staged, head not flipped, C11 reports the residue → the rewrite re-crosses (the ratchet resumes, residue reconciled) → sha round trip across a remount → 4 KiB overwrites → truncate to 100 MiB (the sweep frees every block past the cut) → rm (the corpse sweep) → df baseline → C8/C11 clean" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    var bs_buf: [16]u8 = undefined;
    try rig.formatSetWith(false, &.{ "--block-size", try std.fmt.bufPrint(&bs_buf, "{d}", .{kvmap_block}) });
    try rig.createImage();
    try rig.env.put("SQUEEZEFS_BLOCK_REFS_VERIFY", "1");
    const image = try Io.Dir.cwd().openFile(io, rig.image, .{});
    defer image.close(io);

    // 1. The crash arm: the daemon kills itself after the first committed
    //    record chunk of the crossing, before the flip.
    try rig.env.put("SQUEEZEFS_TEST_KVMAP_CROSSING_KILL_AFTER", "1");
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const baseline = try rig.usedKib();
    {
        // Tolerant: the daemon dies under this script (its dd's fail ENOTCONN).
        const script = try kvmapWriteScript(gpa, 0, 6, true);
        defer gpa.free(script);
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "sh", "-c", script, "smoke", rig.work }, .cwd = .{ .path = rig.mp }, .environ_map = &rig.env });
        defer rig.freeRun(r);
    }
    try testing.expectEqual(std.process.Child.Term{ .signal = .KILL }, try rig.waitChild());
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "BLOCK_MAP_TREE stamped") != null);
        try testing.expect(std.mem.indexOf(u8, text, "TEST SEAM SQUEEZEFS_TEST_KVMAP_CROSSING_KILL_AFTER") != null);
    }
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(rig.mp.ptr, 0)));
    try rig.env.put("SQUEEZEFS_TEST_KVMAP_CROSSING_KILL_AFTER", "0");

    // 2. Remount: the head is still the inline map (every durable block
    //    exact or a hole), the staged records are C11 residue (report-only).
    var mark = try rig.logLen();
    try rig.spawnMount(&.{});
    var ready = try rig.awaitReady(mark);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        const seg = text[mark..ready];
        try testing.expect(std.mem.count(u8, seg, " 0 drift row(s)") == 2);
        const c11 = seg[(std.mem.indexOf(u8, seg, "C11 at mount: ") orelse return error.NoC11Line)..];
        try testing.expectEqual(@as(u64, 0), numberAfter(c11, "C11 at mount: ") orelse 1);
        try testing.expect((numberAfter(c11, "with records, ") orelse 0) >= 1);
        std.debug.print("\nsmoke[kvmap]: after the mid-crossing kill: {s}\n", .{c11[0..@min(c11.len, std.mem.indexOfScalar(u8, c11, '\n') orelse c11.len)]});
    }
    {
        var dir = try Io.Dir.cwd().openDir(io, rig.mp, .{});
        defer dir.close(io);
        const c = try periodicCensus(gpa, io, dir, "big", image);
        try testing.expect(c.size >= kvmap_inline_cap_mib << 20);
        std.debug.print("smoke[kvmap]: big claims {d} MiB after the kill: {d} blocks exact, {d} holes, 0 torn\n", .{ c.size >> 20, c.exact, c.holes });
    }
    // 3. The rewrite from 0: the inline map refills, the crossing re-runs
    //    over the residue and flips; the sha round-trips.
    {
        const script = try kvmapWriteScript(gpa, 0, 6, false);
        defer gpa.free(script);
        const t0 = clock.monotonicNs();
        const out = try rig.onMount(script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "kvmap-write-ok") != null);
        std.debug.print("smoke[kvmap]: 1.5 GiB rewrite + fsync: {d} MB/s\n", .{(kvmap_file_mib << 20) * 1000 / @max(clock.monotonicNs() - t0, 1)});
    }
    {
        const out = try rig.onMount(kvmap_sha_script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "kvmap-sha-ok") != null);
    }
    {
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        try expectMustStayZero(tail);
        try testing.expectEqual(@as(u64, 1), numberAfter(tail, "\"kvmap_crossings\":") orelse 0);
        try testing.expectEqual(@as(u64, 1), numberAfter(tail, "\"map_migrate_resumed\":") orelse 0);
        try testing.expect((numberAfter(tail, "\"kvmap_window_saves\":") orelse 0) > 0);
        try testing.expect((numberAfter(tail, "\"meta_kv_block_map_run_puts\":") orelse 0) > 0);
        std.debug.print("smoke[kvmap]: crossing {d} (resumed {d}), {d} window saves, {d} record puts ({d} runs), {d} deletes, range records {d}, window fills {d} / hits {d}\n", .{ numberAfter(tail, "\"kvmap_crossings\":") orelse 0, numberAfter(tail, "\"map_migrate_resumed\":") orelse 0, numberAfter(tail, "\"kvmap_window_saves\":") orelse 0, numberAfter(tail, "\"meta_kv_block_map_puts\":") orelse 0, numberAfter(tail, "\"meta_kv_block_map_run_puts\":") orelse 0, numberAfter(tail, "\"meta_kv_block_map_deletes\":") orelse 0, numberAfter(tail, "\"meta_kv_block_map_range_records\":") orelse 0, numberAfter(tail, "\"block_map_window_fills\":") orelse 0, numberAfter(tail, "\"block_map_window_hits\":") orelse 0 });
    }
    // 4. Remount cold: the tree-mapped file reads back exact; C11 clean.
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    mark = try rig.logLen();
    try rig.spawnMount(&.{});
    ready = try rig.awaitReady(mark);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        const seg = text[mark..ready];
        try testing.expect(std.mem.count(u8, seg, " 0 drift row(s)") == 2);
        const c11 = seg[(std.mem.indexOf(u8, seg, "C11 at mount: ") orelse return error.NoC11Line)..];
        try testing.expectEqual(@as(u64, 1), numberAfter(c11, "C11 at mount: ") orelse 0);
        try testing.expectEqual(@as(u64, 0), numberAfter(c11, "with records, ") orelse 1);
        try testing.expect(std.mem.indexOf(u8, c11, ", 0 empty head(s), 0 run-foreign shadow(s)") != null);
    }
    {
        const t0 = clock.monotonicNs();
        const out = try rig.onMount(kvmap_sha_script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "kvmap-sha-ok") != null);
        std.debug.print("smoke[kvmap]: cold read of the tree-mapped 1.5 GiB: {d} MB/s\n", .{(kvmap_file_mib << 20) * 1000 / @max(clock.monotonicNs() - t0, 1)});
    }
    // 5. 4 KiB overwrites on the tree-mapped file.
    {
        const script = try kvmapOverwriteScript(gpa, kvmap_overwrites, overwrite_seed);
        defer gpa.free(script);
        const out = try rig.onMount(script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "kvmap-overwrite-ok") != null);
    }
    // 6. The shrink: size-flip-first, the sweep frees every block past the cut.
    var terminal_before: u64 = 0;
    {
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        terminal_before = numberAfter(tail, "\"terminal_frees\":") orelse 0;
    }
    {
        const out = try rig.onMount(kvmap_truncate_script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "kvmap-truncate-ok") != null);
    }
    {
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        try expectMustStayZero(tail);
        try expectLedgersClosed(tail);
        const freed = (numberAfter(tail, "\"terminal_frees\":") orelse 0) - terminal_before;
        try testing.expect(freed >= kvmap_file_mib - kvmap_keep_mib);
        try testing.expect((numberAfter(tail, "\"fuse_kvmap_sweeps_kicked\":") orelse 0) >= 1);
        try testing.expect((numberAfter(tail, "\"map_sweep_chunks\":") orelse 0) >= 1);
        try testing.expect((numberAfter(tail, "\"map_sweep_records\":") orelse 0) >= 1);
        std.debug.print("smoke[kvmap]: truncate 1536 → 100 MiB: {d} terminal frees (≥ {d} blocks past the cut), {d} sweep chunk tx(s), {d} record ops\n", .{ freed, kvmap_file_mib - kvmap_keep_mib, numberAfter(tail, "\"map_sweep_chunks\":") orelse 0, numberAfter(tail, "\"map_sweep_records\":") orelse 0 });
    }
    try rig.awaitUsedKib(baseline + (kvmap_keep_mib << 10));
    // 7. rm: the corpse sweep destroys the inode; df returns to the baseline.
    {
        const out = try rig.onMount("rm -f big && echo rm-ok");
        defer gpa.free(out);
    }
    try rig.awaitUsedKib(baseline);
    {
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        try expectMustStayZero(tail);
        try expectLedgersClosed(tail);
        try testing.expectEqual(@as(u64, 1), numberAfter(tail, "\"kvmap_corpses_destroyed\":") orelse 0);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    {
        const report = try rig.statusReport();
        defer gpa.free(report);
        try testing.expectEqual(@as(u64, 0), refCensus(report).blocks);
        std.debug.print("smoke[kvmap]: {s}", .{report[std.mem.lastIndexOf(u8, report, "summary:").?..]});
    }
}
