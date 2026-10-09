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
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const json_scan = @import("../core/json_scan.zig");
const handler = @import("../fuse/handler.zig");
const status_verb = @import("status.zig");
const uring_fs = @import("../io/uring_fs.zig");
const superblock = @import("../meta/superblock.zig");
const ledger = @import("../meta/ledger.zig");
const device_loader = @import("../meta/device_loader.zig");
const node_seq = @import("../meta/node_seq.zig");
const proc = @import("../meta/guard/proc.zig");
const cr = @import("../meta/sym/control_records.zig");
const appender_page = @import("../meta/sym/appender_page.zig");
const membership = @import("../meta/sym/membership.zig");
const shard = membership.shard;
const join_ladder = membership.join_ladder;
const wire_verbs = @import("../wire/verbs.zig");
const wire_auth = @import("../wire/auth.zig");
const wire_endpoint = @import("../wire/endpoint.zig");
const dialer = @import("../wire/dialer.zig");

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
    /// The test sent `kill -9` to the current child (the kill rows): a
    /// SIGKILL death is then OURS; any other SIGKILL is a stray runner's.
    killed: bool = false,
    env: std.process.Environ.Map,

    fn init(gpa: std.mem.Allocator, io: Io) !*Rig {
        const self = try gpa.create(Rig);
        errdefer gpa.destroy(self);
        const pid = linux.getpid();
        _ = linux.mkdir("/build/tmp", 0o755);
        // The byte-heavy artifacts (data volumes, the cache dir) may live on a
        // capacity-class disk; everything that needs the guest's real block
        // semantics (the metadata volume, mountpoints, logs) stays on /build.
        var proc_env = try std.testing.environ.createMap(gpa);
        defer proc_env.deinit();
        const data_dir: []const u8 = try gpa.dupe(u8, knobs.present(proc_env.get(data_dir_knob)) orelse "/build/tmp");
        defer gpa.free(data_dir);
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
            .d0 = try std.fmt.allocPrintSentinel(gpa, "{s}/smoke-{d}/d0.img", .{ data_dir, pid }, 0),
            .d1 = try std.fmt.allocPrintSentinel(gpa, "{s}/smoke-{d}/d1.img", .{ data_dir, pid }, 0),
            .cache = try std.fmt.allocPrintSentinel(gpa, "{s}/smoke-{d}/cache", .{ data_dir, pid }, 0),
            .data_uri = try std.fmt.allocPrint(gpa, "sqdata://{s}/smoke-{d}/d0.img,{s}/smoke-{d}/d1.img", .{ data_dir, pid, data_dir, pid }),
            .image = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/smoke-{d}/image", .{pid}, 0),
            .env = std.process.Environ.Map.init(gpa),
        };
        try self.env.put("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        _ = linux.mkdir(self.work.ptr, 0o755);
        // The data dir's per-run folder (equals `work` unless the seam moved it).
        const data_work = try std.fmt.allocPrintSentinel(gpa, "{s}/smoke-{d}", .{ data_dir, pid }, 0);
        defer gpa.free(data_work);
        _ = linux.mkdir(data_work.ptr, 0o755);
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

    /// The mount's `.stats` by its absolute path (`Peer.stats`'s law).
    fn stats(self: *Rig) ![]u8 {
        var script: [160]u8 = undefined;
        return self.shOk(self.work, try std.fmt.bufPrint(&script, "cat {s}/.stats", .{self.mp}));
    }

    /// `stats` that answers null instead of failing (the poll's read).
    fn statsTry(self: *Rig) !?[]u8 {
        var script: [160]u8 = undefined;
        const r = try self.sh(self.work, try std.fmt.bufPrint(&script, "cat {s}/.stats", .{self.mp}));
        defer self.gpa.free(r.stderr);
        if (r.term.success()) return r.stdout;
        self.gpa.free(r.stdout);
        return null;
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
        self.killed = false;
        try writePidFile(self.io, self.work, "daemon", self.child_pid);
    }

    fn readLog(self: *Rig) ![]u8 {
        return Io.Dir.cwd().readFileAlloc(self.io, self.log_path, self.gpa, .limited(16 << 20));
    }

    /// The daemon the test expects alive IS alive — else its death is
    /// judged: a SIGKILL the test never sent is a stray runner's sweep
    /// (`pkill -9 -x squeezefs` from a concurrent gate), which SKIPS the
    /// test loud rather than failing a contract nobody tested; any other
    /// exit is the daemon's own and fails.
    fn expectAlive(self: *Rig, waiting_for: []const u8) !void {
        const c = self.child orelse return;
        if (alive(c.id.?)) return;
        const term = try self.waitChild();
        return judgeDeath("A", term, self.killed, waiting_for, self.readLog() catch null, self.gpa);
    }

    /// Bounded poll for a substring in the log (counted from `from`).
    fn awaitLog(self: *Rig, needle: []const u8, from: usize, deadline_ns: u64) !usize {
        const deadline = clock.monotonicNs() + deadline_ns;
        while (true) {
            const text = try self.readLog();
            defer self.gpa.free(text);
            if (text.len > from) if (std.mem.indexOf(u8, text[from..], needle)) |i| return from + i + needle.len;
            try self.expectAlive(needle);
            if (clock.monotonicNs() >= deadline) {
                std.debug.print("\n--- timed out waiting for '{s}' ---\n{s}\n", .{ needle, text });
                return error.Timeout;
            }
            sleepMs(20);
        }
    }

    /// `kill -9` of the rig's daemon (the kill rows): the term is OURS, the
    /// dead connection detached (a type-fuse mount gets no DESTROY).
    fn kill(self: *Rig) !void {
        self.killed = true;
        _ = linux.kill(self.child_pid, .KILL);
        try testing.expectEqual(std.process.Child.Term{ .signal = .KILL }, try self.waitChild());
        _ = linux.umount2(self.mp.ptr, linux.MNT.DETACH);
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

/// The per-test pid file beside the daemon's log (`<work>/<name>.pid`):
/// the tag that tells THIS test's daemons from another runner's on a
/// shared venue (a `pkill -9 -x squeezefs` sweep is read against it).
fn writePidFile(io: Io, work: []const u8, name: []const u8, pid: linux.pid_t) !void {
    var path_buf: [192]u8 = undefined;
    const path = try std.fmt.bufPrint(&path_buf, "{s}/{s}.pid", .{ work, name });
    var text: [16]u8 = undefined;
    const f = try Io.Dir.cwd().createFile(io, path, .{ .truncate = true });
    defer f.close(io);
    try f.writeStreamingAll(io, try std.fmt.bufPrint(&text, "{d}\n", .{pid}));
}

/// A daemon the test expected alive died: a SIGKILL the test never sent
/// is a STRAY runner's sweep (a concurrent gate's `pkill -9 -x squeezefs`
/// or `umount -l /build/tmp/mnt-*`) — the test SKIPS loud, naming it,
/// rather than failing a contract the venue never exercised; any other
/// term is the daemon's own death and fails. `log_text` (owned) is
/// printed and freed.
fn judgeDeath(name: []const u8, term: std.process.Child.Term, killed_by_us: bool, waiting_for: []const u8, log_text: ?[]u8, gpa: std.mem.Allocator) !void {
    defer if (log_text) |t| gpa.free(t);
    if (term == .signal and term.signal == .KILL and !killed_by_us) {
        std.debug.print("\n--- EXTERNAL KILL: daemon {s} died of a SIGKILL this test never sent while waiting for '{s}' — a concurrent runner swept the venue (`pkill -9 -x squeezefs` / `umount -l`); the row is SKIPPED, not failed ---\n", .{ name, waiting_for });
        return error.SkipZigTest;
    }
    std.debug.print("\n--- daemon {s} exited ({f}) while waiting for '{s}' ---\n{s}\n", .{ name, term, waiting_for, log_text orelse "" });
    return error.DaemonExited;
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
        // THE DEFAULT FLIP's other half (charter §5.5): this stage mounted
        // the `--single-writer` opt-out, so every symmetric family — the
        // process-wide `meta.sym` (appenders, manager, join, slots,
        // tokens, membership, xv), the per-volume `sym` words and the
        // data planes' `sym_planes` — reads 0 / `solo` after a full POSIX
        // pass; the mount's own posture names no wire endpoint.
        const meta_line = tail[(std.mem.indexOf(u8, tail, "stats meta: ") orelse return error.MetaFamilyMissing)..];
        var sym_seen: usize = 0;
        var pos: usize = 0;
        while (std.mem.indexOfPos(u8, meta_line, pos, "\"sym\":")) |at| {
            const obj = objectAfter(meta_line[at..], "\"sym\":") orelse return error.SymFamilyMalformed;
            try expectSoloPosture(if (sym_seen == 0) "meta.sym" else "meta.set.volumes[].sym", obj);
            sym_seen += 1;
            pos = at + "\"sym\":".len;
        }
        try testing.expectEqual(@as(usize, 2), sym_seen);
        const data_line = tail[(std.mem.indexOf(u8, tail, "stats data: ") orelse return error.DataFamilyMissing)..];
        try expectSoloPosture("data.sym_planes", objectAfter(data_line, "\"sym_planes\":") orelse return error.SymPlanesFamilyMissing);
        try testing.expect(std.mem.indexOf(u8, tail, "\"wire_endpoint\":\"\"") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"manager_lease\":\"solo\"") != null);
    }
    // umount → the verb SIGNALS the daemon (charter §4 "Teardown": an
    // external unmount sends no DESTROY) → the daemon's own orderly close
    // runs the detach and exits 0 → remount → intact → status.
    try rig.umount();
    const term1 = try rig.waitChild();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, term1);
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[ready_at..], "stop requested (SIGTERM/SIGINT): unmounting") != null);
        try testing.expect(std.mem.indexOf(u8, text[ready_at..], "the FUSE session ended") == null);
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
        // Cross-member creates are intents now (never a split commit):
        // minted ≡ retired + open, nothing left open.
        const minted = numberAfter(tail, "\"xv_cross_owner_intents_minted\":") orelse 0;
        try testing.expect(minted > 0);
        try testing.expectEqual(minted, (numberAfter(tail, "\"xv_cross_owner_intents_retired\":") orelse 0) + (numberAfter(tail, "\"xv_cross_owner_intents_open\":") orelse 0));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tail, "\"xv_cross_owner_intents_open\":"));
        try testing.expect(std.mem.indexOf(u8, tail, "\"routing_same_epoch_dual_claim\":0") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"volume_failed\":false") != null);
        // The slot leases (charter §5.5): each member's manager leases its
        // rotor (+ the native slot on member 0); every mint went through the
        // plane's decision (the closure `affinity + rotor + gather ≡ mints`);
        // nothing was refused at the door on a solo-writer set; the
        // conflict tripwire rides `invariant_tripwires` above.
        std.debug.print("\nsmoke: two-member set: guest trees {d}, cross-member mints {d}, cross-owner intents {d}, gate parks {d}, slot leases held {d}, rotor mints {d}, affinity mints {d}, gather mints {d}, door refusals {d}\n", .{ numberAfter(tail, "\"routing_guest_trees_minted\":") orelse 0, numberAfter(tail, "\"routing_mint_volume_picks\":") orelse 0, minted, numberAfter(tail, "\"routing_gate_parks\":") orelse 0, numberAfter(tail, "\"slot_leases_held\":") orelse 0, numberAfter(tail, "\"rotor_mints\":") orelse 0, numberAfter(tail, "\"affinity_mints\":") orelse 0, numberAfter(tail, "\"dir_gather_mints\":") orelse 0, numberAfter(tail, "\"slot_door_refusals\":") orelse 0 });
        try testing.expect((numberAfter(tail, "\"slot_leases_held\":") orelse 0) >= 2 * 64 + 1);
        const mints = (numberAfter(tail, "\"rotor_mints\":") orelse 0) + (numberAfter(tail, "\"affinity_mints\":") orelse 0) + (numberAfter(tail, "\"dir_gather_mints\":") orelse 0);
        try testing.expect(mints >= routing_dirs * routing_files_per_dir);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tail, "\"slot_door_refusals\":"));
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

// ---------------------------------------------------------------------------
// The N-daemon posture, the manager half (charter §5.5, §5.7): the daemon
// as the manager of its set on the cluster wire, joined by another
// process over real TCP.
// ---------------------------------------------------------------------------

/// `T_owner` the daemon runs the membership shard at in this stage (the
/// test seam): long enough for a joiner's beats to be idle between
/// (`T_self = T_owner − 2·skew − D_purge` stays positive at the 1,100 ms
/// landing ceiling), short enough that a dropped joiner is evicted within
/// two reclaim cadences.
const smoke_t_owner_ms: u64 = 3000;

/// The offline reader a joiner process uses before it holds any wire: the
/// superblock, the newest ledger record, the control records at that
/// checkpoint, the appender pages (`PageReader`).
const OfflineVolume = struct {
    gpa: std.mem.Allocator,
    io: Io,
    env: *const std.process.Environ.Map,
    pool: *uring_fs.Pool,
    path: []const u8,
    sb: superblock.Superblock,
    rec: ledger.LedgerRecord,
    dev: device_loader.DeviceLoader,

    fn open(gpa: std.mem.Allocator, io: Io, env: *const std.process.Environ.Map, path: []const u8) !OfflineVolume {
        const pool = try uring_fs.Pool.init(gpa, uring_fs.Config.resolve(env, derive.facts()));
        errdefer pool.deinit();
        var reason: [512]u8 = undefined;
        const snap = try status_verb.classify(gpa, env, pool, path, &reason);
        const sb = snap.sb orelse return error.NotFormatted;
        const rec = (try status_verb.newestLedger(gpa, env, pool, path, sb)) orelse return error.NoLedger;
        const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
        return .{ .gpa = gpa, .io = io, .env = env, .pool = pool, .path = path, .sb = sb, .rec = rec, .dev = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = sb.root_ledger.start, .bitmap_base = sb.alloc_bitmap.start } };
    }

    fn deinit(self: *OfflineVolume) void {
        self.rec.deinit(self.gpa);
        self.pool.deinit();
    }

    /// Re-read the newest checkpoint (a record committed since the open).
    fn refresh(self: *OfflineVolume) !void {
        const rec = (try status_verb.newestLedger(self.gpa, self.env, self.pool, self.path, self.sb)) orelse return error.NoLedger;
        self.rec.deinit(self.gpa);
        self.rec = rec;
    }

    fn control(self: *OfflineVolume, k: []const u8) !?[]u8 {
        return status_verb.readControlRecord(self.gpa, self.io, self.env, self.pool, self.path, self.sb, &self.rec, k);
    }

    /// Bounded poll for a control record the daemon commits + checkpoints
    /// (CHECKPOINT_MAX_AGE 1 s; the cadence's own flight).
    fn awaitControl(self: *OfflineVolume, k: []const u8) ![]u8 {
        const deadline = clock.monotonicNs() + 20 * std.time.ns_per_s;
        while (true) {
            try self.refresh();
            if (try self.control(k)) |v| return v;
            if (clock.monotonicNs() >= deadline) return error.ControlRecordNotCheckpointed;
            sleepMs(100);
        }
    }

    fn pages(self: *OfflineVolume) join_ladder.PageReader {
        return .{ .ctx = self, .read = &readPage };
    }

    fn readPage(ctx: *anyopaque, offset: u64, out: *[appender_page.PAGE_LEN]u8) error{Io}!void {
        const self: *OfflineVolume = @ptrCast(@alignCast(ctx));
        const img = self.dev.readRange(self.gpa, offset, appender_page.PAGE_LEN) catch return error.Io;
        defer self.gpa.free(img);
        @memcpy(out, img[0..appender_page.PAGE_LEN]);
    }
};

const SmokePoison = struct {
    fired: std.atomic.Value(u32) = .init(0),
    fn poison(ctx: ?*anyopaque, _: []const u8) void {
        const self: *SmokePoison = @ptrCast(@alignCast(ctx.?));
        _ = self.fired.fetchAdd(1, .monotonic);
    }
    fn sink(self: *SmokePoison) membership.park_reclaim.PoisonSink {
        return .{ .ctx = self, .poison = &poison };
    }
};

/// A member's renewal thread: `T_self` at the stage's `T_owner` is 797 ms
/// (`3000 − 2·skew − D_purge`), the renew cadence 265 ms — the ticks must
/// never wait on the stage's own stats reads (a SIGUSR1 dump + the log
/// read can outlast `T_self`), so they run on their own thread at a tenth
/// of `T_self`. Any verdict but idle/renewed is recorded as bad.
const Beater = struct {
    member: *shard.Member,
    stop: std.atomic.Value(bool) = .init(false),
    bad: std.atomic.Value(u8) = .init(0),
    renewed: std.atomic.Value(u32) = .init(0),

    const TICK_MS: u64 = 80;

    fn run(self: *Beater) void {
        while (!self.stop.load(.acquire)) {
            const t = clock.monotonicNs();
            switch (self.member.tick(t / std.time.ns_per_ms, t)) {
                .idle => {},
                .renewed => _ = self.renewed.fetchAdd(1, .monotonic),
                else => self.bad.store(1, .release),
            }
            sleepMs(TICK_MS);
        }
    }
};

fn smokeJoinerIdentity(n: u32) wire_verbs.WireIdentity {
    return .{ .node_token = 0x5A0E_0000 + n, .writer_id = 0x7000 + n, .mount_slot = 0x5000 + n, .reserved = 0 };
}

test "smoke: THE N-DAEMON POSTURE, the manager half — format (the symmetric default; meta + 2 data, cache path) → mount A: the listener is up, .stats names the pid and the wire endpoint, the durable endpoint:{0} record agrees → a second PROCESS dials A over TCP with the secret read off the device: JoinAppender, M rotor slots, a first touch, SlotBusy on A's native slot, a membership join → `appenders` lists its region Live and `clients` lists the member → a joiner that DIES is evicted at T_owner and its death recorded in the ledger → the live joiner releases, leaves (region Free) → `umount` signals A and A's own close detaches" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.spawnMount(&.{});
    const ready_at = try rig.awaitReady(0);

    // The mount's posture: the pid umount signals, the endpoint a joiner
    // dials, both in the `mount` family; the manager lease held.
    var ep_text_buf: [64]u8 = undefined;
    var ep_text: []const u8 = "";
    {
        const tail = try rig.statsTail();
        defer gpa.free(tail);
        try testing.expectEqual(@as(?u64, @intCast(rig.child_pid)), numberAfter(tail, "\"pid\":"));
        const ep = json_scan.string(tail, "wire_endpoint") orelse return error.NoWireEndpoint;
        try testing.expect(std.mem.startsWith(u8, ep, "127.0.0.1:"));
        @memcpy(ep_text_buf[0..ep.len], ep);
        ep_text = ep_text_buf[0..ep.len];
        try testing.expect(std.mem.indexOf(u8, tail, "\"manager_lease\":\"held\"") != null);
        try testing.expectEqual(@as(?u64, 1), numberAfter(tail, "\"wire_listener_up\":"));
        try testing.expectEqual(@as(?u64, 1), numberAfter(tail, "\"membership_shards\":"));
    }
    // The joiner's device-side discovery: the secret and the manager's
    // endpoint off tree 0 at the newest checkpoint.
    var vol = try OfflineVolume.open(gpa, io, &rig.env, rig.img);
    defer vol.deinit();
    const secret = blk: {
        const bytes = (try vol.awaitControl(wire_auth.ENROLL_KEY));
        defer gpa.free(bytes);
        break :blk try wire_auth.decodeEnroll(bytes);
    };
    const advertised = blk: {
        const bytes = try vol.awaitControl(cr.endpointKey(0).slice());
        defer gpa.free(bytes);
        const e = try cr.EndpointRecordWire.decode(bytes);
        var w: wire_endpoint.EndpointWire = .{ .family = e.family, .reserved = 0, .port = e.port, .addr = e.addr };
        var buf: [64]u8 = undefined;
        var fw: std.Io.Writer = .fixed(&buf);
        try w.format(&fw);
        try testing.expectEqualStrings(ep_text, fw.buffered());
        break :blk w;
    };

    // Joiner B: another process on this host dials A.
    var client_b = dialer.Client.init(gpa, io, advertised, secret, smokeJoinerIdentity(1));
    defer client_b.deinit();
    var link_b: join_ladder.TcpLink = .{ .client = &client_b };
    var wire_b: join_ladder.ManagerWire = .{ .gpa = gpa, .link = link_b.link(), .volume = 0, .identity = smokeJoinerIdentity(1), .pages = vol.pages() };
    var b = join_ladder.Joiner.init(gpa, &wire_b);
    defer b.deinit();
    const ident_b: appender_page.Identity = .{ .node_token = smokeJoinerIdentity(1).node_token, .mount_slot = smokeJoinerIdentity(1).mount_slot, .writer_id = smokeJoinerIdentity(1).writer_id };
    try b.joinRegion(.{ .identity = ident_b, .volume_base = node_seq.nodeSeqBase(vol.sb.uuid), .grain = vol.dev.mode.grain(), .ring0_head = vol.rec.journal_tail_seq });
    try testing.expectEqual(@as(u32, 1), b.appenderId());
    const rotor = try b.acquireRotor(4);
    try testing.expectEqual(@as(usize, 4), rotor);
    try testing.expect((try b.firstTouch(60_000)) == .granted);
    const busy = try b.firstTouch(0);
    try testing.expect(busy == .busy and busy.busy.holder == 0);
    var poison: SmokePoison = .{};
    var gate_b = membership.park_reclaim.ParkGate.arm(io, 0, membership.park_reclaim.tParkMaxMs(46_500, membership.park_reclaim.graceMs(smoke_t_owner_ms)));
    var member_b: shard.Member = .{ .gpa = gpa, .id = shard.writerMemberId(smokeJoinerIdentity(1)), .role = .writer, .link = link_b.link(), .pid = @intCast(linux.getpid()), .boot = proc.localIdentity().boot_id, .endpoint = .none, .pr_key = 0, .mount = "/mnt-b", .gate = &gate_b, .poison = poison.sink(), .carriage = b.carriageSink() };
    const now_ms = clock.monotonicNs() / std.time.ns_per_ms;
    try testing.expectEqual(shard.Member.JoinOutcome.granted, member_b.join(now_ms, null));
    // B's beats start NOW: `T_self` (797 ms) runs from the join's send
    // instant, and the stage's own device reads below outlast it.
    var beater: Beater = .{ .member = &member_b };
    const beater_thread = try std.Thread.spawn(.{}, Beater.run, .{&beater});
    var beater_live = true;
    defer if (beater_live) {
        beater.stop.store(true, .release);
        beater_thread.join();
    };

    // Joiner C: joins the same way, then DIES (its connection dropped
    // without a leave): the shard evicts it at T_owner, the ledger records
    // its death.
    {
        var client_c = dialer.Client.init(gpa, io, advertised, secret, smokeJoinerIdentity(2));
        defer client_c.deinit();
        var link_c: join_ladder.TcpLink = .{ .client = &client_c };
        var wire_c: join_ladder.ManagerWire = .{ .gpa = gpa, .link = link_c.link(), .volume = 0, .identity = smokeJoinerIdentity(2), .pages = vol.pages() };
        var c = join_ladder.Joiner.init(gpa, &wire_c);
        defer c.deinit();
        const ident_c: appender_page.Identity = .{ .node_token = smokeJoinerIdentity(2).node_token, .mount_slot = smokeJoinerIdentity(2).mount_slot, .writer_id = smokeJoinerIdentity(2).writer_id };
        try c.joinRegion(.{ .identity = ident_c, .volume_base = node_seq.nodeSeqBase(vol.sb.uuid), .grain = vol.dev.mode.grain(), .ring0_head = vol.rec.journal_tail_seq });
        try testing.expectEqual(@as(u32, 2), c.appenderId());
        try testing.expectEqual(@as(usize, 2), try c.acquireRotor(2));
        var gate_c = membership.park_reclaim.ParkGate.arm(io, 0, membership.park_reclaim.tParkMaxMs(46_500, membership.park_reclaim.graceMs(smoke_t_owner_ms)));
        var member_c: shard.Member = .{ .gpa = gpa, .id = shard.writerMemberId(smokeJoinerIdentity(2)), .role = .writer, .link = link_c.link(), .pid = 1, .boot = @splat(0), .endpoint = .none, .pr_key = 0, .gate = &gate_c, .poison = poison.sink(), .mount = "/mnt-c", .carriage = c.carriageSink() };
        try testing.expectEqual(shard.Member.JoinOutcome.granted, member_c.join(clock.monotonicNs() / std.time.ns_per_ms, null));
    }
    // `appenders`: regions 0 (A, the manager), 1 (B) and 2 (C) Live;
    // `clients`: A's writer claim live + the two member rows (this
    // process's pid for B — live; pid 1 for C — another incarnation).
    {
        const r = try rig.squeezefs(&.{ "appenders", rig.meta_uri });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nappenders: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "3 appender page(s)") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "mgr") != null);
        std.debug.print("\nsmoke: N-daemon: appenders after two joins:\n{s}", .{r.stdout});
    }
    {
        const deadline = clock.monotonicNs() + 20 * std.time.ns_per_s;
        while (true) {
            const r = try rig.squeezefs(&.{ "clients", rig.meta_uri });
            defer rig.freeRun(r);
            try testing.expect(r.term.success());
            const members = std.mem.count(u8, r.stdout, "member ");
            if (members >= 2) {
                try testing.expect(std.mem.indexOf(u8, r.stdout, "writer ") != null);
                std.debug.print("smoke: N-daemon: clients with two members:\n{s}", .{r.stdout});
                break;
            }
            if (clock.monotonicNs() >= deadline) {
                std.debug.print("\n--- clients never listed the member rows ---\n{s}\n", .{r.stdout});
                return error.MembersNotListed;
            }
            sleepMs(200);
        }
    }
    // B beats across several T_owner (idle between beats, renewed at the
    // cadence, on its own thread); C is evicted at T_owner by the daemon's
    // reclaim cadence, and the ledger's writer records its death on
    // volume 0 — B is never touched.
    {
        const deadline = clock.monotonicNs() + (smoke_t_owner_ms * 4 + 20_000) * std.time.ns_per_ms;
        var evicted = false;
        while (clock.monotonicNs() < deadline) {
            const tail = try rig.statsTail();
            defer gpa.free(tail);
            if ((numberAfter(tail, "\"membership_evictions\":") orelse 0) >= 1) {
                evicted = true;
                try testing.expectEqual(@as(?u64, 1), numberAfter(tail, "\"membership_members\":"));
                break;
            }
            sleepMs(500);
        }
        try testing.expect(evicted);
        const dead = try vol.awaitControl(cr.deadMemberKey(.{ .node_token = smokeJoinerIdentity(2).node_token, .mount_slot = smokeJoinerIdentity(2).mount_slot }).slice());
        defer gpa.free(dead);
        const d = try cr.DeadMemberWire.decode(dead);
        try testing.expect(!d.early);
        try testing.expect(d.epoch != 0);
        try testing.expectEqual(@as(u32, 0), poison.fired.load(.monotonic));
    }
    beater.stop.store(true, .release);
    beater_thread.join();
    beater_live = false;
    try testing.expectEqual(@as(u8, 0), beater.bad.load(.acquire));
    try testing.expect(beater.renewed.load(.monotonic) >= 2);
    // B's clean leave: every lease released with words flooring tree 0's
    // record, the membership leave, the region leave → page 1 Free.
    try member_b.leave();
    for (b.leased.items) |l| {
        const rec_bytes = try vol.awaitControl(cr.slotStateKey(l.slot).slice());
        defer gpa.free(rec_bytes);
        const rec = try cr.SlotStateWire.decode(rec_bytes);
        try testing.expect(!try wire_b.releaseSlot(b.appenderId(), l.slot, l.g, .{ .root = rec.root, .cursor = rec.cursor + 1, .extents = rec.slot_tree_extents, .seq_floor = rec.seq_floor + 1 }, &.{}));
    }
    b.leased.items.len = 0;
    try testing.expect(!(try b.leave()).already);
    {
        const r = try rig.squeezefs(&.{ "appenders", "--json", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        std.debug.print("smoke: N-daemon: appenders after B's leave: {s}\n", .{r.stdout});
        // A Free page attests no identity (the leave clears it; the id and
        // term stay for the next joiner's continuation).
        try testing.expect(std.mem.indexOf(u8, r.stdout, "\"appender_id\":1,\"node\":\"0000000000000000\",\"mount_slot\":0,\"writer_id\":\"0000000000000000\",\"state\":\"free\"") != null);
        // C's region was RECOVERED by A's ledger poll (the §5.9 driver over
        // the real backend): its page is Recovered — or already Free, the
        // poll's next tick having released a region no lease names.
        const c_recovered = std.mem.indexOf(u8, r.stdout, "\"appender_id\":2,\"node\":\"000000005a0e0002\",\"mount_slot\":20482,\"writer_id\":\"0000000000007002\",\"state\":\"recovered\"") != null;
        const c_released = std.mem.indexOf(u8, r.stdout, "\"appender_id\":2,\"node\":\"000000005a0e0002\",\"mount_slot\":20482,\"writer_id\":\"0000000000007002\",\"state\":\"free\"") != null;
        try testing.expect(c_recovered or c_released);
    }
    {
        const tail = try rig.statsTail();
        defer gpa.free(tail);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tail, "\"membership_members\":"));
        try testing.expectEqual(@as(?u64, 1), numberAfter(tail, "\"membership_evictions\":"));
        // The closure `dead_members_acted ≡ recorded × regions held`: one
        // death, one region on the one volume A manages.
        try testing.expectEqual(@as(?u64, 1), numberAfter(tail, "\"dead_members_acted\":"));
        try testing.expectEqual(@as(?u64, 1), numberAfter(tail, "\"appender_recoveries\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tail, "\"manager_verb_refusals\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tail, "\"joined_wire_words_rejected\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tail, "\"slot_lease_conflicts\":"));
        try testing.expect((numberAfter(tail, "\"manager_verbs\":") orelse 0) >= 12);
        try testing.expect((numberAfter(tail, "\"membership_joins\":") orelse 0) >= 2);
        try testing.expect((numberAfter(tail, "\"membership_renewals\":") orelse 0) >= 1);
        try expectMustStayZero(tail);
        std.debug.print("smoke: N-daemon manager half: manager verbs {d}, membership joins {d}, renewals {d}, evictions {d}, wire verbs (joiner side) {d}, listener connections {d}\n", .{ numberAfter(tail, "\"manager_verbs\":") orelse 0, numberAfter(tail, "\"membership_joins\":") orelse 0, numberAfter(tail, "\"membership_renewals\":") orelse 0, numberAfter(tail, "\"membership_evictions\":") orelse 0, join_ladder.stats.joined_wire_verbs.load(), numberAfter(tail, "\"wire_listener_connections\":") orelse 0 });
    }
    // umount signals A; A's orderly close detaches and exits 0.
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[ready_at..], "stop requested (SIGTERM/SIGINT): unmounting") != null);
        try testing.expect(std.mem.indexOf(u8, text, "cluster wire: listening at 127.0.0.1:") != null);
    }
}

fn numberAfter(text: []const u8, key: []const u8) ?u64 {
    const i = std.mem.indexOf(u8, text, key) orelse return null;
    var j = i + key.len;
    while (j < text.len and std.ascii.isDigit(text[j])) j += 1;
    return std.fmt.parseInt(u64, text[i + key.len .. j], 10) catch null;
}

/// A SECOND daemon of the rig's set at its own mount point with its own
/// log (the two-daemon matrix: A is the rig's child, B a `Peer`). With a
/// `node` name the daemon runs in its own UTS namespace under that
/// hostname: ANOTHER NODE to the identity law (its node token derives from
/// the hostname), so `appender clear` judges its page foreign residue and
/// a remount under the same name is the same-node successor.
const Peer = struct {
    rig: *Rig,
    name: []const u8,
    node: ?[]const u8 = null,
    mp: [:0]u8,
    log_path: [:0]u8,
    log_file: ?Io.File = null,
    child: ?std.process.Child = null,
    pid: linux.pid_t = 0,
    /// The test sent `kill -9` to the current child (`Rig.killed`'s law).
    killed: bool = false,

    fn init(rig: *Rig, name: []const u8) !*Peer {
        const gpa = rig.gpa;
        const self = try gpa.create(Peer);
        errdefer gpa.destroy(self);
        self.* = .{
            .rig = rig,
            .name = name,
            .mp = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/mnt-{d}-{s}", .{ linux.getpid(), name }, 0),
            .log_path = try std.fmt.allocPrintSentinel(gpa, "{s}/daemon-{s}.log", .{ rig.work, name }, 0),
        };
        _ = linux.mkdir(self.mp.ptr, 0o755);
        return self;
    }

    fn deinit(self: *Peer) void {
        const gpa = self.rig.gpa;
        if (self.child) |*c| {
            _ = linux.kill(self.pid, .KILL);
            _ = c.wait(self.rig.io) catch {};
        }
        self.printAlerts();
        _ = linux.umount2(self.mp.ptr, linux.MNT.DETACH);
        if (self.log_file) |f| f.close(self.rig.io);
        _ = linux.rmdir(self.mp.ptr);
        gpa.free(self.mp);
        gpa.free(self.log_path);
        gpa.destroy(self);
    }

    fn printAlerts(self: *Peer) void {
        const text = self.readLog() catch return;
        defer self.rig.gpa.free(text);
        var it = std.mem.splitScalar(u8, text, '\n');
        var n: usize = 0;
        while (it.next()) |line| {
            if (std.mem.startsWith(u8, line, "error(") or std.mem.startsWith(u8, line, "warn(") or std.mem.indexOf(u8, line, "TRIPWIRE") != null) {
                if (n == 0) std.debug.print("\n--- daemon {s} alerts ---\n", .{self.name});
                std.debug.print("{s}\n", .{line[0..@min(line.len, 400)]});
                n += 1;
                if (n == 40) break;
            }
        }
    }

    fn spawn(self: *Peer, extra: []const []const u8) !void {
        std.debug.assert(self.child == null);
        const rig = self.rig;
        if (self.log_file == null) self.log_file = try Io.Dir.cwd().createFile(rig.io, self.log_path, .{ .truncate = false });
        var argv: [24][]const u8 = undefined;
        var n: usize = 0;
        if (self.node) |node| {
            // `unshare` and `sh` both exec in place: the child's pid IS the
            // daemon's (the kill and the wait address it directly).
            for ([_][]const u8{ "unshare", "-u", "sh", "-c", "hostname \"$0\" && exec \"$@\"", node }) |a| {
                argv[n] = a;
                n += 1;
            }
        }
        argv[n] = rig.bin;
        n += 1;
        argv[n] = "mount";
        n += 1;
        for (extra) |a| {
            argv[n] = a;
            n += 1;
        }
        argv[n] = rig.meta_uri;
        n += 1;
        argv[n] = self.mp;
        n += 1;
        self.child = try std.process.spawn(rig.io, .{ .argv = argv[0..n], .environ_map = &rig.env, .stdin = .ignore, .stdout = .{ .file = self.log_file.? }, .stderr = .{ .file = self.log_file.? } });
        self.pid = self.child.?.id.?;
        self.killed = false;
        var tag_buf: [32]u8 = undefined;
        try writePidFile(rig.io, rig.work, try std.fmt.bufPrint(&tag_buf, "daemon-{s}", .{self.name}), self.pid);
    }

    fn readLog(self: *Peer) ![]u8 {
        return Io.Dir.cwd().readFileAlloc(self.rig.io, self.log_path, self.rig.gpa, .limited(16 << 20));
    }

    /// `Rig.expectAlive`'s law for the peer.
    fn expectAlive(self: *Peer, waiting_for: []const u8) !void {
        if (self.child == null or alive(self.pid)) return;
        const term = try self.waitExit();
        return judgeDeath(self.name, term, self.killed, waiting_for, self.readLog() catch null, self.rig.gpa);
    }

    fn awaitLog(self: *Peer, needle: []const u8, from: usize, deadline_ns: u64) !usize {
        const deadline = clock.monotonicNs() + deadline_ns;
        while (true) {
            const text = try self.readLog();
            defer self.rig.gpa.free(text);
            if (text.len > from) if (std.mem.indexOf(u8, text[from..], needle)) |i| return from + i + needle.len;
            try self.expectAlive(needle);
            if (clock.monotonicNs() >= deadline) {
                std.debug.print("\n--- daemon {s}: timed out waiting for '{s}' ---\n{s}\n", .{ self.name, needle, text });
                return error.Timeout;
            }
            sleepMs(20);
        }
    }

    fn awaitReady(self: *Peer) !usize {
        return self.awaitLog("ready: ", 0, ready_wait_ns);
    }

    /// The mount's `.stats` (the whole JSON; the SIGUSR1 dump's lines are
    /// capped at the log sink's `LINE_MAX`), read by its ABSOLUTE path: a
    /// `chdir` into the mount is a `getattr` of the root — a foreign
    /// object on a joiner, EAGAIN while its holder is dead — and the stats
    /// of a parked daemon are exactly what the kill rows read.
    fn stats(self: *Peer) ![]u8 {
        var script: [160]u8 = undefined;
        return self.rig.shOk(self.rig.work, try std.fmt.bufPrint(&script, "cat {s}/.stats", .{self.mp}));
    }

    /// `stats` that answers null instead of failing: the root's attributes
    /// are its holder's, and a parked joiner's holder is dead — the path
    /// walk to `.stats` answers EAGAIN until the kernel's cached root
    /// attributes serve it (the poll's read).
    fn statsTry(self: *Peer) !?[]u8 {
        var script: [160]u8 = undefined;
        const r = try self.rig.sh(self.rig.work, try std.fmt.bufPrint(&script, "cat {s}/.stats", .{self.mp}));
        defer self.rig.gpa.free(r.stderr);
        if (r.term.success()) return r.stdout;
        self.rig.gpa.free(r.stdout);
        return null;
    }

    /// A `sh -e` script with the peer's mount as cwd.
    fn onMount(self: *Peer, script: []const u8) ![]u8 {
        const rig = self.rig;
        const r = try std.process.run(rig.gpa, rig.io, .{ .argv = &.{ "sh", "-e", "-c", script, "smoke", rig.work }, .cwd = .{ .path = self.mp }, .environ_map = &rig.env });
        defer rig.gpa.free(r.stderr);
        if (!r.term.success()) {
            std.debug.print("\n--- {s}: mount script failed ({f}) ---\n{s}\n--- stdout ---\n{s}\n--- stderr ---\n{s}\n", .{ self.name, r.term, script, r.stdout, r.stderr });
            rig.gpa.free(r.stdout);
            return error.ScriptFailed;
        }
        return r.stdout;
    }

    fn umount(self: *Peer) !void {
        const r = try self.rig.squeezefs(&.{ "umount", self.mp });
        defer self.rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\numount {s}: {s}\n", .{ self.name, r.stderr });
        try testing.expect(r.term.success());
    }

    fn waitExit(self: *Peer) !std.process.Child.Term {
        var c = self.child.?;
        self.child = null;
        const deadline = clock.monotonicNs() + exit_wait_ns;
        while (alive(self.pid) and clock.monotonicNs() < deadline) sleepMs(20);
        if (alive(self.pid)) {
            std.debug.print("\n--- daemon {s} did not exit within {d} s; killing ---\n", .{ self.name, exit_wait_ns / std.time.ns_per_s });
            _ = linux.kill(self.pid, .KILL);
        }
        return c.wait(self.rig.io);
    }

    /// `kill -9`: the daemon dies mid-flight, its dead connection is
    /// detached (a type-fuse mount gets no DESTROY).
    fn kill(self: *Peer) !void {
        self.killed = true;
        _ = linux.kill(self.pid, .KILL);
        try testing.expectEqual(std.process.Child.Term{ .signal = .KILL }, try self.waitExit());
        _ = linux.umount2(self.mp.ptr, linux.MNT.DETACH);
        try testing.expectEqual(@as(usize, 0), try fuseMountsOf(self.rig.gpa, self.rig.io, self.mp));
    }

    /// SIGUSR1 → every stats family in the LOG; the tail from the mark
    /// (owned): the stats of a daemon whose `.stats` path is behind a
    /// foreign root (a parked joiner).
    fn statsDump(self: *Peer) ![]u8 {
        const mark = try self.logLen();
        _ = linux.kill(self.pid, .USR1);
        _ = try self.awaitLog("stats daemon: ", mark, 10 * std.time.ns_per_s);
        const text = try self.readLog();
        defer self.rig.gpa.free(text);
        return self.rig.gpa.dupe(u8, text[mark..]);
    }

    /// The daemon's log length (a mark for `awaitLog`).
    fn logLen(self: *Peer) !usize {
        const text = try self.readLog();
        defer self.rig.gpa.free(text);
        return text.len;
    }
};

/// The two-daemon matrix's file count per directory: 4 × 500 = the 2,000
/// creates the evidence note's row names.
const matrix_dirs: usize = 4;
const matrix_files_per_dir: usize = 500;
/// The matrix's `dd` on B: 200 MiB over the set's 4 MiB data blocks (the
/// format default the rig's set declares) = 50 blocks, every one a grant.
const matrix_dd_blocks: u64 = 50;
const matrix_block_kib: u64 = 4096;

fn matrixCreateScript(gpa: std.mem.Allocator) ![]u8 {
    var w: std.Io.Writer.Allocating = .init(gpa);
    defer w.deinit();
    try w.writer.print("for d in $(seq 0 {d}); do mkdir b$d; done; for d in $(seq 0 {d}); do for i in $(seq 0 {d}); do printf 'b%s-f%s\\n' $d $i > b$d/f$i; done; done; ls b0 | wc -l", .{ matrix_dirs - 1, matrix_dirs - 1, matrix_files_per_dir - 1 });
    return w.toOwnedSlice();
}

test "smoke: THE TWO-DAEMON MATRIX — mount A (the manager) → mount B as a SECOND PROCESS joined over TCP (no flock, the region over the wire, the page as its ledger) → `appenders`/`clients` show both → B creates 2,000 files in its own directories (0 lease conflicts, rotor mints) → A reads B's files under tokens (grants; the second read is served from the cache; a write on B recalls and A's third read is fresh) → `tar -x` on B into a directory A owns (cross-owner intents, 0 stuck, closure) → `chmod` from A on B's file (the record ship; B's stat sees it) → a 200 MiB `dd` on B (every block a ranged grant pulled from A; `df` per the rule on both; the sha round trip from A; `rm` ships the frees to A) → closure + invariant tripwires 0 on both → umount B (page Free, the member row gone) → umount A" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    // The manager's endpoint lands in tree 0 at its first checkpoint; the
    // joiner polls the projection for it (bounded) — nothing to wait for.
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{});
    _ = try b.awaitReady();
    {
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "mounted JOINED as appender 1") != null);
        try testing.expect(std.mem.indexOf(u8, text, "joining the set as an equal writer") != null);
        try testing.expect(std.mem.indexOf(u8, text, "slot leases armed (JOINED") != null);
    }
    {
        const tail = try b.stats();
        defer gpa.free(tail);
        try testing.expect(std.mem.indexOf(u8, tail, "\"mount_posture\":\"joined\"") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"wire_posture\":\"joined\"") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"manager_lease\":\"joined\"") != null);
        try testing.expectEqual(@as(?u64, 1), numberAfter(tail, "\"member_epoch\":"));
    }
    // `appenders`: pages 0 (A, mgr) and 1 (B) Live; `clients`: A's claim
    // and B's member row (B's pid, live).
    {
        const r = try rig.squeezefs(&.{ "appenders", rig.meta_uri });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nappenders: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "2 appender page(s)") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "mgr") != null);
        std.debug.print("\nsmoke: matrix: appenders with A and B:\n{s}", .{r.stdout});
    }
    {
        const deadline = clock.monotonicNs() + 20 * std.time.ns_per_s;
        while (true) {
            const r = try rig.squeezefs(&.{ "clients", rig.meta_uri });
            defer rig.freeRun(r);
            try testing.expect(r.term.success());
            if (std.mem.indexOf(u8, r.stdout, "member ") != null and std.mem.indexOf(u8, r.stdout, "writer ") != null) {
                std.debug.print("smoke: matrix: clients:\n{s}", .{r.stdout});
                break;
            }
            if (clock.monotonicNs() >= deadline) {
                std.debug.print("\n--- clients never listed B's member row ---\n{s}\n", .{r.stdout});
                return error.MembersNotListed;
            }
            sleepMs(200);
        }
    }
    // B creates 2,000 files in its own directories: the mkdirs under `/`
    // (A's slot) are cross-owner intents; the files mint into B's rotor.
    {
        const script = try matrixCreateScript(gpa);
        defer gpa.free(script);
        const out = try b.onMount(script);
        defer gpa.free(out);
        try testing.expectEqualStrings("500\n", out);
        const tail = try b.stats();
        defer gpa.free(tail);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tail, "\"slot_lease_conflicts\":"));
        try testing.expect((numberAfter(tail, "\"rotor_mints\":") orelse 0) > 0);
        try testing.expect((numberAfter(tail, "\"xv_cross_owner_intents_minted\":") orelse 0) >= matrix_dirs);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tail, "\"xv_cross_owner_intents_stuck\":"));
        std.debug.print("smoke: matrix: B created {d} files — rotor_mints {d}, intents minted {d}, slot_grants {d}, extent refills {d}\n", .{ matrix_dirs * matrix_files_per_dir, numberAfter(tail, "\"rotor_mints\":") orelse 0, numberAfter(tail, "\"xv_cross_owner_intents_minted\":") orelse 0, numberAfter(tail, "\"slot_grants\":") orelse 0, numberAfter(tail, "\"extent_grant_refills\":") orelse 0 });
    }
    // A reads B's files under tokens: the first `cat` dials B and takes
    // a grant; the second is served from A's cache (no new grant on B); a
    // write on B recalls A's token and A's third read is fresh.
    {
        const first = try rig.onMount("cat b1/f7; ls b0 | wc -l");
        defer gpa.free(first);
        try testing.expectEqualStrings("b1-f7\n500\n", first);
        const tail_b1 = try b.stats();
        defer gpa.free(tail_b1);
        const grants1 = numberAfter(tail_b1, "\"dlm_token_grants\":") orelse 0;
        try testing.expect(grants1 > 0);
        const second = try rig.onMount("cat b1/f7");
        defer gpa.free(second);
        try testing.expectEqualStrings("b1-f7\n", second);
        const tail_b2 = try b.stats();
        defer gpa.free(tail_b2);
        try testing.expectEqual(grants1, numberAfter(tail_b2, "\"dlm_token_grants\":") orelse 0);
        const w = try b.onMount("printf 'fresh\\n' > b1/f7");
        gpa.free(w);
        const third = try rig.onMount("cat b1/f7");
        defer gpa.free(third);
        try testing.expectEqualStrings("fresh\n", third);
        const tail_b3 = try b.stats();
        defer gpa.free(tail_b3);
        try testing.expect((numberAfter(tail_b3, "\"dlm_token_recalls\":") orelse 0) > 0);
        std.debug.print("smoke: matrix: A's token reads of B's files — grants on B {d}, recalls {d}, A's holder resolves {d}\n", .{ grants1, numberAfter(tail_b3, "\"dlm_token_recalls\":") orelse 0, blk: {
            const ta = try rig.onMount("cat .stats");
            defer gpa.free(ta);
            break :blk numberAfter(ta, "\"dlm_token_reader_holder_resolves\":") orelse 0;
        } });
    }
    // `tar -x` on B into a directory A owns: every create is a cross-owner
    // intent (the dentry ships to A); none stuck; closure.
    {
        const mk = try rig.onMount("mkdir a-dir");
        gpa.free(mk);
        const tar_src = try rig.shOk(rig.work, "rm -rf tree && mkdir -p tree && for i in $(seq 0 199); do printf 't%s\\n' $i > tree/t$i; done && tar -cf tree.tar tree && echo ok");
        gpa.free(tar_src);
        const out = try b.onMount("cd a-dir && tar -xf \"$1/tree.tar\" && ls tree | wc -l");
        defer gpa.free(out);
        try testing.expectEqualStrings("200\n", out);
        const seen = try rig.onMount("ls a-dir/tree | wc -l; cat a-dir/tree/t5");
        defer gpa.free(seen);
        try testing.expectEqualStrings("200\nt5\n", seen);
        const tail = try b.stats();
        defer gpa.free(tail);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tail, "\"xv_cross_owner_intents_stuck\":"));
        const minted = numberAfter(tail, "\"xv_cross_owner_intents_minted\":") orelse 0;
        const retired = numberAfter(tail, "\"xv_cross_owner_intents_retired\":") orelse 0;
        const open = numberAfter(tail, "\"xv_cross_owner_intents_open\":") orelse 0;
        try testing.expectEqual(minted, retired + open);
        std.debug.print("smoke: matrix: tar -x on B into A's dir — intents minted {d}, retired {d}, open {d}, steps shipped {d}\n", .{ minted, retired, open, numberAfter(tail, "\"xv_cross_owner_steps_shipped\":") orelse 0 });
    }
    // `chmod` from A on B's file: ONE shipped record verb to B; B's own
    // kernel view is invalidated, its `stat` sees the mode.
    {
        const ch = try rig.onMount("chmod 600 b2/f3");
        gpa.free(ch);
        const mode = try b.onMount("stat -c %a b2/f3");
        defer gpa.free(mode);
        try testing.expectEqualStrings("600\n", mode);
        const tail = try rig.onMount("cat .stats");
        defer gpa.free(tail);
        try testing.expect((numberAfter(tail, "\"record_ships\":") orelse 0) >= 1);
    }
    // The 200 MiB `dd` on B: every block minted from a ranged grant PULLED
    // from A (the allocation-lease holder — B's own cursor never moves);
    // `df` per the rule on both (B: the holder's word minus B's window = the
    // file; A: every carved grant counts as used at the holder); the sha
    // round trip read from A (the layout off B's token, including the
    // boundary-crossing reads the handler lane serves); `rm` on B ships the
    // terminal frees to A and both `df`s return.
    {
        const out = try b.onMount("mkdir b-data && dd if=/dev/urandom of=b-data/big bs=1M count=200 status=none && sync b-data/big && sha256sum b-data/big | cut -d' ' -f1 && df -k . | tail -1 | awk '{print $3}'");
        defer gpa.free(out);
        var lines = std.mem.splitScalar(u8, out, '\n');
        const sha_b = lines.next() orelse return error.ShaMissing;
        const used_b = try std.fmt.parseInt(u64, lines.next() orelse return error.DfMissing, 10);
        const tb = try b.stats();
        defer gpa.free(tb);
        const topups = numberAfter(tb, "\"block_grant_topups\":") orelse 0;
        try testing.expect(topups >= 1);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"block_grant_topup_refused\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"block_grant_topup_unreachable\":"));
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        const grants = numberAfter(ta, "\"block_grants\":") orelse 0;
        const granted_blocks = numberAfter(ta, "\"block_grant_blocks\":") orelse 0;
        try testing.expect(grants >= 1 and granted_blocks >= matrix_dd_blocks);
        const from_a = try rig.onMount("sha256sum b-data/big | cut -d' ' -f1; df -k . | tail -1 | awk '{print $3}'");
        defer gpa.free(from_a);
        var a_lines = std.mem.splitScalar(u8, from_a, '\n');
        const sha_a = a_lines.next() orelse return error.ShaMissing;
        const used_a = try std.fmt.parseInt(u64, a_lines.next() orelse return error.DfMissing, 10);
        try testing.expectEqualStrings(sha_b, sha_a);
        // `df` per the rule (block 4 MiB = 4096 KiB): B sees exactly its
        // file; A sees every granted block.
        try testing.expectEqual(matrix_dd_blocks * matrix_block_kib, used_b);
        try testing.expectEqual(granted_blocks * matrix_block_kib, used_a);
        const rm = try b.onMount("rm b-data/big");
        gpa.free(rm);
        // The forget → the terminal frees → the shipper's batch at the
        // cadence → A's executor frees them: A's `df` returns the file.
        const deadline = clock.monotonicNs() + 60 * std.time.ns_per_s;
        var used_a_after: u64 = used_a;
        while (clock.monotonicNs() < deadline) {
            const df = try rig.onMount("df -k . | tail -1 | awk '{print $3}'");
            defer gpa.free(df);
            used_a_after = try std.fmt.parseInt(u64, std.mem.trimEnd(u8, df, "\n"), 10);
            if (used_a_after == used_a - matrix_dd_blocks * matrix_block_kib) break;
            sleepMs(200);
        }
        try testing.expectEqual(used_a - matrix_dd_blocks * matrix_block_kib, used_a_after);
        const tb2 = try b.stats();
        defer gpa.free(tb2);
        const ta2 = try rig.onMount("cat .stats");
        defer gpa.free(ta2);
        try testing.expectEqual(@as(?u64, matrix_dd_blocks), numberAfter(tb2, "\"free_ship_freed\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb2, "\"free_ship_refused\":"));
        try testing.expectEqual(@as(?u64, matrix_dd_blocks), numberAfter(ta2, "\"free_served_blocks\":"));
        const df_b = try b.onMount("df -k . | tail -1 | awk '{print $3}'");
        defer gpa.free(df_b);
        std.debug.print("smoke: matrix: 200 MiB dd on B — {d} grant(s) / {d} blocks pulled from A in {d} top-up(s); sha from A equal; df used B {d} KiB, A {d} KiB; after rm on B: {d} frees shipped and freed at A, df A {d} KiB, B {s} KiB\n", .{ grants, granted_blocks, topups, used_b, used_a, numberAfter(tb2, "\"free_shipped_blocks\":") orelse 0, used_a_after, std.mem.trimEnd(u8, df_b, "\n") });
    }
    // Closure on both daemons.
    {
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        try expectMustStayZero(ta);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"manager_verb_refusals\":"));
        try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"membership_members\":"));
        const tb = try b.stats();
        defer gpa.free(tb);
        try expectMustStayZero(tb);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"joined_wire_words_rejected\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"appender_park_expiries\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"extent_grant_closure_violations\":"));
        std.debug.print("smoke: matrix: closure — A: manager_verbs {d}, token grants {d}, xv steps served {d}; B: wire verbs {d}, renewals {d}, grant refills {d}\n", .{ numberAfter(ta, "\"manager_verbs\":") orelse 0, numberAfter(ta, "\"dlm_token_grants\":") orelse 0, numberAfter(ta, "\"xv_cross_owner_steps_served\":") orelse 0, numberAfter(tb, "\"joined_wire_verbs\":") orelse 0, numberAfter(tb, "\"member_renewals\":") orelse 0, numberAfter(tb, "\"extent_grant_refills\":") orelse 0 });
    }
    // umount B: the clean leave — every lease released over the wire,
    // page 1 Free, the member row gone; every file B wrote stays visible
    // from A (its slots are the manager's now).
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, b.mp));
    {
        const r = try rig.squeezefs(&.{ "appenders", "--json", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "\"appender_id\":1,\"node\":\"0000000000000000\",\"mount_slot\":0,\"writer_id\":\"0000000000000000\",\"state\":\"free\"") != null);
        const after = try rig.onMount("ls b3 | wc -l; cat b1/f7; stat -c %a b2/f3");
        defer gpa.free(after);
        try testing.expectEqualStrings("500\nfresh\n600\n", after);
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"membership_members\":"));
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

// ---------------------------------------------------------------------------
// THE KILL ROWS (charter §5.5 "Death and recovery", "PARK-AND-RECLAIM",
// §8 `appender clear`): two daemons over real TCP, `kill -9` of either.
// ---------------------------------------------------------------------------

/// The kill rows' peer node name: B runs under its own UTS hostname (a
/// distinct node token), so a dead B's page is foreign residue to the
/// smoke's host (`appender clear` attests it) and a remount of B under the
/// same name is the same-node successor.
const kill_peer_node = "sqz-nodeb";
/// The kill rows' tree: enough entries that a `tar -x` over the wire is
/// mid-flight at the kill; each `t%d\n`, so a body names its file.
const kill_row_tree_files: usize = 1000;
/// Files visible on B before the kill: creates, cross-owner steps and
/// writes all in flight.
const kill_row_kill_after: usize = 200;
/// The storm on B for the manager's death: creates in B's own slots.
const kill_row_storm_files: usize = 2000;
/// How long a daemon's recovery / park / reclaim is given to show in
/// `.stats`: `T_owner` (3 s) + the reclaim cadence (5 s) + the recovery
/// bound (≈ 1.1 s) + the next tick, with headroom for a debug venue.
const kill_row_wait_ns: u64 = 60 * std.time.ns_per_s;

/// Poll a mount's `.stats` (`host` is a `*Rig` or a `*Peer`: both read it
/// through `stats`) until every `(key, want)` holds or the deadline
/// passes; the last tail either way (owned by the caller).
fn awaitStats(host: anytype, pairs: []const struct { key: []const u8, want: u64 }, deadline_ns: u64) ![]u8 {
    const deadline = clock.monotonicNs() + deadline_ns;
    while (true) {
        try host.expectAlive(if (pairs.len > 0) pairs[0].key else ".stats");
        const tail = (try host.statsTry()) orelse {
            if (clock.monotonicNs() >= deadline) return error.StatsUnreadable;
            sleepMs(200);
            continue;
        };
        var ok = true;
        for (pairs) |p| ok = ok and (numberAfter(tail, p.key) orelse std.math.maxInt(u64)) == p.want;
        if (ok or clock.monotonicNs() >= deadline) {
            if (!ok) std.debug.print("\n--- stats never reached the wanted words ---\n", .{});
            return tail;
        }
        testing.allocator.free(tail);
        sleepMs(200);
    }
}

/// The tar source of the kill rows under the rig's work dir.
fn killRowTarSource(rig: *Rig) !void {
    var script: std.Io.Writer.Allocating = .init(rig.gpa);
    defer script.deinit();
    try script.writer.print("rm -rf tree tree.tar && mkdir tree && i=0; while [ $i -lt {d} ]; do printf 't%s\\n' $i > tree/t$i; i=$((i + 1)); done; tar -cf tree.tar tree && echo ok", .{kill_row_tree_files});
    const out = try rig.shOk(rig.work, script.written());
    rig.gpa.free(out);
}

/// The files `tar -x` reported it could NOT open or write (`tar: tree/tN:
/// Cannot open: …`, `… Cannot write: …`) — the UNACKED set: their create
/// or write never returned to tar. Every other `tree/` entry tar touched
/// was acked (a later `Cannot change mode`/`utime`/`close` is an op after
/// the data's ack). Owned names.
fn tarUnackedNames(gpa: std.mem.Allocator, tar_err: []const u8) !std.ArrayList([]const u8) {
    var out: std.ArrayList([]const u8) = .empty;
    errdefer {
        for (out.items) |n| gpa.free(n);
        out.deinit(gpa);
    }
    var it = std.mem.splitScalar(u8, tar_err, '\n');
    while (it.next()) |line| {
        const prefix = "tar: tree/";
        if (!std.mem.startsWith(u8, line, prefix)) continue;
        const rest = line[prefix.len..];
        const colon = std.mem.indexOf(u8, rest, ": Cannot ") orelse continue;
        const verb = rest[colon + ": Cannot ".len ..];
        if (!std.mem.startsWith(u8, verb, "open") and !std.mem.startsWith(u8, verb, "write")) continue;
        try out.append(gpa, try gpa.dupe(u8, rest[0..colon]));
    }
    return out;
}

fn freeNames(gpa: std.mem.Allocator, names: *std.ArrayList([]const u8)) void {
    for (names.items) |n| gpa.free(n);
    names.deinit(gpa);
}

fn namesContain(names: []const []const u8, name: []const u8) bool {
    for (names) |n| if (std.mem.eql(u8, n, name)) return true;
    return false;
}

/// The charter's contract (§1.10 "zero acked loss") over the `tar -x` tree
/// under `dir` on `host`: every file of the `kill_row_tree_files` tar was
/// ACKED on — its open and write returned — is present and WHOLE (its
/// body names it: `t17` holds `t17`); a file tar reported unacked may be
/// absent, EMPTY (its create landed, its write never did) or whole (the
/// write landed, the reply did not) — never torn otherwise. Answers the
/// count of whole files.
fn expectTreeAcked(host: anytype, dir: []const u8, unacked: []const []const u8) !usize {
    const gpa = testing.allocator;
    var script: std.Io.Writer.Allocating = .init(gpa);
    defer script.deinit();
    try script.writer.print("for f in {s}/*; do printf '%s\\t%s\\n' \"$(basename $f)\" \"$(cat $f)\"; done", .{dir});
    const out = try host.onMount(script.written());
    defer gpa.free(out);
    var present: std.ArrayList([]const u8) = .empty;
    defer present.deinit(gpa);
    var whole: usize = 0;
    var empty_unacked: usize = 0;
    var bad: usize = 0;
    var lines = std.mem.splitScalar(u8, out, '\n');
    while (lines.next()) |line| {
        if (line.len == 0) continue;
        const tab = std.mem.indexOfScalar(u8, line, '\t') orelse return error.Unparsed;
        const name = line[0..tab];
        const body = line[tab + 1 ..];
        try present.append(gpa, name);
        if (std.mem.eql(u8, body, name)) {
            whole += 1;
        } else if (body.len == 0 and namesContain(unacked, name)) {
            empty_unacked += 1;
        } else {
            std.debug.print("\n--- {s}/{s}: body '{s}' ({s} by tar) — a torn or lost acked write ---\n", .{ dir, name, body, if (namesContain(unacked, name)) "unacked" else "ACKED" });
            bad += 1;
        }
    }
    // Every acked name is present (tar walks every entry; a name it did
    // not report unacked was acked).
    var name_buf: [16]u8 = undefined;
    for (0..kill_row_tree_files) |i| {
        const name = try std.fmt.bufPrint(&name_buf, "t{d}", .{i});
        if (namesContain(present.items, name) or namesContain(unacked, name)) continue;
        std.debug.print("\n--- {s}/{s}: ACKED by tar, absent on the mount — an acked create lost ---\n", .{ dir, name });
        bad += 1;
    }
    try testing.expectEqual(@as(usize, 0), bad);
    std.debug.print("smoke: kill rows (a): {s}: {d} whole, {d} empty unacked, {d} unacked by tar\n", .{ dir, whole, empty_unacked, unacked.len });
    return whole;
}

/// Every file under `dir` on `host`'s mount is whole (its body names it:
/// `t17` holds `t17`) — answers the count.
fn expectTreeWhole(host: anytype, dir: []const u8) !usize {
    var script: std.Io.Writer.Allocating = .init(testing.allocator);
    defer script.deinit();
    try script.writer.print("n=0; for f in {s}/*; do b=$(basename $f); test \"$(cat $f)\" = \"$b\" || {{ echo \"TORN $f\"; exit 1; }}; n=$((n + 1)); done; echo $n", .{dir});
    const out = try host.onMount(script.written());
    defer testing.allocator.free(out);
    return std.fmt.parseInt(usize, std.mem.trimEnd(u8, out, "\n"), 10);
}

test "smoke: THE KILL ROWS (a) — kill -9 B mid-`tar -x` into A's directory ⇒ A evicts B at T_owner, records the death, recovers its region (dead_members_acted ≡ recorded × regions held, rollbacks 0, the page recovered → free), every file B acked is whole on A → remount B under the same node (the same-node successor) sees them → B's FIRST cross-owner mkdir succeeds → A's `rm -rf` of the recovered tree completes; recall timeouts 0, tripwires 0 on both" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.node = kill_peer_node;
    try b.spawn(&.{});
    _ = try b.awaitReady();
    try killRowTarSource(rig);
    {
        const mk = try rig.onMount("mkdir a-dir");
        gpa.free(mk);
    }
    // `tar -x` on B into A's directory, in a child; B dies once enough
    // files are visible on its own mount.
    var tar_path_buf: [160]u8 = undefined;
    const tar_path = try std.fmt.bufPrint(&tar_path_buf, "{s}/tree.tar", .{rig.work});
    var dest_buf: [160]u8 = undefined;
    const dest = try std.fmt.bufPrint(&dest_buf, "{s}/a-dir", .{b.mp});
    // tar's stderr IS the ack ledger: the files it could not open or write
    // when B died are the unacked ones (read after the wait).
    var tar_err_buf: [192]u8 = undefined;
    const tar_err_path = try std.fmt.bufPrint(&tar_err_buf, "{s}/tar.err", .{rig.work});
    const tar_err_file = try Io.Dir.cwd().createFile(io, tar_err_path, .{ .truncate = true });
    defer tar_err_file.close(io);
    var tar = try std.process.spawn(io, .{ .argv = &.{ "tar", "-xf", tar_path, "-C", dest }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .{ .file = tar_err_file } });
    var seen: usize = 0;
    {
        const deadline = clock.monotonicNs() + 60 * std.time.ns_per_s;
        var tree_buf: [160]u8 = undefined;
        const tree_path = try std.fmt.bufPrint(&tree_buf, "{s}/a-dir/tree", .{b.mp});
        while (clock.monotonicNs() < deadline) {
            if (Io.Dir.cwd().openDir(io, tree_path, .{ .iterate = true })) |d| {
                var dir = d;
                defer dir.close(io);
                seen = try countFiles(gpa, io, dir);
                if (seen >= kill_row_kill_after) break;
            } else |_| {}
            sleepMs(5);
        }
    }
    try testing.expect(seen >= kill_row_kill_after);
    try b.kill();
    const tar_term = try tar.wait(io);
    const tar_err = try Io.Dir.cwd().readFileAlloc(io, tar_err_path, gpa, .limited(1 << 20));
    defer gpa.free(tar_err);
    var unacked = try tarUnackedNames(gpa, tar_err);
    defer freeNames(gpa, &unacked);
    // tar met the dead transport (exit 2, every later entry refused): a
    // tar that finished clean never saw the kill.
    try testing.expect(tar_term != .exited or tar_term.exited != 0);
    try testing.expect(unacked.items.len > 0);
    std.debug.print("\nsmoke: kill rows (a): kill -9 B with {d} of {d} files visible; tar {f}, {d} entries unacked\n", .{ seen, kill_row_tree_files, tar_term, unacked.items.len });
    // A: eviction at T_owner → the death recorded → acted → the region
    // recovered → released at the next tick.
    const t_kill = clock.monotonicNs();
    const ta = try awaitStats(rig, &.{ .{ .key = "\"appender_recoveries\":", .want = 1 }, .{ .key = "\"recovered_regions_released\":", .want = 1 } }, kill_row_wait_ns);
    defer gpa.free(ta);
    const recovered_ms = (clock.monotonicNs() - t_kill) / std.time.ns_per_ms;
    try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"membership_evictions\":"));
    try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"dead_members_recorded\":"));
    try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"dead_members_acted\":"));
    try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"appender_recoveries\":"));
    try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"appender_recovery_rollbacks\":"));
    // No image B wrote is unreached (the census judges by content: a
    // refill granted after B's last page write is returned unnamed, never
    // an orphan).
    try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"dead_appender_orphans\":"));
    try expectMustStayZero(ta);
    {
        const r = try rig.squeezefs(&.{ "appenders", "--json", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        // A released region keeps the dead identity and term on its Free
        // page (unlike a clean leave's cleared identity).
        const row = std.mem.indexOf(u8, r.stdout, "\"appender_id\":1,") orelse return error.PageMissing;
        try testing.expect(std.mem.indexOf(u8, r.stdout[row..], "\"state\":\"free\"") != null);
    }
    // Every file tar was acked on is whole on A (C8/C6: the body names the
    // file); the unacked tail may be absent or empty, never torn.
    const whole_a = try expectTreeAcked(rig, "a-dir/tree", unacked.items);
    std.debug.print("smoke: kill rows (a): A recovered B's region {d} ms after the kill — evictions 1, dead_members recorded 1 / acted 1, appender_recoveries 1, rollbacks 0, orphans 0, unwritten grant returned {d}, page free; {d} files whole on A ({d} seen at the kill)\n", .{ recovered_ms, numberAfter(ta, "\"dead_appender_unwritten_returned\":") orelse 0, whole_a, seen });
    // The same-node successor: B again under the same node name.
    const mark_b = try b.logLen();
    try b.spawn(&.{});
    _ = try b.awaitLog("ready: ", mark_b, ready_wait_ns);
    {
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expectEqual(@as(usize, 2), std.mem.count(u8, text, "mounted JOINED as appender 1"));
    }
    const whole_b = try expectTreeAcked(b, "a-dir/tree", unacked.items);
    try testing.expectEqual(whole_a, whole_b);
    // B's FIRST cross-owner op after the recovery: a mkdir under A's root.
    {
        const t0 = clock.monotonicNs();
        const out = try b.onMount("timeout -s KILL 20 mkdir b2-dir && echo ok");
        defer gpa.free(out);
        try testing.expectEqualStrings("ok\n", out);
        const mkdir_ms = (clock.monotonicNs() - t0) / std.time.ns_per_ms;
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expectEqual(@as(?u64, 1), numberAfter(tb, "\"xv_cross_owner_intents_minted\":"));
        try testing.expectEqual(@as(?u64, 1), numberAfter(tb, "\"xv_cross_owner_intents_retired\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"xv_cross_owner_intents_stuck\":"));
        std.debug.print("smoke: kill rows (a): the successor's first cross-owner mkdir landed in {d} ms (intents minted 1, retired 1)\n", .{mkdir_ms});
    }
    // A's `rm -rf` of the recovered tree: every unlink recalls the
    // successor's tokens and completes.
    {
        const t0 = clock.monotonicNs();
        const out = try rig.onMount("timeout -s KILL 60 rm -rf a-dir/tree && ls a-dir | wc -l");
        defer gpa.free(out);
        try testing.expectEqualStrings("0\n", out);
        const rm_ms = (clock.monotonicNs() - t0) / std.time.ns_per_ms;
        const ta2 = try rig.onMount("cat .stats");
        defer gpa.free(ta2);
        try expectMustStayZero(ta2);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta2, "\"dlm_token_recall_timeouts_live\":"));
        const tb = try b.stats();
        defer gpa.free(tb);
        try expectMustStayZero(tb);
        std.debug.print("smoke: kill rows (a): A's rm -rf of the recovered tree ({d} files) completed in {d} ms — recalls {d}, acked at B {d}, timeouts 0, tripwires 0 on both\n", .{ whole_a, rm_ms, numberAfter(ta2, "\"dlm_token_recalls\":") orelse 0, numberAfter(tb, "\"dlm_token_recalls_acked\":") orelse 0 });
    }
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

test "smoke: THE KILL ROWS (b) — kill -9 A (the manager) mid-storm on B ⇒ B PARKS-AND-RECLAIMS at T_self (acks held at the pre-admission gate, park expiries 0) → remount A (the D0 ladder: the same-host dead-pid proof, A wins volume 0 again at term 2, the grace window open, the allocation leases taken over from the dead incarnation) → B's reclaim lands in the grace window, its manager link re-points, the held writes drain and every storm file is whole on both" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.node = kill_peer_node;
    try b.spawn(&.{});
    _ = try b.awaitReady();
    // The storm on B in a child: creates into B's own directory, every
    // body naming its file; the script's exit is the storm's verdict.
    {
        const mk = try b.onMount("mkdir storm");
        gpa.free(mk);
    }
    // The storm runs WITH `storm` AS ITS CWD: every path it walks is B's
    // own object. A path through A's ROOT (`storm/sN` from the mount
    // root) revalidates the `storm` dentry against the root's holder
    // every `dir_entry` TTL (1 s) — a foreign read that, uncached while A
    // is dead, is the charter's typed EAGAIN, not the row's contract (the
    // row pins B's OWN writes held at the gate; the finding is recorded
    // in the evidence note). The storm's stderr names a refused op and
    // its errno (read on failure).
    var storm_script: std.Io.Writer.Allocating = .init(gpa);
    defer storm_script.deinit();
    try storm_script.writer.print("cd \"$1/storm\" && i=0; while [ $i -lt {d} ]; do printf 's%s\\n' $i > s$i || {{ echo \"storm: s$i refused (rc $?)\" >&2; exit 1; }}; i=$((i + 1)); done", .{kill_row_storm_files});
    var storm_err_buf: [192]u8 = undefined;
    const storm_err_path = try std.fmt.bufPrint(&storm_err_buf, "{s}/storm.err", .{rig.work});
    const storm_err_file = try Io.Dir.cwd().createFile(io, storm_err_path, .{ .truncate = true });
    defer storm_err_file.close(io);
    var storm = try std.process.spawn(io, .{ .argv = &.{ "sh", "-c", storm_script.written(), "storm", b.mp }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .{ .file = storm_err_file } });
    var seen: usize = 0;
    {
        const deadline = clock.monotonicNs() + 60 * std.time.ns_per_s;
        var storm_buf: [160]u8 = undefined;
        const storm_path = try std.fmt.bufPrint(&storm_buf, "{s}/storm", .{b.mp});
        while (clock.monotonicNs() < deadline) {
            var dir = try Io.Dir.cwd().openDir(io, storm_path, .{ .iterate = true });
            defer dir.close(io);
            seen = try countFiles(gpa, io, dir);
            if (seen >= kill_row_kill_after) break;
            sleepMs(5);
        }
    }
    try testing.expect(seen >= kill_row_kill_after);
    // B's pool before the kill: sized by its measured SMO rate (charter
    // §5.5), so the storm's claims through `T_self` never exhaust it
    // while the manager cannot refill.
    const tb_pre = try b.stats();
    defer gpa.free(tb_pre);
    try rig.kill();
    const t_kill = clock.monotonicNs();
    std.debug.print("\nsmoke: kill rows (b): kill -9 A with {d} storm files visible on B (B's pool: smo_ewma {d} milli/s, refills {d}, refill kicks {d}; B's tokens: grants {d}, hits {d}, cached {d}, recalls received {d})\n", .{ seen, numberAfter(tb_pre, "\"extent_grant_smo_ewma_milli_per_s\":") orelse 0, numberAfter(tb_pre, "\"extent_grant_refills\":") orelse 0, numberAfter(tb_pre, "\"extent_grant_refill_kicks\":") orelse 0, numberAfter(tb_pre, "\"dlm_token_grants\":") orelse 0, numberAfter(tb_pre, "\"dlm_token_hits\":") orelse 0, numberAfter(tb_pre, "\"dlm_token_cached\":") orelse 0, numberAfter(tb_pre, "\"dlm_token_recalls_received\":") orelse 0 });
    // B parks at T_self: the daemon's own word in its log (its `.stats`
    // is behind the kernel's revalidation of the root — a foreign object
    // whose holder is dead — until the successor stands), the storm's
    // acks held, the storm child alive — no op of the storm was answered
    // `GrantExhausted` (EAGAIN) in the window before the park.
    const b_mark = try b.logLen();
    _ = try b.awaitLog("appender park: T_self reached with the home volume 0's manager unreachable — parked", 0, kill_row_wait_ns);
    const parked_ms = (clock.monotonicNs() - t_kill) / std.time.ns_per_ms;
    {
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[b_mark..], "POISONED") == null);
        if (!alive(storm.id.?)) {
            const term = try storm.wait(io);
            const storm_err = try Io.Dir.cwd().readFileAlloc(io, storm_err_path, gpa, .limited(1 << 16));
            defer gpa.free(storm_err);
            const tb_now = try b.statsDump();
            defer gpa.free(tb_now);
            std.debug.print("\n--- the storm child exited ({f}) before B's park: an op was refused while the manager was unreachable ---\n{s}\nB: manager_dependency_stalls {d}, enospc refusals {d}, heap promised {d}; tokens: grants {d}, hits {d}, cached {d}, sheds {d}, recalls received {d}, serve refusals {d}, reprobes {d}, repoints {d}, channel_ok {d}\n{s}\n", .{ term, storm_err, numberAfter(tb_now, "\"manager_dependency_stalls\":") orelse 0, numberAfter(tb_now, "\"meta_kv_enospc_refusals\":") orelse 0, numberAfter(tb_now, "\"meta_kv_heap_promised\":") orelse 0, numberAfter(tb_now, "\"dlm_token_grants\":") orelse 0, numberAfter(tb_now, "\"dlm_token_hits\":") orelse 0, numberAfter(tb_now, "\"dlm_token_cached\":") orelse 0, numberAfter(tb_now, "\"dlm_token_sheds\":") orelse 0, numberAfter(tb_now, "\"dlm_token_recalls_received\":") orelse 0, numberAfter(tb_now, "\"dlm_token_serve_refusals\":") orelse 0, numberAfter(tb_now, "\"dlm_token_reader_channel_reprobes\":") orelse 0, numberAfter(tb_now, "\"dlm_token_holder_repoints\":") orelse 0, numberAfter(tb_now, "\"dlm_token_channel_ok\":") orelse 0, text[b_mark..] });
            return error.StormRefusedBeforePark;
        }
    }
    std.debug.print("smoke: kill rows (b): B PARKED {d} ms after the kill (acks held); the storm child is alive, waiting\n", .{parked_ms});
    // Remount A: the D0 ladder's same-host dead-pid proof; A wins volume
    // 0 again as the manager at term 2 with the grace window open.
    const mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        const seg = text[mark..];
        try testing.expect(std.mem.indexOf(u8, seg, "appender 0 Live as the manager (term 2") != null);
        try testing.expect(std.mem.indexOf(u8, seg, "membership owner term 2 (T_owner 3000 ms, grace open)") != null);
        try testing.expect(std.mem.indexOf(u8, seg, "no grant plane armed") == null);
    }
    // B's reclaim lands: the park released (the park and the fence
    // counted once each, no expiry), the storm completes.
    const reclaimed = try awaitStats(b, &.{.{ .key = "\"appender_parked\":", .want = 0 }}, kill_row_wait_ns);
    defer gpa.free(reclaimed);
    try testing.expectEqual(@as(?u64, 0), numberAfter(reclaimed, "\"appender_park_expiries\":"));
    try testing.expectEqual(@as(?u64, 1), numberAfter(reclaimed, "\"membership_self_fences\":"));
    try testing.expectEqual(@as(?u64, 1), numberAfter(reclaimed, "\"manager_repoints\":"));
    try testing.expect((numberAfter(reclaimed, "\"dlm_token_holder_repoints\":") orelse 0) >= 1);
    {
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "the manager's listener moved to") != null);
        try testing.expect(std.mem.indexOf(u8, text, "admitted the reclaim") != null);
    }
    const storm_term = try storm.wait(io);
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, storm_term);
    // Every storm file whole on B (its own slots) AND on A (under tokens
    // from B — A's reader plane dialled B's listener afresh).
    const whole_b = try expectTreeWhole(b, "storm");
    try testing.expectEqual(kill_row_storm_files, whole_b);
    {
        const out = try rig.onMount("ls storm | wc -l; cat storm/s7");
        defer gpa.free(out);
        var want_buf: [32]u8 = undefined;
        try testing.expectEqualStrings(try std.fmt.bufPrint(&want_buf, "{d}\ns7\n", .{kill_row_storm_files}), out);
    }
    {
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        try expectMustStayZero(ta);
        try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"membership_members\":"));
        // The dead incarnation's allocation leases were taken over, one
        // per data volume: its death recorded EARLY by each takeover (the
        // same key; retired by the grant), B's region never recovered.
        try testing.expectEqual(@as(?u64, 2), numberAfter(ta, "\"alloc_lease_same_node_takeovers\":"));
        try testing.expectEqual(@as(?u64, 2), numberAfter(ta, "\"alloc_lease_successions\":"));
        try testing.expectEqual(@as(?u64, 2), numberAfter(ta, "\"dead_members_recorded\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"appender_recoveries\":"));
        // B's held write outlived the op watchdog's 5 s by design (the
        // park holds acks up to `T_park_max`): the must-stay-0s that
        // matter here are named one by one.
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"invariant_tripwires\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"appender_park_expiries\":"));
        // The storm never met `GrantExhausted`: the pool held through the
        // manager's absence (must-stay-0 at the sized grant).
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"manager_dependency_stalls\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"transport_requests_abandoned\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"open_count_stranded\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"joined_wire_words_rejected\":"));
        try testing.expect(std.mem.indexOf(u8, tb, "\"volume_failed\":false") != null);
        std.debug.print("smoke: kill rows (b): A remounted as the manager at term 2 (grace open), B's reclaim landed (manager repoints 1, token plane repoints {d}, reconnects {d}), the storm's {d} files whole on both; park expiries 0, tripwires 0 on both\n", .{ numberAfter(tb, "\"dlm_token_holder_repoints\":") orelse 0, numberAfter(tb, "\"cluster_wire_reconnects\":") orelse 0, whole_b });
    }
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

test "smoke: THE KILL ROWS (c) — `appender clear` refuses a live-mounted set; after A's clean umount parks B and B is killed unrecovered, `appender clear <uri> 1` performs the operator path (the page Recovering{0}, the writer bring-up, the EARLY record, the leave) and `appenders` shows `recovering`; `appender clear <uri> 0` refuses; the next mount's poll recovers the region and `appenders` shows `free`" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.node = kill_peer_node;
    try b.spawn(&.{});
    _ = try b.awaitReady();
    {
        const out = try b.onMount("mkdir bdir && for i in 0 1 2 3 4 5 6 7; do printf 'b%s\\n' $i > bdir/f$i; done; ls bdir | wc -l");
        defer gpa.free(out);
        try testing.expectEqualStrings("8\n", out);
    }
    // Live-mounted: refused.
    {
        const r = try rig.squeezefs(&.{ "appender", "clear", rig.meta_uri, "1" });
        defer rig.freeRun(r);
        try testing.expect(!r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stderr, "live-mounted") != null);
    }
    // A leaves cleanly (its claim deleted); B parks at T_self; B dies
    // parked with its page Live — unrecovered, no manager to record it.
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try b.awaitLog("appender park: T_self reached with the home volume 0's manager unreachable — parked", 0, kill_row_wait_ns);
    try b.kill();
    {
        const r = try rig.squeezefs(&.{ "appenders", "--json", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "\"appender_id\":1,") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "\"state\":\"live\"") != null);
    }
    // The operator path: the four writes.
    {
        const r = try rig.squeezefs(&.{ "appender", "clear", rig.meta_uri, "1" });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nappender clear: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "appender 1 cleared (was live)") != null);
    }
    {
        const r = try rig.squeezefs(&.{ "appenders", "--json", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        const row = std.mem.indexOf(u8, r.stdout, "\"appender_id\":1,") orelse return error.PageMissing;
        try testing.expect(std.mem.indexOf(u8, r.stdout[row..], "\"state\":\"recovering\"") != null);
        std.debug.print("\nsmoke: kill rows (c): appender clear 1 → the page Recovering{{0}} + the EARLY record:\n{s}", .{r.stdout});
    }
    // Appender 0's page is the manager's: refused.
    {
        const r = try rig.squeezefs(&.{ "appender", "clear", rig.meta_uri, "0" });
        defer rig.freeRun(r);
        try testing.expect(!r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stderr, "refusing") != null);
    }
    // The next mount's poll recovers the attested region: recovered, then
    // free; every file B acked is whole.
    const mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    const ta = try awaitStats(rig, &.{ .{ .key = "\"appender_recoveries\":", .want = 1 }, .{ .key = "\"recovered_regions_released\":", .want = 1 } }, kill_row_wait_ns);
    defer gpa.free(ta);
    try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"dead_members_acted\":"));
    try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"appender_recovery_rollbacks\":"));
    try testing.expect((numberAfter(ta, "\"slot_custody_quarantines_installed\":") orelse 0) >= 1);
    try expectMustStayZero(ta);
    {
        const r = try rig.squeezefs(&.{ "appenders", "--json", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        const row = std.mem.indexOf(u8, r.stdout, "\"appender_id\":1,") orelse return error.PageMissing;
        try testing.expect(std.mem.indexOf(u8, r.stdout[row..], "\"state\":\"free\"") != null);
    }
    {
        const out = try rig.onMount("ls bdir | wc -l; cat bdir/f3");
        defer gpa.free(out);
        try testing.expectEqualStrings("8\nb3\n", out);
        std.debug.print("smoke: kill rows (c): the next mount recovered the attested region (acted 1, rollbacks 0, custody quarantined {d}), the page free, B's 8 files whole\n", .{numberAfter(ta, "\"slot_custody_quarantines_installed\":") orelse 0});
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

test "smoke: THE `-o ro` READER OF A LIVE SET — C mounts `-o ro` beside A (the manager) and B (joined): no flock, no claim, no region; a member-reader + token client (TTLs 0, reader_staleness_bound_ms 0) reading B's 2,000 files and A's tree under tokens (0 local reads) → B writes ⇒ C's next read is fresh → A and B leave ⇒ C becomes the QUIESCENT reader of their last checkpoint → A mounts again ⇒ C fail-stops ESTALE at its next op (`.stats` still readable) → remount C ⇒ a token reader again" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{});
    _ = try b.awaitReady();
    {
        const script = try matrixCreateScript(gpa);
        defer gpa.free(script);
        const out = try b.onMount(script);
        defer gpa.free(out);
        try testing.expectEqualStrings("500\n", out);
        const mk = try rig.onMount("mkdir a-tree && for i in 0 1 2 3; do printf 'a%s\\n' $i > a-tree/f$i; done; echo ok");
        gpa.free(mk);
    }
    // C: `-o ro` of the LIVE set — a reader, never a claim.
    const c = try Peer.init(rig, "c");
    defer c.deinit();
    try c.spawn(&.{ "-o", "ro" });
    _ = try c.awaitReady();
    {
        const text = try c.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "mounted as a READER") != null);
        try testing.expect(std.mem.indexOf(u8, text, "`-o ro` of a LIVE set — a member-reader of the home shard") != null);
        try testing.expect(std.mem.indexOf(u8, text, "TTLs attr 0 ms entry 0 ms dir_entry 0 ms negative 0 ms") != null);
        try testing.expect(std.mem.indexOf(u8, text, "writer claim committed") == null);
    }
    {
        const r = try rig.squeezefs(&.{ "appenders", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        // A reader holds no region: the directory still lists A and B alone.
        try testing.expect(std.mem.indexOf(u8, r.stdout, "2 appender page(s)") != null);
    }
    // C reads B's 2,000 files and A's tree under tokens.
    {
        const out = try c.onMount("ls b0 b1 b2 b3 | grep -c '^f'; cat b1/f7; cat a-tree/f2; ls a-tree | wc -l; echo x > b0/new 2>&1 || echo refused");
        defer gpa.free(out);
        try testing.expectEqualStrings("2000\nb1-f7\na2\n4\nrefused\n", out);
        const tc = try c.stats();
        defer gpa.free(tc);
        try testing.expect(std.mem.indexOf(u8, tc, "\"mount_posture\":\"reader\"") != null);
        try testing.expect(std.mem.indexOf(u8, tc, "\"reader_arm\":\"tokens\"") != null);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tc, "\"reader_staleness_bound_ms\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tc, "\"dlm_token_local_reads\":"));
        try testing.expect((numberAfter(tc, "\"dlm_token_grants\":") orelse 0) >= 2000);
        try testing.expectEqual(@as(?u64, 2), numberAfter(tc, "\"dlm_token_reader_holder_planes\":"));
        std.debug.print("\nsmoke: -o ro: C read B's 2,000 files and A's tree under tokens — grants {d}, hits {d}, local reads 0, holder planes 2, staleness bound 0 ms\n", .{ numberAfter(tc, "\"dlm_token_grants\":") orelse 0, numberAfter(tc, "\"dlm_token_hits\":") orelse 0 });
    }
    // B writes: the holder's recall reaches C's listener; C's next read
    // is the fresh content (no TTL, no poll — the recall is the proof).
    {
        const w = try b.onMount("printf 'fresh\\n' > b1/f7");
        gpa.free(w);
        const out = try c.onMount("cat b1/f7");
        defer gpa.free(out);
        try testing.expectEqualStrings("fresh\n", out);
        const tc = try c.stats();
        defer gpa.free(tc);
        try testing.expect((numberAfter(tc, "\"dlm_token_recalls_received\":") orelse 0) >= 1);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tc, "\"dlm_token_recall_timeouts_live\":"));
    }
    // The writers leave cleanly: C's lease lapses, no writer face stands,
    // C becomes the quiescent reader of their last checkpoint.
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    {
        // The reader's root is its holder's until the lease lapses: the
        // walk to `.stats` may answer EAGAIN in between (the tolerant read).
        const deadline = clock.monotonicNs() + kill_row_wait_ns;
        while (true) {
            if (try c.statsTry()) |tc| {
                defer gpa.free(tc);
                if (std.mem.indexOf(u8, tc, "\"reader_arm\":\"quiescent\"") != null) break;
            }
            if (clock.monotonicNs() >= deadline) return error.ReaderNeverQuiescent;
            sleepMs(200);
        }
        const out = try c.onMount("cat b1/f7; ls b3 | wc -l; cat a-tree/f2");
        defer gpa.free(out);
        try testing.expectEqualStrings("fresh\n500\na2\n", out);
        const tc = try c.stats();
        defer gpa.free(tc);
        try testing.expectEqual(@as(?u64, 1), numberAfter(tc, "\"reader_quiescent_transitions\":"));
        try testing.expect((numberAfter(tc, "\"dlm_token_local_reads\":") orelse 0) >= 1);
        std.debug.print("smoke: -o ro: A and B left — C became the quiescent reader of their last checkpoint; every read local and fresh\n", .{});
    }
    // A mounts again: the quiescent reader's poll sees the writer and
    // fail-stops — every op ESTALE, `.stats` still readable.
    const mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    {
        const deadline = clock.monotonicNs() + kill_row_wait_ns;
        while (true) {
            const tc = try c.stats();
            defer gpa.free(tc);
            if (std.mem.indexOf(u8, tc, "\"reader_fail_stopped\":true") != null) break;
            if (clock.monotonicNs() >= deadline) return error.ReaderNeverFailStopped;
            sleepMs(100);
        }
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "sh", "-c", "ls b3 2>&1; cat a-tree/f2 2>&1" }, .cwd = .{ .path = c.mp }, .environ_map = &rig.env });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try testing.expectEqual(@as(usize, 2), std.mem.count(u8, r.stdout, "Stale file handle"));
        const text = try c.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "a writer appeared (ledger_advance) under the quiescent reader — every metadata op now answers ESTALE") != null);
        std.debug.print("smoke: -o ro: A mounted again — C fail-stopped (ESTALE on ls and cat; .stats readable)\n", .{});
    }
    // Remount C: a token reader of the live set again.
    try c.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try c.waitExit());
    const mark_c = try c.logLen();
    try c.spawn(&.{ "-o", "ro" });
    _ = try c.awaitLog("ready: ", mark_c, ready_wait_ns);
    {
        const out = try c.onMount("cat b1/f7; cat a-tree/f2");
        defer gpa.free(out);
        try testing.expectEqualStrings("fresh\na2\n", out);
        const tc = try c.stats();
        defer gpa.free(tc);
        try testing.expect(std.mem.indexOf(u8, tc, "\"reader_arm\":\"tokens\"") != null);
        try expectMustStayZero(tc);
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        try expectMustStayZero(ta);
        try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"membership_members\":"));
    }
    try c.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try c.waitExit());
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
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
const data_dir_knob = "SQUEEZEFS_TEST_DATA_DIR";
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

/// The JSON object that follows `key` in `text` (brace-balanced, strings
/// skipped), or null.
fn objectAfter(text: []const u8, key: []const u8) ?[]const u8 {
    const at = (std.mem.indexOf(u8, text, key) orelse return null) + key.len;
    if (at >= text.len or text[at] != '{') return null;
    var depth: usize = 0;
    var in_str = false;
    var i = at;
    while (i < text.len) : (i += 1) {
        const c = text[i];
        if (in_str) {
            if (c == '\\') i += 1 else if (c == '"') in_str = false;
            continue;
        }
        switch (c) {
            '"' => in_str = true,
            '{', '[' => depth += 1,
            '}', ']' => {
                depth -= 1;
                if (depth == 0) return text[at .. i + 1];
            },
            else => {},
        }
    }
    return null;
}

/// The words a symmetric gauge may read on a `--single-writer` mount.
const solo_words = [_][]const u8{ "solo", "off", "none", "vacant", "" };

/// Charter §5.5 "Every one reads 0/`solo` on a `--single-writer`
/// mount": every `"key":value` pair of `family` (recursively) is a number
/// reading 0, a boolean reading false, or a string from `solo_words`;
/// the first offender is named.
fn expectSoloPosture(family_name: []const u8, family: []const u8) !void {
    var bad: usize = 0;
    var i: usize = 0;
    while (std.mem.indexOfScalarPos(u8, family, i, '"')) |q| {
        const end = std.mem.indexOfScalarPos(u8, family, q + 1, '"') orelse break;
        const k = family[q + 1 .. end];
        i = end + 1;
        if (i >= family.len or family[i] != ':') continue;
        const v = std.mem.trimStart(u8, family[i + 1 ..], " ");
        if (v.len == 0) break;
        switch (v[0]) {
            '{', '[' => {},
            '"' => {
                const se = std.mem.indexOfScalarPos(u8, v, 1, '"') orelse break;
                const s = v[1..se];
                var ok = false;
                for (solo_words) |w| ok = ok or std.mem.eql(u8, w, s);
                if (!ok) {
                    std.debug.print("\n--- {s}: {s} = \"{s}\" on a --single-writer mount (expected a solo word) ---\n", .{ family_name, k, s });
                    bad += 1;
                }
                i += 1 + se + 1;
            },
            't' => {
                std.debug.print("\n--- {s}: {s} = true on a --single-writer mount ---\n", .{ family_name, k });
                bad += 1;
            },
            'f', 'n' => {},
            else => {
                var e: usize = 0;
                while (e < v.len and (std.ascii.isDigit(v[e]) or v[e] == '-' or v[e] == '.')) : (e += 1) {}
                const n = v[0..e];
                if (n.len == 0) continue;
                if (!std.mem.allEqual(u8, n, '0') and !std.mem.eql(u8, n, "0.0")) {
                    std.debug.print("\n--- {s}: {s} = {s} on a --single-writer mount (expected 0) ---\n", .{ family_name, k, n });
                    bad += 1;
                }
            },
        }
    }
    try testing.expectEqual(@as(usize, 0), bad);
}

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

/// Kernel aio (the raw `io_setup`/`io_submit`/`io_getevents` syscalls — no
/// shim, no libaio) against `path` on a mount: `n` PWRITEs of one 4 KiB
/// page each in ONE `io_submit` (offsets `0, 4 KiB, …`: a fresh file's
/// inline → striped promotion under concurrent writes), every event's
/// `res` the page, then `n` PREADs in one submit and a byte-exact
/// read-back of every page. `oflags` extra open flags (`O_DIRECT` is the
/// kernel's GUP/direct path through `fuse_direct_io`).
fn kernelAioPass(path: [:0]const u8, n: usize, o_direct: bool) !void {
    const aio = @import("../shim/aio.zig");
    const page: usize = 4096;
    var ctx: aio.io_context_t = 0;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.syscall2(.io_setup, 32, @intFromPtr(&ctx))));
    defer _ = linux.syscall1(.io_destroy, ctx);
    var flags: linux.O = .{ .ACCMODE = .RDWR, .CREAT = true, .TRUNC = true, .CLOEXEC = true };
    if (o_direct) flags.DIRECT = true;
    const rc = linux.open(path.ptr, flags, 0o644);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    const gpa = testing.allocator;
    const wbuf = try gpa.alignedAlloc(u8, .fromByteUnits(page), n * page);
    defer gpa.free(wbuf);
    const rbuf = try gpa.alignedAlloc(u8, .fromByteUnits(page), n * page);
    defer gpa.free(rbuf);
    for (0..n) |i| @memset(wbuf[i * page ..][0..page], @as(u8, @intCast('a' + i)));
    @memset(rbuf, 0);
    const cbs = try gpa.alloc(aio.Iocb, n);
    defer gpa.free(cbs);
    const ptrs = try gpa.alloc(*aio.Iocb, n);
    defer gpa.free(ptrs);
    const evs = try gpa.alloc(aio.IoEvent, n);
    defer gpa.free(evs);
    for ([_]u16{ aio.IOCB_CMD_PWRITE, aio.IOCB_CMD_PREAD }) |op| {
        for (cbs, 0..) |*cb, i| {
            const buf = if (op == aio.IOCB_CMD_PWRITE) wbuf else rbuf;
            cb.* = .{ .data = 100 + i, .key = 0, .aio_rw_flags = 0, .aio_lio_opcode = op, .aio_reqprio = 0, .aio_fildes = fd, .buf = @intFromPtr(buf.ptr + i * page), .nbytes = page, .offset = @intCast(i * page), .__pad3 = 0, .flags = 0, .resfd = 0 };
            ptrs[i] = cb;
        }
        const submitted = linux.syscall3(.io_submit, ctx, n, @intFromPtr(ptrs.ptr));
        try testing.expectEqual(n, submitted);
        var got: usize = 0;
        while (got < n) {
            const ts: linux.timespec = .{ .sec = 30, .nsec = 0 };
            const r = linux.syscall5(.io_getevents, ctx, 1, n - got, @intFromPtr(evs.ptr + got), @intFromPtr(&ts));
            try testing.expectEqual(linux.E.SUCCESS, linux.errno(r));
            try testing.expect(r > 0);
            got += r;
        }
        for (evs) |ev| {
            try testing.expect(ev.data >= 100 and ev.data < 100 + n);
            if (ev.res != page) std.debug.print("\n--- kernel aio {s} on {s}: iocb {d} res {d} ({t}) ---\n", .{ if (op == aio.IOCB_CMD_PWRITE) "PWRITE" else "PREAD", path, ev.data - 100, ev.res, linux.errno(@as(usize, @bitCast(ev.res))) });
            try testing.expectEqual(@as(i64, page), ev.res);
        }
    }
    try testing.expectEqualSlices(u8, wbuf, rbuf);
}

/// The kernel-aio pass on a set WITH or WITHOUT a cache path: a fresh
/// file, the same file again (`O_TRUNC` over an existing striped file), a
/// fresh file under `O_DIRECT`; then the whole bytes read back through
/// `cat | sha256sum` against the expected image.
fn kernelAioStage(rig: *Rig, cache: bool) !void {
    try rig.formatSet(cache);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const n: usize = 8;
    var path_buf: [192]u8 = undefined;
    const path = try std.fmt.bufPrintSentinel(&path_buf, "{s}/aio.bin", .{rig.mp}, 0);
    try kernelAioPass(path, n, false);
    try kernelAioPass(path, n, false);
    var dpath_buf: [192]u8 = undefined;
    const dpath = try std.fmt.bufPrintSentinel(&dpath_buf, "{s}/aio-direct.bin", .{rig.mp}, 0);
    try kernelAioPass(dpath, n, true);
    // From another process, through the kernel's read path: both files
    // hash to the image the pass wrote (page i filled with 'a' + i).
    var image: [8 * 4096]u8 = undefined;
    for (0..n) |i| @memset(image[i * 4096 ..][0..4096], @as(u8, @intCast('a' + i)));
    var digest: [std.crypto.hash.sha2.Sha256.digest_length]u8 = undefined;
    std.crypto.hash.sha2.Sha256.hash(&image, &digest, .{});
    var want_buf: [160]u8 = undefined;
    const want = try std.fmt.bufPrint(&want_buf, "{x}  aio.bin\n{x}  aio-direct.bin\n", .{ digest, digest });
    const out = try rig.onMount("sha256sum aio.bin aio-direct.bin");
    defer testing.allocator.free(out);
    try testing.expectEqualStrings(want, out);
    const tail = try rig.stats();
    defer testing.allocator.free(tail);
    try expectMustStayZero(tail);
    std.debug.print("\nsmoke: kernel aio on the plain mount ({s} cache path): 3 passes × {d} PWRITE + {d} PREAD byte-exact (fresh, O_TRUNC over striped, O_DIRECT); write routing inline {d} / staged {d} / striped {d}, promotions {d}, held-slot stores {d}, extractions {d}\n", .{ if (cache) "with a" else "no", n, n, numberAfter(tail, "\"fuse_write_inline\":") orelse 0, numberAfter(tail, "\"fuse_write_staged\":") orelse 0, numberAfter(tail, "\"fuse_write_striped\":") orelse 0, numberAfter(tail, "\"fuse_write_promotions_to_striped\":") orelse 0, numberAfter(tail, "\"held_stores\":") orelse 0, numberAfter(tail, "\"transport_zc_write_extractions\":") orelse 0 });
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

test "smoke: KERNEL AIO on the plain mount (no shim): io_submit of 8 × 4 KiB PWRITEs into a fresh file (the inline → striped promotion under concurrent writes), again over the existing file, under O_DIRECT; every io_getevents res the page, byte-exact read-back — on a set WITHOUT a cache path (every write beyond inline striped) and WITH one" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    {
        const rig = try Rig.init(testing.allocator, io);
        defer rig.deinit();
        try kernelAioStage(rig, false);
    }
    {
        const rig = try Rig.init(testing.allocator, io);
        defer rig.deinit();
        try kernelAioStage(rig, true);
    }
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
