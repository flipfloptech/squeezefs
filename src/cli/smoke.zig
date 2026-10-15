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
//! claimed size is exact or a hole), `squeezefs fsck --offline --scrub`
//! (OUR oracle, C1–C17) is clean after every kill, and the mount-time C8
//! oracle and the C2 reconcile report zero drift on the remount.
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
//! transaction atomic, torn-write immune — and after `kill -9` the tree is
//! judged at the WRITE(2) grain (charter §1.10 "zero acked loss" is a
//! promise about syscalls, not files): every write tar was acked on is
//! present whole; a file tar was NOT acked on is whole (landed, the reply
//! lost), empty (its CREATE landed, its WRITE never issued) or cut exactly
//! at one of tar's own write(2) boundaries — GNU tar extracts an entry
//! whose data straddles a 10,240 B record as TWO writes (`TarWriteGrain`);
//! nothing is ever torn inside a write, nothing acked is lost. The strict
//! cadence (`SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`: the barrier before every
//! ack) pins the same contract through the lane's other arm. Linux only.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const clock = @import("../core/clock.zig");
const inos = @import("../meta/inos.zig");
const log = @import("../core/log.zig");
const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const json_scan = @import("../core/json_scan.zig");
const handler = @import("../fuse/handler.zig");
const transport = @import("../fuse/transport.zig");
const fuse_geometry = @import("../fuse/geometry.zig");
const status_verb = @import("status.zig");
const cli_supervisor = @import("supervisor.zig");
const uring_fs = @import("../io/uring_fs.zig");
const superblock = @import("../meta/superblock.zig");
const ledger = @import("../meta/ledger.zig");
const device_loader = @import("../meta/device_loader.zig");
const node_seq = @import("../meta/node_seq.zig");
const proc = @import("../meta/guard/proc.zig");
const lease_clock = @import("../meta/guard/lease_clock.zig");
const extent_grant = @import("../meta/sym/extent_grant.zig");
const cr = @import("../meta/sym/control_records.zig");
const route_table = @import("../meta/routing/route_table.zig");
const appender_page = @import("../meta/sym/appender_page.zig");
const membership = @import("../meta/sym/membership.zig");
const sym_xv = @import("../meta/sym/xv.zig");
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
/// The file in flight at the kill is usually the newest visible one, f199
/// — a two-write entry under GNU tar's record grain (`TarWriteGrain`), so
/// the kill lands between an entry's two writes far more often than the
/// 75-in-1,500 chance.
const kill_after_files: usize = 200;
/// The tar block (POSIX ustar): headers and data padding are 512 B units;
/// `tar -R` counts in them.
const tar_block: u64 = 512;

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
            if (std.mem.startsWith(u8, line, "error(") or std.mem.startsWith(u8, line, "warning(") or std.mem.indexOf(u8, line, "TRIPWIRE") != null) {
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
        return self.formatSetSized(cache, extra, data_len);
    }

    /// `formatSetWith` over data volumes of `data_bytes` each (a row that
    /// fills the set to ENOSPC wants it small).
    fn formatSetSized(self: *Rig, cache: bool, extra: []const []const u8, data_bytes: u64) !void {
        try self.createMetaVolume();
        for ([_][]const u8{ self.d0, self.d1 }) |dp| {
            const f = try Io.Dir.cwd().createFile(self.io, dp, .{ .truncate = true });
            defer f.close(self.io);
            try f.setLength(self.io, data_bytes);
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

    /// The mount's `.stats` by its absolute path (`Peer.stats`'s law),
    /// parsed STRICTLY: every smoke row that reads the document is a rail
    /// that it is ONE document every tool can parse.
    fn stats(self: *Rig) ![]u8 {
        var script: [160]u8 = undefined;
        const text = try self.shOk(self.work, try std.fmt.bufPrint(&script, "cat {s}/.stats", .{self.mp}));
        errdefer self.gpa.free(text);
        try expectStrictStats(self.gpa, text);
        return text;
    }

    /// `stats` that answers null instead of failing (the poll's read).
    fn statsTry(self: *Rig) !?[]u8 {
        var script: [160]u8 = undefined;
        const r = try self.sh(self.work, try std.fmt.bufPrint(&script, "cat {s}/.stats", .{self.mp}));
        defer self.gpa.free(r.stderr);
        if (r.term.success()) {
            errdefer self.gpa.free(r.stdout);
            try expectStrictStats(self.gpa, r.stdout);
            return r.stdout;
        }
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
        const term = try c.wait(self.io);
        if (!self.killed) printUncleanExit("A", term, self.readLog() catch null, self.gpa);
        return term;
    }

    fn umount(self: *Rig) !void {
        const r = try self.squeezefs(&.{ "umount", self.mp });
        defer self.freeRun(r);
        // An orderly unmount is silent; a fallback says why even at rc 0.
        if (!r.term.success() or r.stderr.len != 0) std.debug.print("\numount ({f}): {s}\n", .{ r.term, r.stderr });
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

/// A daemon the test did not kill ended by a signal: the signal's name and
/// the log's tail — a panic's trace starts no `error(` line and the log
/// dies with the work dir, so this is the only record a failing row keeps
/// (an `exit N` is a refusal the rows judge themselves). `log_text`
/// (owned) is freed.
fn printUncleanExit(name: []const u8, term: std.process.Child.Term, log_text: ?[]u8, gpa: std.mem.Allocator) void {
    defer if (log_text) |t| gpa.free(t);
    if (term == .exited) return;
    const text = log_text orelse "";
    const tail_bytes: usize = 6 << 10;
    std.debug.print("\n--- daemon {s} ended {f}; its log's last {d} B ---\n{s}\n--- end of daemon {s}'s log ---\n", .{ name, term, @min(text.len, tail_bytes), text[text.len - @min(text.len, tail_bytes) ..], name });
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
        return testkit.skip(@src(), .external_interference, "a daemon died of a SIGKILL this test never sent (a concurrent runner swept the venue)");
    }
    std.debug.print("\n--- daemon {s} exited ({f}) while waiting for '{s}' ---\n{s}\n", .{ name, term, waiting_for, log_text orelse "" });
    return error.DaemonExited;
}

/// Root, `/dev/fuse`, `CAP_SYS_ADMIN`, the binary beside us.
/// The venue law (`testkit.venue.mount`): the binary beside us, root,
/// `/dev/fuse`, CAP_SYS_ADMIN — each absence its own ledgered class.
const stats_doc = @import("../stats/stats.zig");

/// A `.stats` document a STRICT parser accepts — no key repeats inside its
/// object (`DuplicateField`); the repeated keys named by path when one does.
fn expectStrictStats(gpa: std.mem.Allocator, text: []const u8) !void {
    var parsed = std.json.parseFromSlice(std.json.Value, gpa, text, .{ .duplicate_field_behavior = .@"error" }) catch |e| {
        var dups: std.ArrayList([]u8) = .empty;
        defer {
            for (dups.items) |d| gpa.free(d);
            dups.deinit(gpa);
        }
        stats_doc.duplicateKeys(gpa, text, &dups) catch {};
        std.debug.print("\n--- .stats refused by a strict parser ({t}; {d} B):", .{ e, text.len });
        for (dups.items) |d| std.debug.print(" {s}", .{d});
        std.debug.print(" ---\n", .{});
        return e;
    };
    parsed.deinit();
}

fn venueMissing(io: Io) ?testkit.Why {
    return testkit.venue.mount(io, &.{"squeezefs"});
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
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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
        try testing.expect(std.mem.indexOf(u8, tail, "\"transport_park_tick_commit_lost_wakes\":0,") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "\"transport_park_tick_cqe_lost_wakes\":0,") != null);
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
        try testing.expect(std.mem.indexOf(u8, text[ready_at..], "open_count_stranded=0 transport_requests_abandoned=0 tick_commit_lost_wakes=0 tick_cqe_lost_wakes=0 ") != null);
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
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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
    /// This daemon's environment when it differs from the rig's (the
    /// MutualTls rows: every node its own certificate and key).
    env: ?*const std.process.Environ.Map = null,
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
            if (std.mem.startsWith(u8, line, "error(") or std.mem.startsWith(u8, line, "warning(") or std.mem.indexOf(u8, line, "TRIPWIRE") != null) {
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
        self.child = try std.process.spawn(rig.io, .{ .argv = argv[0..n], .environ_map = self.env orelse &rig.env, .stdin = .ignore, .stdout = .{ .file = self.log_file.? }, .stderr = .{ .file = self.log_file.? } });
        self.pid = self.child.?.id.?;
        self.killed = false;
        var tag_buf: [32]u8 = undefined;
        try writePidFile(rig.io, rig.work, try std.fmt.bufPrint(&tag_buf, "daemon-{s}", .{self.name}), self.pid);
    }

    fn readLog(self: *Peer) ![]u8 {
        return Io.Dir.cwd().readFileAlloc(self.rig.io, self.log_path, self.rig.gpa, .limited(16 << 20));
    }

    /// `onMount` under `timeout -s KILL <secs>` (`Rig.onMountTimed`'s law).
    fn onMountTimed(self: *Peer, script: []const u8, secs: u32) ![]u8 {
        const rig = self.rig;
        var buf: [16]u8 = undefined;
        const s = try std.fmt.bufPrint(&buf, "{d}", .{secs});
        const r = try std.process.run(rig.gpa, rig.io, .{ .argv = &.{ "timeout", "-s", "KILL", s, "sh", "-e", "-c", script, "smoke", rig.work }, .cwd = .{ .path = self.mp }, .environ_map = &rig.env });
        defer rig.gpa.free(r.stderr);
        if (!r.term.success()) {
            std.debug.print("\n--- {s}: mount script failed ({f}) ---\n{s}\n--- stdout ---\n{s}\n--- stderr ---\n{s}\n", .{ self.name, r.term, script, r.stdout, r.stderr });
            rig.gpa.free(r.stdout);
            return error.ScriptFailed;
        }
        return r.stdout;
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
        const text = try self.rig.shOk(self.rig.work, try std.fmt.bufPrint(&script, "cat {s}/.stats", .{self.mp}));
        errdefer self.rig.gpa.free(text);
        try expectStrictStats(self.rig.gpa, text);
        return text;
    }

    /// `stats` that answers null instead of failing: the root's attributes
    /// are its holder's, and a parked joiner's holder is dead — the path
    /// walk to `.stats` answers EAGAIN until the kernel's cached root
    /// attributes serve it (the poll's read).
    fn statsTry(self: *Peer) !?[]u8 {
        var script: [160]u8 = undefined;
        const r = try self.rig.sh(self.rig.work, try std.fmt.bufPrint(&script, "cat {s}/.stats", .{self.mp}));
        defer self.rig.gpa.free(r.stderr);
        if (r.term.success()) {
            errdefer self.rig.gpa.free(r.stdout);
            try expectStrictStats(self.rig.gpa, r.stdout);
            return r.stdout;
        }
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
        // An orderly unmount is silent; a fallback says why even at rc 0.
        if (!r.term.success() or r.stderr.len != 0) std.debug.print("\numount {s} ({f}): {s}\n", .{ self.name, r.term, r.stderr });
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
        const term = try c.wait(self.rig.io);
        if (!self.killed) printUncleanExit(self.name, term, self.readLog() catch null, self.rig.gpa);
        return term;
    }

    /// `waitExit` judged: a clean exit 0, else the log (a panic's trace
    /// starts no `error(` line, so `printAlerts` would miss it).
    fn expectCleanExit(self: *Peer, waiting_for: []const u8) !void {
        const term = try self.waitExit();
        if (term == .exited and term.exited == 0) return;
        return judgeDeath(self.name, term, self.killed, waiting_for, self.readLog() catch null, self.rig.gpa);
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
/// The format default data block (4 MiB) in KiB — every rig set's.
const matrix_block_kib: u64 = 4096;

fn matrixCreateScript(gpa: std.mem.Allocator) ![]u8 {
    var w: std.Io.Writer.Allocating = .init(gpa);
    defer w.deinit();
    try w.writer.print("for d in $(seq 0 {d}); do mkdir b$d; done; for d in $(seq 0 {d}); do for i in $(seq 0 {d}); do printf 'b%s-f%s\\n' $d $i > b$d/f$i; done; done; ls b0 | wc -l", .{ matrix_dirs - 1, matrix_dirs - 1, matrix_files_per_dir - 1 });
    return w.toOwnedSlice();
}

test "smoke: THE TWO-DAEMON MATRIX — mount A (the manager) → mount B as a SECOND PROCESS joined over TCP (no flock, the region over the wire, the page as its ledger) → `appenders`/`clients` show both → B creates 2,000 files in its own directories (0 lease conflicts, rotor mints) → A reads B's files under tokens (grants; the second read is served from the cache; a write on B recalls and A's third read is fresh) → `tar -x` on B into a directory A owns (cross-owner intents, 0 stuck, closure) → `chmod` from A on B's file (the record ship; B's stat sees it) → a 200 MiB `dd` on B (every block a ranged grant pulled from A; `df` per the rule on both; the sha round trip from A; `rm` ships the frees to A) → A writes 8 MiB into B's file (the promotion and every block-map merge SHIP as layout publishes to B, the size claim as one setattr; the sha equal on both) → closure + invariant tripwires 0 on both → umount B (page Free, the member row gone) → umount A" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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
    // A WRITES B's file AFTER the df arithmetic above (the holder's own
    // carve for the write counts as used from B's view): charter §5.5 "the
    // layout publish" of a
    // foreign-slot file): 8 MiB from A into b2/f9 — the promotion of B's
    // inline record and every block-map merge SHIP as layout publishes to
    // B (B stages the record and its refs in ITS tree, recalls A's token),
    // the size claim ships as one setattr; B reads the bytes back
    // byte-exact and A reads them under B's token.
    {
        const out = try rig.onMount("dd if=/dev/urandom of=b2/f9 bs=1M count=8 conv=notrunc status=none && sync b2/f9 && sha256sum b2/f9 | cut -d' ' -f1 && stat -c %s b2/f9");
        defer gpa.free(out);
        var lines = std.mem.splitScalar(u8, out, '\n');
        const sha_a = lines.next() orelse return error.ShaMissing;
        try testing.expectEqualStrings("8388608", std.mem.trim(u8, lines.next() orelse "", " \n"));
        const from_b = try b.onMount("sha256sum b2/f9 | cut -d' ' -f1 && stat -c %s b2/f9");
        defer gpa.free(from_b);
        var bl = std.mem.splitScalar(u8, from_b, '\n');
        try testing.expectEqualStrings(sha_a, bl.next() orelse "");
        try testing.expectEqualStrings("8388608", std.mem.trim(u8, bl.next() orelse "", " \n"));
        const ta = try rig.stats();
        defer gpa.free(ta);
        const tb = try b.stats();
        defer gpa.free(tb);
        const shipped = numberAfter(ta, "\"layout_map_merges_shipped\":") orelse 0;
        const served = numberAfter(tb, "\"layout_publishes_served\":") orelse 0;
        try testing.expect(shipped >= 2);
        try testing.expectEqual(shipped, numberAfter(ta, "\"foreign_publish_ships\":") orelse 0);
        try testing.expectEqual(shipped, served);
        try testing.expectEqual(served, numberAfter(tb, "\"foreign_publish_served\":") orelse 0);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"foreign_publish_refusals\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"foreign_publish_kvmap_refusals\":"));
        try testing.expect((numberAfter(ta, "\"fuse_write_foreign_striped\":") orelse 0) >= 1);
        std.debug.print("smoke: matrix: A wrote 8 MiB into B's file — layout publishes shipped {d} (= served at B {d}), foreign striped writes {d}, record ships {d}; sha equal on both\n", .{ shipped, served, numberAfter(ta, "\"fuse_write_foreign_striped\":") orelse 0, numberAfter(ta, "\"record_ships\":") orelse 0 });
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

test "smoke: INVALIDATION COHERENCE ACROSS DAEMONS (decision 0003 §AB.2, §AG.2) — every TTL class at 60 s on both (a stale entry would outlive the row); B evicts every token entry right after its serve (`SQUEEZEFS_TEST_TOKEN_EVICT_AFTER_SERVE`: B holds NO entry when A's change lands): mount A, B JOINS (A's TTL law flips to PEERED, logged) → A caches a NEGATIVE `d/x`, B creates it in A's directory (a served step at A) ⇒ A sees it at once → B caches a negative `d/y`, A creates it ⇒ once B's notifier drained, B sees it (the recalled name) → B caches `d/x`, A unlinks it ⇒ B misses it → B caches `d/y` and a negative `d/z`, A renames y → z ⇒ B sees both → B caches z's mode, A chmods it ⇒ B's stat is fresh → B counts the recalled names and its kernel-pinned drops, no epoch owed on either" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    // Every class far past the row's length: only an invalidation can make
    // a cached entry fresh here.
    for ([_][]const u8{ "SQUEEZEFS_FUSE_ATTR_TTL_MS", "SQUEEZEFS_FUSE_ENTRY_TTL_MS", "SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS", "SQUEEZEFS_FUSE_NEGATIVE_TTL_MS" }) |k| try rig.env.put(k, "60000");
    try rig.formatSet(true);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const mk = try rig.onMount("mkdir d && echo ok");
    gpa.free(mk);
    var env_b = try envWith(gpa, &rig.env, &.{.{ "SQUEEZEFS_TEST_TOKEN_EVICT_AFTER_SERVE", "1" }});
    defer env_b.deinit();
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.env = &env_b;
    try b.spawn(&.{});
    _ = try b.awaitReady();
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "a member joined — the kernel's TTLs take the peered classes") != null);
    }
    // A served step at the holder: A's kernel held the negative.
    {
        const before = try rig.onMount("test -e d/x && echo yes || echo no");
        defer gpa.free(before);
        try testing.expectEqualStrings("no\n", before);
        const made = try b.onMount("touch d/x && echo ok");
        gpa.free(made);
        // A's own kernel is invalidated by its notifier thread, which the
        // served step's ack does not wait for (0003 §AB.2 (2)): the row
        // waits for the observable within 2 s — a lost invalidation keeps
        // the negative for the whole 60 s TTL.
        const after = try rig.onMount("for i in $(seq 100); do test -e d/x && { echo yes; exit 0; }; sleep 0.02; done; echo no");
        defer gpa.free(after);
        try testing.expectEqualStrings("yes\n", after);
    }
    // The recalled name: B's kernel held the negative; B's cache dropped
    // the directory's entry right after serving it (the seam), so only
    // the §AG.2 pin keeps A recalling B. A's create returns once B acked,
    // B's names queued before the ack and written by B's notifier after
    // it: the step waits for that notifier, then judges at once.
    {
        const before = try b.onMount("test -e d/y && echo yes || echo no");
        defer gpa.free(before);
        try testing.expectEqualStrings("no\n", before);
        const made = try rig.onMount("touch d/y && echo ok");
        gpa.free(made);
        try awaitNotifierDrained(b);
        const after = try b.onMount("test -e d/y && echo yes || echo no");
        defer gpa.free(after);
        try testing.expectEqualStrings("yes\n", after);
    }
    // An unlink: B's kernel held the positive.
    {
        const before = try b.onMount("test -e d/x && echo yes || echo no");
        defer gpa.free(before);
        try testing.expectEqualStrings("yes\n", before);
        const gone = try rig.onMount("rm d/x && echo ok");
        gpa.free(gone);
        try awaitNotifierDrained(b);
        const after = try b.onMount("test -e d/x && echo yes || echo no");
        defer gpa.free(after);
        try testing.expectEqualStrings("no\n", after);
    }
    // A rename: both names.
    {
        const before = try b.onMount("test -e d/y && echo yes || echo no; test -e d/z && echo yes || echo no");
        defer gpa.free(before);
        try testing.expectEqualStrings("yes\nno\n", before);
        const moved = try rig.onMount("mv d/y d/z && echo ok");
        gpa.free(moved);
        try awaitNotifierDrained(b);
        const after = try b.onMount("test -e d/y && echo yes || echo no; test -e d/z && echo yes || echo no");
        defer gpa.free(after);
        try testing.expectEqualStrings("no\nyes\n", after);
    }
    // A setattr: B's kernel held the mode.
    {
        const before = try b.onMount("stat -c %a d/z");
        defer gpa.free(before);
        try testing.expectEqualStrings("644\n", before);
        const ch = try rig.onMount("chmod 600 d/z && echo ok");
        gpa.free(ch);
        try awaitNotifierDrained(b);
        const after = try b.onMount("stat -c %a d/z");
        defer gpa.free(after);
        try testing.expectEqualStrings("600\n", after);
    }
    {
        const tb = try b.stats();
        defer gpa.free(tb);
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        const names_b = numberAfter(tb, "\"dlm_token_kernel_name_invals\":") orelse 0;
        try testing.expect(names_b >= 4);
        // Every directory entry B served was dropped, never released.
        try testing.expect((numberAfter(tb, "\"dlm_token_kernel_pinned_drops\":") orelse 0) >= 4);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"transport_notify_epochs_owed\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"transport_notify_epochs_owed\":"));
        std.debug.print("smoke: coherence: B's recalled names {d}, epochs {d}; A's epochs sent {d}\n", .{ names_b, numberAfter(tb, "\"dlm_token_kernel_epochs\":") orelse 0, numberAfter(ta, "\"transport_notify_epochs_sent\":") orelse 0 });
    }
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

/// A recall's names are queued on the reader's sideband before its ack and
/// written by the notifier after it (decision 0003 §AG.2, "at notify
/// speed"): wait — bounded — until the peer's sideband owes the kernel no
/// write. The bound is the notifier's round under load, never a TTL.
fn awaitNotifierDrained(p: *Peer) !void {
    const until = clock.monotonicNs() + 5 * std.time.ns_per_s;
    var pending: ?u64 = null;
    while (clock.monotonicNs() < until) {
        const t = try p.stats();
        defer p.rig.gpa.free(t);
        pending = numberAfter(t, "\"transport_notify_pending\":");
        if (pending == @as(?u64, 0)) return;
    }
    std.debug.print("\n{s}: the notifier still owed {?d} write(s) after 5 s\n", .{ p.name, pending });
    return error.NotifierNotDrained;
}

/// The grant-plane row of `vol_tag` in a `.stats` text (`block_grant_holder`
/// / `block_grant_writer`): the JSON from its `"vol_tag"` on, or null.
fn grantRowOf(stats_text: []const u8, family: []const u8, tag_hex: []const u8) ?[]const u8 {
    const fam = std.mem.indexOf(u8, stats_text, family) orelse return null;
    var needle: [40]u8 = undefined;
    const n = std.fmt.bufPrint(&needle, "\"vol_tag\":\"{s}\"", .{tag_hex}) catch return null;
    const i = std.mem.indexOfPos(u8, stats_text, fam, n) orelse return null;
    return stats_text[i..];
}

test "smoke: THE ONLINE VOLUME ROW ACROSS DAEMONS — mount A (the manager) + B JOINED → `volume add-data --no-rebalance <A> d2` → B learns the generation bump at its next renewal (NO remount): the record pulled over the wire, d2 attached and armed on B (data_volumes 3, writer volumes 3, its generation = A's) → B writes 4 MiB files until one's stripe unit hashes onto d2 (B's writer row on d2 consumed grants A's holder row carved) and A reads every file back byte-exact → B removes them (the frees ship to A) → `volume remove-data <A> d2` → B flips d2 Draining at the bump and returns its window (24 more files on B place nothing there) → the drain retires d2 → B flips it Retired → closure on both → umount B, A" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    // fsync promotes a staged file (the A/B lever's ON arm): each file
    // lands its block through the router's placement at its own fsync, so
    // the row reads placement per file instead of at B's dismount.
    try rig.env.put("SQUEEZEFS_FSYNC_PROMOTE_STAGED", "1");
    // A holds the online arm of d2 until its holder answered a data-plane
    // verb for a volume it has not armed (or T_owner passed): a joiner that
    // learnt d2 before A's arm would ask A for it inside the window — A's
    // log names which ended the hold, the closure judges A's answers.
    try rig.env.put("SQUEEZEFS_TEST_ONLINE_ARM_HOLD", "1");
    try rig.formatSet(true);
    const d2 = try std.fmt.allocPrintSentinel(gpa, "{s}/d2.img", .{rig.work}, 0);
    defer gpa.free(d2);
    {
        const f = try Io.Dir.cwd().createFile(io, d2, .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, data_len);
    }
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{});
    _ = try b.awaitReady();
    const gen0 = blk: {
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expectEqual(@as(?u64, 2), numberAfter(tb, "\"data_volumes\":"));
        break :blk numberAfter(tb, "\"volume_config_generation\":") orelse return error.NoGeneration;
    };
    // 1. add-data on A (the manager), no rebalance: the row watches B's
    //    OWN placement onto the new volume.
    var tag_buf: [20]u8 = undefined;
    var gen_after: u64 = 0;
    {
        const r = try rig.squeezefs(&.{ "volume", "add-data", "--no-rebalance", rig.mp, d2 });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nadd-data: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "live") != null);
        const i = std.mem.indexOf(u8, r.stdout, "vol-") orelse return error.NoVolumeId;
        @memcpy(&tag_buf, r.stdout[i .. i + 20]);
        gen_after = numberAfter(r.stdout, "config generation ") orelse return error.NoGeneration;
    }
    try testing.expectEqual(gen0 + 1, gen_after);
    const tag_id: []const u8 = &tag_buf;
    const tag_hex = tag_buf[4..20];
    // 2. B learns at its next renewal (T_owner 3 s ⇒ a 265 ms cadence):
    //    the grant carried the generation, B pulled the record, attached
    //    d2 as ordinal 2 and armed its writer row — no remount.
    {
        const deadline = clock.monotonicNs() + 30 * std.time.ns_per_s;
        while (true) {
            const probe = try b.stats();
            defer gpa.free(probe);
            if ((numberAfter(probe, "\"volume_config_generation\":") orelse 0) == gen_after) {
                // A render reads the plane's family before the lifecycle's:
                // the words are judged on a snapshot taken AFTER the bump.
                const tb = try b.stats();
                defer gpa.free(tb);
                try testing.expectEqual(@as(?u64, 3), numberAfter(tb, "\"data_volumes\":"));
                try testing.expectEqual(@as(?u64, 3), numberAfter(tb, "\"block_grant_writer_volumes\":"));
                try testing.expectEqual(@as(?u64, 1), numberAfter(tb, "\"volume_online_attached\":"));
                try testing.expectEqual(@as(?u64, 1), numberAfter(tb, "\"volume_config_pulls\":"));
                try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"volume_config_pull_failures\":"));
                try testing.expect(grantRowOf(tb, "\"block_grant_writer\":", tag_hex) != null);
                break;
            }
            if (clock.monotonicNs() >= deadline) {
                std.debug.print("\n--- B never learnt generation {d} ---\n{s}\n", .{ gen_after, probe });
                return error.JoinerNeverLearnt;
            }
            sleepMs(100);
        }
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "joined this mount ONLINE as ordinal 2 (active)") != null);
        // The generation moves at A's step 7 (decision 0003 §AI.3): nothing
        // asked A for d2 before its arm.
        const at = try rig.readLog();
        defer gpa.free(at);
        try testing.expect(std.mem.indexOf(u8, at, "held until T_owner passed with no data-plane verb for an unarmed volume") != null);
    }
    // 3. B writes 4 MiB files (one block each, promoted at their fsync
    //    through the router's round-robin band of three) until d2 holds
    //    one (bounded at 40): every block there is minted from a grant B's
    //    writer row pulled from A's holder row on d2; A reads every file
    //    back.
    var consumed_d2: u64 = 0;
    var files: u32 = 0;
    while (files < 40) {
        var script: [256]u8 = undefined;
        const out = try b.onMount(try std.fmt.bufPrint(&script, "mkdir -p b-data && for n in $(seq {d} {d}); do dd if=/dev/urandom of=b-data/f$n bs=1M count=4 conv=fsync status=none; done && echo ok", .{ files, files + 7 }));
        gpa.free(out);
        files += 8;
        const tb = try b.stats();
        defer gpa.free(tb);
        const row_b = grantRowOf(tb, "\"block_grant_writer\":", tag_hex) orelse return error.NoWriterRow;
        consumed_d2 = numberAfter(row_b, "\"consumed\":") orelse 0;
        if (consumed_d2 >= 1) break;
    }
    try testing.expect(consumed_d2 >= 1);
    {
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        const row_a = grantRowOf(ta, "\"block_grant_holder\":", tag_hex) orelse return error.NoHolderRow;
        const population_d2 = numberAfter(row_a, "\"population\":") orelse 0;
        try testing.expect(population_d2 >= consumed_d2);
        const from_b = try b.onMount("sha256sum b-data/f* | sort -k2");
        defer gpa.free(from_b);
        const from_a = try rig.onMount("sha256sum b-data/f* | sort -k2");
        defer gpa.free(from_a);
        try testing.expectEqualStrings(from_b, from_a);
        std.debug.print("\nsmoke[online-volume]: B learnt generation {d} at its renewal; d2 armed online as ordinal 2; {d} file(s) on B put {d} block(s) on d2 (A's holder population {d}); every sha from A equal\n", .{ gen_after, files, consumed_d2, population_d2 });
    }
    // 4. remove-data d2 on A: B flips it Draining at the bump (its window
    //    on d2 returns to A) and places nothing more there; the drain
    //    retires it; B flips it Retired. B's files go first: the mover's
    //    census walks the manager's OWN slot trees, so a drain over blocks
    //    a joiner's files reference waits on the cross-owner census (owed,
    //    stated in the evidence) — the frees ship to A at B's cadence.
    var drain_id_buf: [jobs_records.JOB_ID_TEXT_LEN]u8 = undefined;
    {
        const rm = try b.onMount("rm -f b-data/f*; echo ok");
        gpa.free(rm);
        const deadline = clock.monotonicNs() + 60 * std.time.ns_per_s;
        while (true) {
            const tb = try b.stats();
            defer gpa.free(tb);
            if ((numberAfter(tb, "\"free_ship_freed\":") orelse 0) >= @as(u64, files)) break;
            if (clock.monotonicNs() >= deadline) return error.FreesNeverShipped;
            sleepMs(200);
        }
    }
    {
        const r = try rig.squeezefs(&.{ "volume", "remove-data", rig.mp, tag_id });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nremove-data: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "Draining") != null);
        @memcpy(&drain_id_buf, jobIdIn(r.stdout) orelse return error.NoDrainJob);
    }
    {
        const deadline = clock.monotonicNs() + 30 * std.time.ns_per_s;
        while (true) {
            const tb = try b.stats();
            defer gpa.free(tb);
            if ((numberAfter(tb, "\"volume_config_generation\":") orelse 0) == gen_after + 1) {
                const again = try b.stats();
                defer gpa.free(again);
                try testing.expectEqual(@as(?u64, 1), numberAfter(again, "\"volume_online_flips\":"));
                break;
            }
            if (clock.monotonicNs() >= deadline) return error.JoinerNeverLearntDraining;
            sleepMs(100);
        }
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "active → draining") != null);
        // Draining is ineligible for placement: 24 more files, none on d2
        // (its window returned: nothing remains installed there).
        const out = try b.onMount("for n in $(seq 100 123); do dd if=/dev/urandom of=b-data/g$n bs=1M count=4 conv=fsync status=none; done && echo ok");
        defer gpa.free(out);
        const tb = try b.stats();
        defer gpa.free(tb);
        const row_b = grantRowOf(tb, "\"block_grant_writer\":", tag_hex) orelse return error.NoWriterRow;
        try testing.expectEqual(consumed_d2, numberAfter(row_b, "\"consumed\":") orelse 0);
        try testing.expectEqual(@as(u64, 0), numberAfter(row_b, "\"window_remaining\":") orelse 99);
    }
    try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, &drain_id_buf, 300 * std.time.ns_per_s));
    {
        const deadline = clock.monotonicNs() + 30 * std.time.ns_per_s;
        while (true) {
            const tb = try b.stats();
            defer gpa.free(tb);
            if ((numberAfter(tb, "\"volume_config_generation\":") orelse 0) == gen_after + 2) {
                const again = try b.stats();
                defer gpa.free(again);
                try testing.expectEqual(@as(?u64, 2), numberAfter(again, "\"volume_online_flips\":"));
                try testing.expectEqual(@as(?u64, 0), numberAfter(again, "\"volume_config_pull_failures\":"));
                break;
            }
            if (clock.monotonicNs() >= deadline) return error.JoinerNeverLearntRetired;
            sleepMs(100);
        }
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "draining → retired") != null);
        // B's later files read equal on both.
        const from_b = try b.onMount("sha256sum b-data/g* | sort -k2");
        defer gpa.free(from_b);
        const from_a = try rig.onMount("sha256sum b-data/g* | sort -k2");
        defer gpa.free(from_a);
        try testing.expectEqualStrings(from_b, from_a);
        std.debug.print("smoke[online-volume]: remove-data on A → B flipped d2 draining at generation {d} (its window returned; 24 more files placed nothing there) and retired at {d}; B's files read equal on both\n", .{ gen_after + 1, gen_after + 2 });
    }
    // Closure on both daemons.
    {
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        try expectMustStayZero(ta);
        const tb = try b.stats();
        defer gpa.free(tb);
        try expectMustStayZero(tb);
    }
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

// ---------------------------------------------------------------------------
// `volume enable-symmetric` (charter §5.4, §8): the flat class converted in
// place, then the ordinary symmetric posture over it.
// ---------------------------------------------------------------------------

/// The flat tree the conversion must carry over: 4 directories × 50 files.
const convert_dirs: usize = 4;
const convert_files_per_dir: usize = 50;

fn convertTreeScript(gpa: std.mem.Allocator) ![]u8 {
    var w: std.Io.Writer.Allocating = .init(gpa);
    defer w.deinit();
    try w.writer.print("for d in $(seq 0 {d}); do mkdir f$d; for i in $(seq 0 {d}); do printf 'f%s-%s\\n' $d $i > f$d/n$i; done; done; printf 'inline\\n' > top; find . -path ./.stats -prune -o -type f -print | wc -l", .{ convert_dirs - 1, convert_files_per_dir - 1 });
    return w.toOwnedSlice();
}

fn convertVerifyScript(gpa: std.mem.Allocator) ![]u8 {
    var w: std.Io.Writer.Allocating = .init(gpa);
    defer w.deinit();
    try w.writer.print("for d in $(seq 0 {d}); do for i in $(seq 0 {d}); do test \"$(cat f$d/n$i)\" = \"f$d-$i\"; done; done; cat top; find . -path ./.stats -prune -o -type f -print | wc -l", .{ convert_dirs - 1, convert_files_per_dir - 1 });
    return w.toOwnedSlice();
}

test "smoke: `volume enable-symmetric` — format --single-writer (meta + 2 data, a cache path) → mount, a tree of 201 files, the verb REFUSES the mounted volume (exit 1, MOUNTED) → clean umount → the verb converts (the link claimed, the ring re-based, the stamp) → a second run refuses (already symmetric) → mount A: the MANAGER posture (manager_lease held, symmetric_meta 1, appenders 1 page mgr), every file intact → B JOINS as a second process (appender 1), creates in its own slots, A reads B's files under tokens → umount B, umount A → `fsck --offline --scrub` 0 findings" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSetWith(true, &.{"--single-writer"});
    // The flat mount and its tree.
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    {
        const script = try convertTreeScript(gpa);
        defer gpa.free(script);
        const out = try rig.onMount(script);
        defer gpa.free(out);
        try testing.expectEqualStrings("201\n", out);
        const tail = try rig.stats();
        defer gpa.free(tail);
        try testing.expect(std.mem.indexOf(u8, tail, "\"manager_lease\":\"solo\"") != null);
    }
    // The verb against the MOUNTED volume: the flock refuses, nothing written.
    {
        const r = try rig.squeezefs(&.{ "volume", "enable-symmetric", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "MOUNTED") != null);
        std.debug.print("\nsmoke: enable-symmetric on the mounted volume: {s}", .{r.stderr});
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    // The conversion, then its idempotent refusal.
    {
        const r = try rig.squeezefs(&.{ "volume", "enable-symmetric", rig.meta_uri });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nenable-symmetric: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "SYMMETRIC_PLANE enabled") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "1 member(s) converted, 0 already symmetric") != null);
        std.debug.print("smoke: {s}", .{r.stdout});
    }
    {
        const r = try rig.squeezefs(&.{ "volume", "enable-symmetric", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "already carries SYMMETRIC_PLANE") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "0 member(s) converted, 1 already symmetric") != null);
    }
    // `appenders` offline: appender 0's Free page on the converted slots.
    {
        const r = try rig.squeezefs(&.{ "appenders", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "1 appender page(s)") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "free") != null);
    }
    // Mount A: the symmetric manager; the tree intact; the clean-unmount
    // verdict on the re-based ring.
    const mark = try rig.logLen();
    try rig.spawnMount(&.{});
    const ready = try rig.awaitReady(mark);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[mark..ready], ", clean unmount") != null);
        try testing.expect(std.mem.indexOf(u8, text[mark..ready], "appender 0 Live as the manager (term 1") != null);
        const script = try convertVerifyScript(gpa);
        defer gpa.free(script);
        const out = try rig.onMount(script);
        defer gpa.free(out);
        try testing.expectEqualStrings("inline\n201\n", out);
        const tail = try rig.stats();
        defer gpa.free(tail);
        try testing.expect(std.mem.indexOf(u8, tail, "\"manager_lease\":\"held\"") != null);
        try testing.expectEqual(@as(?u64, 1), numberAfter(tail, "\"symmetric_meta\":"));
        try testing.expect(std.mem.indexOf(u8, tail, "\"wire_listener_up\":1") != null);
    }
    // B joins over the wire and writes into its own slots; A reads them.
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{});
    _ = try b.awaitReady();
    {
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "mounted JOINED as appender 1") != null);
        const out = try b.onMount("mkdir bj && for i in $(seq 0 99); do printf 'bj-%s\\n' $i > bj/f$i; done; ls bj | wc -l");
        defer gpa.free(out);
        try testing.expectEqualStrings("100\n", out);
        const from_a = try rig.onMount("ls bj | wc -l; cat bj/f42; cat f2/n7");
        defer gpa.free(from_a);
        try testing.expectEqualStrings("100\nbj-42\nf2-7\n", from_a);
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"slot_lease_conflicts\":"));
        try testing.expect((numberAfter(tb, "\"dlm_token_grants\":") orelse 0) > 0);
        try expectMustStayZero(tb);
        const r = try rig.squeezefs(&.{ "appenders", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "2 appender page(s)") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "mgr") != null);
        std.debug.print("smoke: enable-symmetric: B joined the converted volume — token grants on B {d}, rotor mints {d}\n", .{ numberAfter(tb, "\"dlm_token_grants\":") orelse 0, numberAfter(tb, "\"rotor_mints\":") orelse 0 });
    }
    {
        const ta = try rig.stats();
        defer gpa.free(ta);
        try expectMustStayZero(ta);
        try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"membership_members\":"));
    }
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    {
        const after = try rig.onMount("ls bj | wc -l; cat bj/f42");
        defer gpa.free(after);
        try testing.expectEqualStrings("100\nbj-42\n", after);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the conversion and the two-daemon pass");
}

/// The rejoin row's cycles: each one a clean umount of B and its remount
/// under the same mount point while A stays up — the window the rejoin's
/// stale projection needs is B's remount inside A's 1 s cadence, which a
/// cycle hits most times; three cycles make a miss of the race vanishing.
const rejoin_cycles: usize = 3;

/// B's fill for one cycle under `r<k>/`: 1 KiB files (inline), 64 KiB files
/// (staged in B's ring, promoted at B's dismount into packs; A reads them
/// only after it — charter §6.1 "until promoted, other clients read
/// size-consistent zeros"), one fsync'd 8 MiB file (two blocks: striped) —
/// each cut from the rig's random image at its own offset — then every
/// path's sha256 appended to `$1/rejoin.sha` (relative to the mount root,
/// the list every later read checks).
fn rejoinFillScript(gpa: std.mem.Allocator, k: usize) ![]u8 {
    var w: std.Io.Writer.Allocating = .init(gpa);
    defer w.deinit();
    try w.writer.print("mkdir r{[k]d} && i=0; while [ $i -lt 40 ]; do dd if=\"$1/image\" of=r{[k]d}/s$i bs=1k skip=$(({[k]d} * 64 + i)) count=1 status=none; i=$((i + 1)); done; i=0; while [ $i -lt 8 ]; do dd if=\"$1/image\" of=r{[k]d}/p$i bs=64k skip=$(({[k]d} * 16 + i)) count=1 status=none; i=$((i + 1)); done; dd if=\"$1/image\" of=r{[k]d}/big bs=1M skip={[k]d} count=8 conv=fsync status=none; sha256sum r{[k]d}/* >> \"$1/rejoin.sha\"; ls r{[k]d} | wc -l", .{ .k = k });
    return w.toOwnedSlice();
}

test "smoke: A JOINER'S CLEAN UNMOUNT AND REJOIN while the manager stays up — mount A (the manager) → B JOINED writes inline, staged and striped files (sha list) → `umount` B (clean: every lease released, the page Free, every staged file promoted; A reads every file sha-equal) → B mounts again at the same mount point: JOINED as appender 1 again (its projection's rows naming appender 1 are a previous incarnation's — loaded unleased), every file of every cycle sha-equal on B, B writes more; three cycles → umount B, every file sha-equal on A → umount A → `fsck --offline --scrub` 0 findings" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.createImage();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{});
    _ = try b.awaitReady();
    {
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "mounted JOINED as appender 1") != null);
        const script = try rejoinFillScript(gpa, 0);
        defer gpa.free(script);
        const out = try b.onMount(script);
        defer gpa.free(out);
        try testing.expectEqualStrings("49\n", out);
    }
    var stale_rows: u64 = 0;
    for (1..rejoin_cycles + 1) |k| {
        try b.umount();
        try b.expectCleanExit("B's clean unmount before its rejoin");
        // B's dismount promoted its staged files: A reads every one.
        const from_a = try rig.onMount("sha256sum --quiet -c \"$1/rejoin.sha\" && echo ok");
        defer gpa.free(from_a);
        try testing.expectEqualStrings("ok\n", from_a);
        const mark = try b.logLen();
        try b.spawn(&.{});
        const ready = try b.awaitLog("ready: ", mark, ready_wait_ns);
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[mark..ready], "mounted JOINED as appender 1") != null);
        // Every file of every earlier cycle, then B's next fill.
        const back = try b.onMount("sha256sum --quiet -c \"$1/rejoin.sha\" && echo ok");
        defer gpa.free(back);
        try testing.expectEqualStrings("ok\n", back);
        const script = try rejoinFillScript(gpa, k);
        defer gpa.free(script);
        const out = try b.onMount(script);
        defer gpa.free(out);
        try testing.expectEqualStrings("49\n", out);
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"slot_lease_conflicts\":"));
        try expectMustStayZero(tb);
        const stale = numberAfter(tb, "\"slot_projection_stale_self_rows\":") orelse 0;
        stale_rows += stale;
        std.debug.print("smoke: rejoin cycle {d}: B JOINED as appender 1 again, {d} stale projection row(s) of its previous incarnation loaded unleased; {d} files sha-equal\n", .{ k, stale, k * 49 });
    }
    try b.umount();
    try b.expectCleanExit("B's last unmount");
    {
        const from_a = try rig.onMount("sha256sum --quiet -c \"$1/rejoin.sha\" && echo ok");
        defer gpa.free(from_a);
        try testing.expectEqualStrings("ok\n", from_a);
        const ta = try rig.stats();
        defer gpa.free(ta);
        try expectMustStayZero(ta);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the joiner's rejoin cycles");
    std.debug.print("smoke: rejoin: {d} cycles, {d} files, {d} stale projection row(s) loaded unleased in all\n", .{ rejoin_cycles, (rejoin_cycles + 1) * 49, stale_rows });
}

/// The member-scope row's residents: A's 64 KiB files (staged in A's ring,
/// one block each well under it) and B's.
const scope_a_files: usize = 200;
const scope_b_files: usize = 8;

/// The last line a bring-up logged for its staging ring under a root whose
/// path contains `member_root_tail` (`<cache>/member-<slot>`): its
/// `recovered N record(s)` count.
fn stagingRecoveredOf(log_text: []const u8, member_root_tail: []const u8) ?u64 {
    var it = std.mem.splitScalar(u8, log_text, '\n');
    var last: ?u64 = null;
    while (it.next()) |line| {
        if (std.mem.indexOf(u8, line, "staging ring ") == null or std.mem.indexOf(u8, line, member_root_tail) == null) continue;
        last = numberAfter(line, "recovered ");
    }
    return last;
}

test "smoke: TWO MOUNTS OF ONE SET ON ONE HOST, one cache path — each member binds its OWN staging root (0003 §T.1): A (the manager) stages 200 files, B joins beside A's LIVE ring and recovers 0 records; `staging list` shows both roots live; a SECOND SET formatted on the same cache path is refused at its mount naming A's live claim (never wiping A's ring); B's dismount promotes its own 8 files, A's dismount A's 200 — every one sha-equal after a remount; fsck --offline --scrub 0 findings" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.createImage();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    var script: std.Io.Writer.Allocating = .init(gpa);
    defer script.deinit();
    // A stages its residents (the ring holds them until A's dismount).
    {
        try script.writer.print("mkdir a && i=0; while [ $i -lt {d} ]; do dd if=\"$1/image\" of=a/p$i bs=64k skip=$i count=1 status=none; i=$((i + 1)); done; sha256sum a/* > \"$1/scope.sha\"; ls a | wc -l", .{scope_a_files});
        const out = try rig.onMount(script.written());
        defer gpa.free(out);
        try testing.expectEqual(scope_a_files, try std.fmt.parseInt(usize, std.mem.trimEnd(u8, out, "\n"), 10));
    }
    // A's ring holds them (the pool's charged footprint; a 64 KiB image is
    // one record each).
    const a_staged = blk: {
        const sa = try rig.stats();
        defer gpa.free(sa);
        break :blk numberAfter(sa, "\"current_staged_write_bytes\":") orelse 0;
    };
    try testing.expect(a_staged >= scope_a_files * 64 * 1024);
    // B joins beside A's LIVE ring: its own root, nothing recovered.
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{});
    _ = try b.awaitReady();
    {
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "mounted JOINED as appender 1") != null);
        try testing.expectEqual(@as(?u64, 0), stagingRecoveredOf(text, "/member-"));
        const sb = try b.stats();
        defer gpa.free(sb);
        try testing.expectEqual(@as(?u64, 0), numberAfter(sb, "\"staging_recovery_records\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(sb, "\"staging_member_residue_roots\":"));
        const sa = try rig.stats();
        defer gpa.free(sa);
        try testing.expectEqual(a_staged, numberAfter(sa, "\"current_staged_write_bytes\":") orelse 0);
        const at = try rig.readLog();
        defer gpa.free(at);
        try testing.expectEqual(@as(?u64, 0), stagingRecoveredOf(at, "/member-00000000"));
        std.debug.print("smoke: member scope: A stages {d} files ({d} B charged) in member-00000000; B joined beside them and recovered {d} record(s) in its own root\n", .{ scope_a_files, a_staged, stagingRecoveredOf(text, "/member-") orelse 0 });
    }
    // Both roots under the cache path, both owners live.
    {
        const r = try rig.squeezefs(&.{ "staging", "list", "--json", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "/member-00000000\",\"binding\":\"bound\",\"mount_slot\":0,\"appender_id\":0,") != null);
        try testing.expectEqual(@as(usize, 2), std.mem.count(u8, r.stdout, "\"owner_live\":true"));
        std.debug.print("smoke: member scope: staging list: {s}", .{r.stdout});
    }
    // B's own residents.
    {
        script.clearRetainingCapacity();
        try script.writer.print("mkdir b && i=0; while [ $i -lt {d} ]; do dd if=\"$1/image\" of=b/q$i bs=64k skip=$((i + {d})) count=1 status=none; i=$((i + 1)); done; sha256sum b/* >> \"$1/scope.sha\"; ls b | wc -l", .{ scope_b_files, scope_a_files });
        const out = try b.onMount(script.written());
        defer gpa.free(out);
        try testing.expectEqual(scope_b_files, try std.fmt.parseInt(usize, std.mem.trimEnd(u8, out, "\n"), 10));
    }
    // A SECOND SET on the same cache path: its manager's root IS A's
    // member root name — the live claim refuses it (the old ladder read
    // A's marker as another set's and WIPED A's live ring).
    {
        const c_img = try std.fmt.allocPrint(gpa, "{s}/c-vol.img", .{rig.work});
        defer gpa.free(c_img);
        const c_uri = try std.fmt.allocPrint(gpa, "sqmeta://{s}", .{c_img});
        defer gpa.free(c_uri);
        const c_data = try std.fmt.allocPrint(gpa, "sqdata://{s}/c-d0.img,{s}/c-d1.img", .{ rig.work, rig.work });
        defer gpa.free(c_data);
        const c_log = try std.fmt.allocPrint(gpa, "{s}/daemon-c.log", .{rig.work});
        defer gpa.free(c_log);
        const c_mp = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/mnt-{d}-c", .{linux.getpid()}, 0);
        defer gpa.free(c_mp);
        _ = linux.mkdir(c_mp.ptr, 0o755);
        defer _ = linux.rmdir(c_mp.ptr);
        defer _ = linux.umount2(c_mp.ptr, linux.MNT.DETACH);
        script.clearRetainingCapacity();
        try script.writer.print("truncate -s {d} c-vol.img && truncate -s {d} c-d0.img && truncate -s {d} c-d1.img && echo ok", .{ volume_len, data_len, data_len });
        const made = try rig.shOk(rig.work, script.written());
        gpa.free(made);
        {
            const r = try rig.squeezefs(&.{ "format", "--disk-cache-paths", rig.cache, c_uri, c_data });
            defer rig.freeRun(r);
            if (!r.term.success()) std.debug.print("\nformat C: {s}\n", .{r.stderr});
            try testing.expect(r.term.success());
        }
        const r = try rig.squeezefs(&.{ "mount", "--daemon", "--log-file", c_log, c_uri, c_mp });
        defer rig.freeRun(r);
        if (r.term.success()) _ = rig.squeezefs(&.{ "umount", c_mp }) catch {};
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "StagingOwnerLive") != null);
        const c_text = try Io.Dir.cwd().readFileAlloc(io, c_log, gpa, .limited(1 << 20));
        defer gpa.free(c_text);
        try testing.expect(std.mem.indexOf(u8, c_text, "held LIVE by a mount of ANOTHER SET on this cache path") != null);
        try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, c_mp));
        const sa = try rig.stats();
        defer gpa.free(sa);
        try testing.expectEqual(a_staged, numberAfter(sa, "\"current_staged_write_bytes\":") orelse 0);
        try testing.expectEqual(@as(?u64, 0), numberAfter(sa, "\"staging_generation_discards\":"));
        std.debug.print("smoke: member scope: a second set on the same cache path refused at its mount (StagingOwnerLive); A's ring intact ({d} B charged), 0 discards\n", .{a_staged});
    }
    // B's dismount promotes ITS residents; A's ring is untouched.
    const mark_b = try b.logLen();
    try b.umount();
    try b.expectCleanExit("B's unmount");
    {
        const text = try b.readLog();
        defer gpa.free(text);
        var want_buf: [64]u8 = undefined;
        try testing.expect(std.mem.indexOf(u8, text[mark_b..], try std.fmt.bufPrint(&want_buf, "dismount promotion: {d} staged file(s)", .{scope_b_files})) != null);
        try testing.expect(std.mem.indexOf(u8, text[mark_b..], ", 0 failed") != null);
        const sa = try rig.stats();
        defer gpa.free(sa);
        try testing.expectEqual(a_staged, numberAfter(sa, "\"current_staged_write_bytes\":") orelse 0);
        // A reads B's promoted files.
        script.clearRetainingCapacity();
        try script.writer.print("grep ' b/' \"$1/scope.sha\" | sha256sum --quiet -c && echo ok", .{});
        const out = try rig.onMount(script.written());
        defer gpa.free(out);
        try testing.expectEqualStrings("ok\n", out);
    }
    // A's dismount promotes ITS residents — all of them.
    const mark_a = try rig.logLen();
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        var want_buf: [64]u8 = undefined;
        const want = try std.fmt.bufPrint(&want_buf, "dismount promotion: {d} staged file(s)", .{scope_a_files});
        try testing.expect(std.mem.indexOf(u8, text[mark_a..], want) != null);
    }
    _ = try fsckClean(rig, "the member-scope row");
    // Every file of both members after a remount.
    const mark_r = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark_r);
    {
        const out = try rig.onMount("sha256sum --quiet -c \"$1/scope.sha\" && echo ok");
        defer gpa.free(out);
        try testing.expectEqualStrings("ok\n", out);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    std.debug.print("smoke: member scope: B promoted its {d}, A its {d}; every file sha-equal after the remount; fsck clean\n", .{ scope_b_files, scope_a_files });
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
/// The storm on B for the manager's death: creates in B's own slots, OPEN-
/// ENDED until the row drops its stop sentinel after B's reclaim — a fixed
/// count outlasted `T_self` (≈ 3 s) only at a Debug daemon's create rate
/// (the ReleaseSafe gate binary creates 1,800 small files in under 2 s and
/// the row read a completed storm as "refused before the park").
const kill_row_storm_stop = "storm.stop";
const kill_row_storm_count = "storm.count";
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
/// or write never returned to tar. Every other entry tar touched was acked
/// (a later `Cannot change mode`/`utime`/`close` is an op after the data's
/// ack). `prefix` is the message head up to the member name (`tar: tree/`
/// for a `tar -cf … tree` archive, `tar: ./` for `-C dir .`). Owned names.
fn tarUnackedNames(gpa: std.mem.Allocator, tar_err: []const u8, prefix: []const u8) !std.ArrayList([]const u8) {
    var out: std.ArrayList([]const u8) = .empty;
    errdefer {
        for (out.items) |n| gpa.free(n);
        out.deinit(gpa);
    }
    var it = std.mem.splitScalar(u8, tar_err, '\n');
    while (it.next()) |line| {
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

/// A copy of `base` with `overrides` applied (the MutualTls rows' per-node
/// environments).
fn envWith(gpa: std.mem.Allocator, base: *const std.process.Environ.Map, overrides: []const [2][]const u8) !std.process.Environ.Map {
    var m = std.process.Environ.Map.init(gpa);
    errdefer m.deinit();
    var it = base.iterator();
    while (it.next()) |e| try m.put(e.key_ptr.*, e.value_ptr.*);
    for (overrides) |kv| try m.put(kv[0], kv[1]);
    return m;
}

test "smoke: MutualTls on this kernel — `key tls-init`/`tls-issue` ×3 through the binary (OpenSSL verifies the chain, a foreign CA's leaf fails it) → a mount under SQUEEZEFS_WIRE_CHANNEL=mutual_tls: WITH the tls ULP it comes up (channel 1) and unmounts clean, WITHOUT it it REFUSES on the console naming the missing ULP (the record layer is the kernel's — never a userspace fall-back) → a CA-less mutual_tls refuses naming the CA → a mismatched key refuses → the plaintext mount of the same set serves and unmounts" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    // The PKI through the binary: one CA, node-a and node-b under it; a
    // FOREIGN CA of the same name with node-c under it.
    var pki_buf: [256]u8 = undefined;
    const pki_dir = try std.fmt.bufPrint(&pki_buf, "{s}/pki", .{rig.work});
    var pa: [320]u8 = undefined;
    var pb: [320]u8 = undefined;
    var pc: [320]u8 = undefined;
    var pf: [320]u8 = undefined;
    const dir_a = try std.fmt.bufPrint(&pa, "{s}/a", .{pki_dir});
    const dir_b = try std.fmt.bufPrint(&pb, "{s}/b", .{pki_dir});
    const dir_c = try std.fmt.bufPrint(&pc, "{s}/c", .{pki_dir});
    const dir_f = try std.fmt.bufPrint(&pf, "{s}/foreign", .{pki_dir});
    inline for (.{
        .{ "key", "tls-init", "--name", "smoke-ca", pki_dir },
        .{ "key", "tls-issue", "--node", "node-a", "--out", dir_a, pki_dir },
        .{ "key", "tls-issue", "--node", "node-b", "--out", dir_b, pki_dir },
        .{ "key", "tls-init", "--name", "smoke-ca", dir_f },
        .{ "key", "tls-issue", "--node", "node-c", "--out", dir_c, dir_f },
    }) |argv| {
        const r = try rig.squeezefs(&argv);
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\n--- {s} {s}: {s}\n", .{ argv[0], argv[1], r.stderr });
        try testing.expect(r.term.success());
    }
    // A second tls-init without --force refuses (a CA key is never
    // overwritten by accident).
    {
        const r = try rig.squeezefs(&.{ "key", "tls-init", pki_dir });
        defer rig.freeRun(r);
        try testing.expect(!r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stderr, "never overwritten") != null);
    }
    // OpenSSL's verdict on the issued chain (when the venue has it).
    {
        var script: [1024]u8 = undefined;
        const r = try rig.sh(rig.work, try std.fmt.bufPrint(&script, "command -v openssl >/dev/null || {{ echo no-openssl; exit 0; }}; openssl verify -CAfile {s}/ca.pem {s}/node.pem {s}/node.pem && (openssl verify -CAfile {s}/ca.pem {s}/node.pem 2>&1 || true) | grep -c 'verification failed'", .{ pki_dir, dir_a, dir_b, pki_dir, dir_c }));
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\n--- openssl verify: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        if (std.mem.indexOf(u8, r.stdout, "no-openssl") == null) {
            try testing.expect(std.mem.indexOf(u8, r.stdout, "node.pem: OK") != null);
            try testing.expect(std.mem.endsWith(u8, std.mem.trimEnd(u8, r.stdout, "\n"), "1"));
        }
        std.debug.print("\nsmoke: mtls: the PKI through the binary; openssl: {s}", .{r.stdout});
    }
    var ca_path_buf: [320]u8 = undefined;
    const ca_path = try std.fmt.bufPrint(&ca_path_buf, "{s}/ca.pem", .{pki_dir});
    var cert_a: [320]u8 = undefined;
    var key_a: [320]u8 = undefined;
    var key_b: [320]u8 = undefined;
    var ca_missing: [320]u8 = undefined;
    try rig.formatSet(true);
    // 1. A mount under mutual_tls with a good identity: with the tls ULP it
    //    comes up and unmounts clean; without it it refuses on the console
    //    naming the ULP and the remedy (CONFIG_TLS) — never a userspace
    //    fall-back.
    var env_tls = try envWith(gpa, &rig.env, &.{ .{ "SQUEEZEFS_WIRE_CHANNEL", "mutual_tls" }, .{ "SQUEEZEFS_WIRE_TLS_CA", ca_path }, .{ "SQUEEZEFS_WIRE_TLS_CERT", try std.fmt.bufPrint(&cert_a, "{s}/node.pem", .{dir_a}) }, .{ "SQUEEZEFS_WIRE_TLS_KEY", try std.fmt.bufPrint(&key_a, "{s}/node.key", .{dir_a}) } });
    defer env_tls.deinit();
    const ulps = try rig.shOk(rig.work, "cat /proc/sys/net/ipv4/tcp_available_ulp");
    defer gpa.free(ulps);
    const kernel_has_tls = std.mem.indexOf(u8, ulps, "tls") != null;
    if (kernel_has_tls) {
        const a = try Peer.init(rig, "a-tls");
        defer a.deinit();
        a.env = &env_tls;
        try a.spawn(&.{});
        _ = try a.awaitReady();
        const text = try a.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "channel class mutual_tls — node 'node-a' under CA 'smoke-ca'") != null);
        try testing.expect(std.mem.indexOf(u8, text, "channel mutual_tls)") != null);
        const st = try a.stats();
        defer gpa.free(st);
        try testing.expectEqual(@as(?u64, 1), numberAfter(st, "\"cluster_wire_channel_mutual_tls\":"));
        try a.umount();
        try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try a.waitExit());
        try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, a.mp));
        std.debug.print("smoke: mtls: mount under mutual_tls on kernel TLS (ulp '{s}'): up with channel 1, unmounted clean\n", .{std.mem.trimEnd(u8, ulps, "\n")});
    } else {
        const a = try Peer.init(rig, "a-tls");
        defer a.deinit();
        a.env = &env_tls;
        try a.spawn(&.{});
        const term = try a.waitExit();
        try testing.expect(!term.success());
        const text = try a.readLog();
        defer gpa.free(text);
        const needle = "needs kernel TLS and this kernel has no `tls` ULP";
        if (std.mem.indexOf(u8, text, needle) == null) std.debug.print("\n--- the mutual_tls mount's log ---\n{s}\n", .{text});
        try testing.expect(std.mem.indexOf(u8, text, needle) != null);
        try testing.expect(std.mem.indexOf(u8, text, "never a userspace fall-back") != null);
        var it = std.mem.splitScalar(u8, text, '\n');
        while (it.next()) |line| if (std.mem.indexOf(u8, line, needle) != null) {
            std.debug.print("smoke: mtls: mount under mutual_tls: {f}; {s}\n", .{ term, line[0..@min(line.len, 360)] });
            break;
        };
        try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, a.mp));
    }
    // 2. CA-less: the CA path names no file.
    {
        var env_noca = try envWith(gpa, &env_tls, &.{.{ "SQUEEZEFS_WIRE_TLS_CA", try std.fmt.bufPrint(&ca_missing, "{s}/no-such-ca.pem", .{pki_dir}) }});
        defer env_noca.deinit();
        const a = try Peer.init(rig, "a-noca");
        defer a.deinit();
        a.env = &env_noca;
        try a.spawn(&.{});
        const term = try a.waitExit();
        try testing.expect(!term.success());
        const text = try a.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "a CA-less TLS configuration is refused, never downgraded") != null);
    }
    // 3. A key that is not the certificate's (node-b's key under node-a's cert).
    {
        var env_mismatch = try envWith(gpa, &env_tls, &.{.{ "SQUEEZEFS_WIRE_TLS_KEY", try std.fmt.bufPrint(&key_b, "{s}/node.key", .{dir_b}) }});
        defer env_mismatch.deinit();
        const a = try Peer.init(rig, "a-badkey");
        defer a.deinit();
        a.env = &env_mismatch;
        try a.spawn(&.{});
        const term = try a.waitExit();
        try testing.expect(!term.success());
        const text = try a.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "does not match the node certificate") != null);
    }
    // 4. The same set under the plaintext class serves (the refusals left
    //    nothing behind) and unmounts clean.
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    {
        const out = try rig.onMount("printf 'x\\n' > f && cat f; cat .stats");
        defer gpa.free(out);
        try testing.expect(std.mem.startsWith(u8, out, "x\n"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(out, "\"cluster_wire_channel_mutual_tls\":"));
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

/// The MutualTls matrix's `dd` on B: 200 MiB over 4 MiB blocks (the
/// Plaintext matrix's shape, byte for byte).
const mtls_dd_mib: u64 = 200;

test "smoke: THE TWO-DAEMON MATRIX over MutualTls on KERNEL TLS — `key tls-init` + `tls-issue` ×2 → mount A (SQUEEZEFS_WIRE_CHANNEL=mutual_tls, node-a) → mount B (node-b) JOINED over TLS (.stats: channel 1, handshakes, 0 refusals) → A reads B's file under a token, B's write recalls it → B's mkdir under A's root (a cross-owner intent) → a 200 MiB dd on B, the sha from A equal (the Plaintext row's result) → 0 MAC failures and 0 refusals on both → a THIRD daemon under a FOREIGN CA is refused (its mount fails naming the CA, A counts tls_refused_untrusted) → a PLAINTEXT daemon is refused by name at the handshake bound (A counts plaintext_refused) → umount B, A; on a kernel without the tls ULP the row is the ledger's other_step skip" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    {
        const ulps = try rig.shOk(rig.work, "cat /proc/sys/net/ipv4/tcp_available_ulp; uname -r");
        defer gpa.free(ulps);
        if (std.mem.indexOf(u8, ulps, "tls") == null) {
            var detail: [96]u8 = undefined;
            return testkit.skip(@src(), .other_step, std.fmt.bufPrint(&detail, "kernel {s}: no tls ULP (CONFIG_TLS)", .{std.mem.trimEnd(u8, ulps[std.mem.lastIndexOfScalar(u8, std.mem.trimEnd(u8, ulps, "\n"), '\n') orelse 0 ..], "\n")}) catch "kernel without the tls ULP (CONFIG_TLS)");
        }
    }
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    // The PKI through the binary: one CA, node-a and node-b under it; a
    // FOREIGN CA of the same name with node-c under it.
    var pki_buf: [256]u8 = undefined;
    const pki_dir = try std.fmt.bufPrint(&pki_buf, "{s}/pki", .{rig.work});
    var pa: [320]u8 = undefined;
    var pb: [320]u8 = undefined;
    var pc: [320]u8 = undefined;
    var pf: [320]u8 = undefined;
    const dir_a = try std.fmt.bufPrint(&pa, "{s}/a", .{pki_dir});
    const dir_b = try std.fmt.bufPrint(&pb, "{s}/b", .{pki_dir});
    const dir_c = try std.fmt.bufPrint(&pc, "{s}/c", .{pki_dir});
    const dir_f = try std.fmt.bufPrint(&pf, "{s}/foreign", .{pki_dir});
    inline for (.{
        .{ "key", "tls-init", "--name", "smoke-ca", pki_dir },
        .{ "key", "tls-issue", "--node", "node-a", "--out", dir_a, pki_dir },
        .{ "key", "tls-issue", "--node", "node-b", "--out", dir_b, pki_dir },
        .{ "key", "tls-init", "--name", "smoke-ca", dir_f },
        .{ "key", "tls-issue", "--node", "node-c", "--out", dir_c, dir_f },
    }) |argv| {
        const r = try rig.squeezefs(&argv);
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\n--- {s} {s}: {s}\n", .{ argv[0], argv[1], r.stderr });
        try testing.expect(r.term.success());
    }
    var ca_path_buf: [320]u8 = undefined;
    const ca_path = try std.fmt.bufPrint(&ca_path_buf, "{s}/ca.pem", .{pki_dir});
    var cert_a: [320]u8 = undefined;
    var key_a: [320]u8 = undefined;
    var cert_b: [320]u8 = undefined;
    var key_b: [320]u8 = undefined;
    var cert_c: [320]u8 = undefined;
    var key_c: [320]u8 = undefined;
    var ca_f: [320]u8 = undefined;
    try rig.env.put("SQUEEZEFS_WIRE_CHANNEL", "mutual_tls");
    try rig.env.put("SQUEEZEFS_WIRE_TLS_CA", ca_path);
    try rig.env.put("SQUEEZEFS_WIRE_TLS_CERT", try std.fmt.bufPrint(&cert_a, "{s}/node.pem", .{dir_a}));
    try rig.env.put("SQUEEZEFS_WIRE_TLS_KEY", try std.fmt.bufPrint(&key_a, "{s}/node.key", .{dir_a}));
    var env_b = try envWith(gpa, &rig.env, &.{ .{ "SQUEEZEFS_WIRE_TLS_CERT", try std.fmt.bufPrint(&cert_b, "{s}/node.pem", .{dir_b}) }, .{ "SQUEEZEFS_WIRE_TLS_KEY", try std.fmt.bufPrint(&key_b, "{s}/node.key", .{dir_b}) } });
    defer env_b.deinit();
    var env_c = try envWith(gpa, &rig.env, &.{ .{ "SQUEEZEFS_WIRE_TLS_CA", try std.fmt.bufPrint(&ca_f, "{s}/ca.pem", .{dir_f}) }, .{ "SQUEEZEFS_WIRE_TLS_CERT", try std.fmt.bufPrint(&cert_c, "{s}/node.pem", .{dir_c}) }, .{ "SQUEEZEFS_WIRE_TLS_KEY", try std.fmt.bufPrint(&key_c, "{s}/node.key", .{dir_c}) } });
    defer env_c.deinit();
    var env_d = try envWith(gpa, &rig.env, &.{.{ "SQUEEZEFS_WIRE_CHANNEL", "plaintext" }});
    defer env_d.deinit();
    try rig.formatSet(true);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "channel class mutual_tls — node 'node-a' under CA 'smoke-ca'") != null);
    }
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.env = &env_b;
    try b.spawn(&.{});
    _ = try b.awaitReady();
    {
        const text = try b.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "mounted JOINED as appender 1") != null);
        try testing.expect(std.mem.indexOf(u8, text, "channel class mutual_tls — node 'node-b'") != null);
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"cluster_wire_channel_mutual_tls\":"));
        try testing.expectEqual(@as(?u64, 1), numberAfter(tb, "\"cluster_wire_channel_mutual_tls\":"));
        try testing.expect((numberAfter(ta, "\"cluster_wire_tls_handshakes\":") orelse 0) >= 1);
        try testing.expect((numberAfter(tb, "\"cluster_wire_tls_handshakes\":") orelse 0) >= 1);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"cluster_wire_plaintext_refused\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"cluster_wire_tls_refused_untrusted\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"cluster_wire_tls_refused_expired\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"cluster_wire_tls_refused_protocol\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"cluster_wire_tls_dial_refused\":"));
        std.debug.print("\nsmoke: mtls matrix [ktls]: B joined over kernel TLS — A: handshakes {d}, sessions admitted {d}; B: handshakes {d}, wire verbs {d}\n", .{ numberAfter(ta, "\"cluster_wire_tls_handshakes\":") orelse 0, numberAfter(ta, "\"cluster_wire_sessions_admitted\":") orelse 0, numberAfter(tb, "\"cluster_wire_tls_handshakes\":") orelse 0, numberAfter(tb, "\"joined_wire_verbs\":") orelse 0 });
    }
    // A reads B's file under a token; B's write recalls it; A's next read
    // is fresh.
    {
        const w = try b.onMount("mkdir b-dir && printf 'b-f\\n' > b-dir/f");
        gpa.free(w);
        const first = try rig.onMount("cat b-dir/f");
        defer gpa.free(first);
        try testing.expectEqualStrings("b-f\n", first);
        const w2 = try b.onMount("printf 'fresh\\n' > b-dir/f");
        gpa.free(w2);
        const second = try rig.onMount("cat b-dir/f");
        defer gpa.free(second);
        try testing.expectEqualStrings("fresh\n", second);
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expect((numberAfter(tb, "\"dlm_token_grants\":") orelse 0) >= 1);
        try testing.expect((numberAfter(tb, "\"dlm_token_recalls\":") orelse 0) >= 1);
    }
    // B's mkdir under A's root: a cross-owner intent shipped over TLS.
    {
        const mk = try rig.onMount("mkdir a-dir");
        gpa.free(mk);
        const out = try b.onMount("mkdir a-dir/from-b && printf 'x\\n' > a-dir/from-b/f && ls a-dir");
        defer gpa.free(out);
        try testing.expectEqualStrings("from-b\n", out);
        const seen = try rig.onMount("ls a-dir; cat a-dir/from-b/f");
        defer gpa.free(seen);
        try testing.expectEqualStrings("from-b\nx\n", seen);
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expect((numberAfter(tb, "\"xv_cross_owner_intents_minted\":") orelse 0) >= 1);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"xv_cross_owner_intents_stuck\":"));
    }
    // The 200 MiB dd on B (its grants pulled from A over TLS); the sha
    // round trip from A: the Plaintext row's result, byte for byte.
    {
        var script: [256]u8 = undefined;
        const t0 = clock.monotonicNs();
        const out = try b.onMount(try std.fmt.bufPrint(&script, "mkdir b-data && dd if=/dev/urandom of=b-data/big bs=1M count={d} status=none && sync b-data/big && sha256sum b-data/big | cut -d' ' -f1", .{mtls_dd_mib}));
        defer gpa.free(out);
        const dd_ms = (clock.monotonicNs() - t0) / std.time.ns_per_ms;
        const sha_b = std.mem.trimEnd(u8, out, "\n");
        const from_a = try rig.onMount("sha256sum b-data/big | cut -d' ' -f1");
        defer gpa.free(from_a);
        try testing.expectEqualStrings(sha_b, std.mem.trimEnd(u8, from_a, "\n"));
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expect((numberAfter(ta, "\"block_grants\":") orelse 0) >= 1);
        try expectMustStayZero(ta);
        try expectMustStayZero(tb);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"cluster_wire_mac_failures\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"cluster_wire_mac_failures\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"cluster_wire_admissions_refused\":"));
        std.debug.print("smoke: mtls matrix: 200 MiB dd on B in {d} ms, sha from A equal; wire over TLS — A: frames received {d}, bytes received {d}, grants {d}; B: frames sent {d}, bytes sent {d}, key updates {d}; MAC failures 0, refusals 0 on both\n", .{ dd_ms, numberAfter(ta, "\"cluster_wire_frames_received\":") orelse 0, numberAfter(ta, "\"cluster_wire_bytes_received\":") orelse 0, numberAfter(ta, "\"block_grants\":") orelse 0, numberAfter(tb, "\"cluster_wire_frames_sent\":") orelse 0, numberAfter(tb, "\"cluster_wire_bytes_sent\":") orelse 0, numberAfter(tb, "\"cluster_wire_tls_key_updates\":") orelse 0 });
    }
    // The negative half: a THIRD daemon under a FOREIGN CA of the same
    // name — its dial is refused at the TLS handshake (its log names the
    // CA), its mount fails, and A counts the untrusted peer.
    {
        const c = try Peer.init(rig, "c");
        defer c.deinit();
        c.env = &env_c;
        try c.spawn(&.{});
        const term = try c.waitExit();
        try testing.expect(!term.success());
        const text = try c.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "refused at the TLS handshake") != null);
        try testing.expect(std.mem.indexOf(u8, text, "not under this set's CA") != null or std.mem.indexOf(u8, text, "refused THIS node's certificate") != null);
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        try testing.expect((numberAfter(ta, "\"cluster_wire_tls_refused_untrusted\":") orelse 0) >= 1);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"cluster_wire_sessions_poisoned\":"));
        std.debug.print("smoke: mtls matrix: a daemon under a FOREIGN CA: mount {f}; A counts tls_refused_untrusted {d}\n", .{ term, numberAfter(ta, "\"cluster_wire_tls_refused_untrusted\":") orelse 0 });
        var it = std.mem.splitScalar(u8, text, '\n');
        while (it.next()) |line| if (std.mem.indexOf(u8, line, "TLS handshake") != null) {
            std.debug.print("smoke: mtls matrix: C's log: {s}\n", .{line[0..@min(line.len, 300)]});
            break;
        };
    }
    // A PLAINTEXT daemon at the MutualTls set: silent for the handshake
    // bound, answered with a Refused naming the class.
    {
        const d = try Peer.init(rig, "d");
        defer d.deinit();
        d.env = &env_d;
        try d.spawn(&.{});
        const term = try d.waitExit();
        try testing.expect(!term.success());
        const text = try d.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "this channel is MutualTls") != null);
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        try testing.expect((numberAfter(ta, "\"cluster_wire_plaintext_refused\":") orelse 0) >= 1);
        std.debug.print("smoke: mtls matrix: a PLAINTEXT daemon: mount {f}; A counts plaintext_refused {d}\n", .{ term, numberAfter(ta, "\"cluster_wire_plaintext_refused\":") orelse 0 });
    }
    // umount B (the clean leave over TLS), then A.
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, b.mp));
    {
        const after = try rig.onMount("cat b-dir/f; ls a-dir");
        defer gpa.free(after);
        try testing.expectEqualStrings("fresh\nfrom-b\n", after);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

test "smoke: THE KILL ROWS (a) — kill -9 B mid-`tar -x` into A's directory ⇒ A evicts B at T_owner, records the death, recovers its region (dead_members_acted ≡ recorded × regions held, rollbacks 0, the page recovered → free), every file B acked is whole on A → remount B under the same node (the same-node successor) sees them → B's FIRST cross-owner mkdir succeeds → A's `rm -rf` of the recovered tree completes; recall timeouts 0, tripwires 0 on both" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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
    var unacked = try tarUnackedNames(gpa, tar_err, "tar: tree/");
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
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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
    // The storm's stop sentinel and its final count live in the rig's work
    // dir (local paths: the storm must never walk the mount root).
    var storm_stop_buf: [192]u8 = undefined;
    const storm_stop_path = try std.fmt.bufPrint(&storm_stop_buf, "{s}/{s}", .{ rig.work, kill_row_storm_stop });
    var storm_count_buf: [192]u8 = undefined;
    const storm_count_path = try std.fmt.bufPrint(&storm_count_buf, "{s}/{s}", .{ rig.work, kill_row_storm_count });
    const storm_script = "cd \"$1/storm\" && i=0; while [ ! -e \"$2\" ]; do printf 's%s\\n' $i > s$i || { echo \"storm: s$i refused (rc $?)\" >&2; exit 1; }; i=$((i + 1)); done; echo $i > \"$3\"";
    var storm_err_buf: [192]u8 = undefined;
    const storm_err_path = try std.fmt.bufPrint(&storm_err_buf, "{s}/storm.err", .{rig.work});
    const storm_err_file = try Io.Dir.cwd().createFile(io, storm_err_path, .{ .truncate = true });
    defer storm_err_file.close(io);
    var storm = try std.process.spawn(io, .{ .argv = &.{ "sh", "-c", storm_script, "storm", b.mp, storm_stop_path, storm_count_path }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .{ .file = storm_err_file } });
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
    // The parked joiner's `.stats` (rung 10 item 5a): the kernel
    // revalidates the root before the virtual lookup, and the root is the
    // dead manager's object — the read lands (the cached token while the
    // lease stands, the last root entry once it lapses: the unit rows) and
    // names the park, so the operator reads WHY the mount is parked.
    {
        const tb = (try b.statsTry()) orelse return error.ParkedStatsUnreadable;
        defer gpa.free(tb);
        try testing.expectEqual(@as(?u64, @backingInt(membership.park_reclaim.Word.parked)), numberAfter(tb, "\"appender_parked\":"));
    }
    std.debug.print("smoke: kill rows (b): B PARKED {d} ms after the kill (acks held; .stats readable, naming the park); the storm child is alive, waiting\n", .{parked_ms});
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
    // The storm ran through the park and the reclaim: stop it now and read
    // how many files it created (every one of them acked, none refused).
    {
        const stop = try Io.Dir.cwd().createFile(io, storm_stop_path, .{ .truncate = true });
        stop.close(io);
    }
    const storm_term = try storm.wait(io);
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, storm_term);
    const storm_created: usize = blk: {
        const text = try Io.Dir.cwd().readFileAlloc(io, storm_count_path, gpa, .limited(64));
        defer gpa.free(text);
        break :blk try std.fmt.parseInt(usize, std.mem.trim(u8, text, " \n"), 10);
    };
    try testing.expect(storm_created >= kill_row_kill_after);
    std.debug.print("smoke: kill rows (b): the storm created {d} files across the kill, the park and the reclaim\n", .{storm_created});
    // Every storm file whole on B (its own slots) AND on A (under tokens
    // from B — A's reader plane dialled B's listener afresh).
    const whole_b = try expectTreeWhole(b, "storm");
    try testing.expectEqual(storm_created, whole_b);
    {
        const out = try rig.onMount("ls storm | wc -l; cat storm/s7");
        defer gpa.free(out);
        var want_buf: [32]u8 = undefined;
        try testing.expectEqualStrings(try std.fmt.bufPrint(&want_buf, "{d}\ns7\n", .{storm_created}), out);
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
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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

// ---------------------------------------------------------------------------
// THE KVDA PAGE SINK (charter §5.5 "pages written before barrier #1";
// rung 10 item 2): the holder dies with a joiner's grants outstanding.
// ---------------------------------------------------------------------------

/// The joiner's first `dd` on the kill row: 200 MiB = 50 blocks over the
/// set's two data volumes, pulled as floor grants (64 each) — far more
/// granted than minted, so the holder dies with a wide unconsumed window.
const kvda_dd_mib: u64 = 200;
const kvda_dd_blocks: u64 = kvda_dd_mib / 4;

/// The two outcomes of one row (`skip_pages` = `SQUEEZEFS_TEST_KVDA_PAGE_SKIP`
/// on the dying holder): with the pages the successor re-learns nothing;
/// without them it re-learns the joiner's window from the declaration.
fn kvdaKillRow(skip_pages: bool) !void {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    if (skip_pages) try rig.env.put("SQUEEZEFS_TEST_KVDA_PAGE_SKIP", "1");
    try rig.formatSet(true);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    // The holder's pages: held on volume 0 and published, one per data volume.
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "its pages held on the home volume and journaled into ring 0") != null);
        const ta = try rig.stats();
        defer gpa.free(ta);
        try testing.expectEqual(@as(?u64, 2), numberAfter(ta, "\"data_alloc_bitmap_holds\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"data_alloc_bitmap_successor_loads\":"));
    }
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.node = kill_peer_node;
    try b.spawn(&.{});
    _ = try b.awaitReady();
    // B's dd: the grants pulled from A, every carve's SET journaled into
    // ring 0 before its reply; B's renewals DECLARE its windows and A
    // folds them (the joiner-side grant ledger: consumed grants retire).
    var sha1_buf: [80]u8 = undefined;
    var sha1: []const u8 = "";
    {
        var script_buf: [256]u8 = undefined;
        const out = try b.onMount(try std.fmt.bufPrint(&script_buf, "mkdir kv && dd if=/dev/urandom of=kv/one bs=1M count={d} status=none && sync kv/one && sha256sum kv/one | cut -d' ' -f1", .{kvda_dd_mib}));
        defer gpa.free(out);
        sha1 = try std.fmt.bufPrint(&sha1_buf, "{s}", .{std.mem.trimEnd(u8, out, "\n")});
    }
    const ckpts0 = blk: {
        // B's beat declares every renewal: the holder's count climbs.
        const ta = inner: {
            const deadline = clock.monotonicNs() + kill_row_wait_ns;
            while (true) {
                const t = try rig.stats();
                if ((numberAfter(t, "\"block_grant_windows_declared\":") orelse 0) >= 1 or clock.monotonicNs() >= deadline) break :inner t;
                gpa.free(t);
                sleepMs(200);
            }
        };
        defer gpa.free(ta);
        try testing.expect((numberAfter(ta, "\"block_grant_blocks\":") orelse 0) >= kvda_dd_blocks);
        try testing.expect((numberAfter(ta, "\"data_alloc_bitmap_deltas_journaled\":") orelse 0) >= 1);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"block_grant_deferred\":"));
        break :blk numberAfter(ta, "\"meta_kv_checkpoints\":") orelse 0;
    };
    // Two more checkpoints on A: the tail moves past every delta of the
    // dd's carves (what a successor's kept window would otherwise fold),
    // and — unless the seam skips them — the pages carry the carves.
    {
        var ckpt_buf: [48]u8 = undefined;
        const ta = try awaitStats(rig, &.{.{ .key = "\"meta_kv_checkpoints\":", .want = ckpts0 + 2 }}, kill_row_wait_ns);
        defer gpa.free(ta);
        const pages = numberAfter(ta, "\"data_alloc_bitmap_pages_written\":") orelse 0;
        if (skip_pages) try testing.expectEqual(@as(u64, 0), pages) else try testing.expect(pages >= 1);
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expect((numberAfter(tb, "\"block_grant_windows_declared_by_writer\":") orelse 0) >= 1);
        std.debug.print("\nsmoke: kvda ({s}): B's dd pulled {d} grant(s) / {d} blocks; A journaled {d} delta(s), wrote {d} page(s), folded {d} declaration(s) (open grants {s}); killing A\n", .{ if (skip_pages) "pages SKIPPED" else "pages held", numberAfter(ta, "\"block_grants\":") orelse 0, numberAfter(ta, "\"block_grant_blocks\":") orelse 0, numberAfter(ta, "\"data_alloc_bitmap_deltas_journaled\":") orelse 0, pages, numberAfter(ta, "\"block_grant_windows_declared\":") orelse 0, if (std.mem.indexOf(u8, ta, "\"open_grants\":")) |i| ta[i..@min(ta.len, i + 16)] else try std.fmt.bufPrint(&ckpt_buf, "?", .{}) });
    }
    // The holder dies with B's windows outstanding; B parks at T_self.
    try rig.kill();
    _ = try b.awaitLog("appender park: T_self reached with the home volume 0's manager unreachable — parked", 0, kill_row_wait_ns);
    // The successor: the same-node takeover folds the kept window onto
    // the predecessor's pages and loads them at its arm.
    if (skip_pages) _ = rig.env.swapRemove("SQUEEZEFS_TEST_KVDA_PAGE_SKIP");
    const mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        const seg = text[mark..];
        try testing.expect(std.mem.indexOf(u8, seg, "appender 0 Live as the manager (term 2") != null);
        try testing.expect(std.mem.indexOf(u8, seg, "the kept window (") != null);
        try testing.expect(std.mem.indexOf(u8, seg, "predecessor bitmap page(s) loaded through") != null);
        try testing.expect(std.mem.indexOf(u8, seg, "no grant plane armed") == null);
    }
    // B's reclaim lands and its next renewal DECLARES its windows to the
    // successor: adopted under its name.
    {
        const reclaimed = try awaitStats(b, &.{.{ .key = "\"appender_parked\":", .want = 0 }}, kill_row_wait_ns);
        defer gpa.free(reclaimed);
        try testing.expectEqual(@as(?u64, 0), numberAfter(reclaimed, "\"appender_park_expiries\":"));
    }
    // The successor's first folded declaration (B's beat declares every
    // renewal: the count climbs from 0 at the remount).
    const ta2 = blk: {
        const deadline = clock.monotonicNs() + kill_row_wait_ns;
        while (true) {
            const ta = try rig.stats();
            if ((numberAfter(ta, "\"block_grant_windows_declared\":") orelse 0) >= 1 or clock.monotonicNs() >= deadline) break :blk ta;
            gpa.free(ta);
            sleepMs(200);
        }
    };
    defer gpa.free(ta2);
    std.debug.print("smoke: kvda ({s}): after the remount — successor loads {d}, folds {d}, bits loaded {d}, leaks adopted {d}, relearned {d}, declarations folded {d}, takeovers {d}; holder rows {s}\n", .{ if (skip_pages) "pages SKIPPED" else "pages held", numberAfter(ta2, "\"data_alloc_bitmap_successor_loads\":") orelse 0, numberAfter(ta2, "\"data_alloc_bitmap_predecessor_folds\":") orelse 0, numberAfter(ta2, "\"data_alloc_bitmap_bits_loaded\":") orelse 0, numberAfter(ta2, "\"data_alloc_bitmap_leaks_adopted\":") orelse 0, numberAfter(ta2, "\"block_grant_relearned_from_windows\":") orelse 0, numberAfter(ta2, "\"block_grant_windows_declared\":") orelse 0, numberAfter(ta2, "\"alloc_lease_same_node_takeovers\":") orelse 0, if (std.mem.indexOf(u8, ta2, "\"block_grant_holder\":")) |i| ta2[i..@min(ta2.len, i + 400)] else "?" });
    try testing.expectEqual(@as(?u64, 2), numberAfter(ta2, "\"data_alloc_bitmap_successor_loads\":"));
    try testing.expectEqual(@as(?u64, 2), numberAfter(ta2, "\"data_alloc_bitmap_predecessor_folds\":"));
    try testing.expectEqual(@as(?u64, 2), numberAfter(ta2, "\"alloc_lease_same_node_takeovers\":"));
    const relearned = numberAfter(ta2, "\"block_grant_relearned_from_windows\":") orelse 0;
    const adopted = numberAfter(ta2, "\"data_alloc_bitmap_leaks_adopted\":") orelse 0;
    const loaded = numberAfter(ta2, "\"data_alloc_bitmap_bits_loaded\":") orelse 0;
    if (skip_pages) {
        // The fallback: the stale pages carried none of the dd's carves,
        // the kept window was checkpointed away — B's declared window is
        // re-learnt, SET under the successor's term.
        try testing.expect(relearned >= 1);
    } else {
        // The pages carried every carve: B's window was LOADED, then
        // ADOPTED by its declaration — nothing re-learnt.
        try testing.expectEqual(@as(u64, 0), relearned);
        try testing.expect(loaded >= 1 and adopted >= 1);
    }
    try testing.expectEqual(@as(?u64, 0), numberAfter(ta2, "\"data_alloc_bitmap_decls_rejected\":"));
    // B keeps minting from its ADOPTED window (never re-granted to anyone
    // else) and the successor serves its top-ups; both files whole from A.
    var sha2_buf: [80]u8 = undefined;
    var sha2: []const u8 = "";
    {
        var script_buf: [256]u8 = undefined;
        const out = try b.onMount(try std.fmt.bufPrint(&script_buf, "dd if=/dev/urandom of=kv/two bs=1M count={d} status=none && sync kv/two && sha256sum kv/two | cut -d' ' -f1", .{kvda_dd_mib / 2}));
        defer gpa.free(out);
        sha2 = try std.fmt.bufPrint(&sha2_buf, "{s}", .{std.mem.trimEnd(u8, out, "\n")});
    }
    {
        const from_a = try rig.onMount("sha256sum kv/one kv/two | cut -d' ' -f1");
        defer gpa.free(from_a);
        var lines = std.mem.splitScalar(u8, from_a, '\n');
        try testing.expectEqualStrings(sha1, lines.next() orelse "");
        try testing.expectEqualStrings(sha2, lines.next() orelse "");
    }
    {
        const ta3 = try rig.stats();
        defer gpa.free(ta3);
        try expectMustStayZero(ta3);
        try testing.expectEqual(relearned, numberAfter(ta3, "\"block_grant_relearned_from_windows\":") orelse 0);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta3, "\"block_grant_deferred\":"));
        const tb = try b.stats();
        defer gpa.free(tb);
        try expectMustStayZero(tb);
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"block_grant_topup_refused\":"));
        std.debug.print("smoke: kvda ({s}): the successor loaded {d} bit(s) from the predecessor's pages, adopted {d} from B's declaration, re-learnt {d}; B's second dd minted from its adopted window; both shas equal from A\n", .{ if (skip_pages) "pages SKIPPED" else "pages held", loaded, adopted, relearned });
    }
    // B leaves: the holder is the only writer, and the remainder the
    // declarations never named (B's published blocks, invisible to A's
    // census while B's trees were B's) converges under A's now-exact
    // census — REFERENCED after all, never released.
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    {
        const ta4 = try awaitStats(rig, &.{.{ .key = "\"data_alloc_bitmap_leaks_pending\":", .want = 0 }}, kill_row_wait_ns);
        defer gpa.free(ta4);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta4, "\"data_alloc_bitmap_leaks_pending\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta4, "\"data_alloc_bitmap_leaks_released\":"));
        // With the pages, B's published blocks were candidates the census
        // referenced; without them nothing was ever a candidate (the stale
        // pages carried no bit beyond the refs census) — the consumed
        // blocks below B's windows are the frontier's to protect.
        if (!skip_pages) try testing.expect((numberAfter(ta4, "\"data_alloc_bitmap_leaks_referenced\":") orelse 0) >= 1);
        std.debug.print("smoke: kvda ({s}): after B's leave the sole writer's census converged the remainder — referenced {d}, released {d}\n", .{ if (skip_pages) "pages SKIPPED" else "pages held", numberAfter(ta4, "\"data_alloc_bitmap_leaks_referenced\":") orelse 0, numberAfter(ta4, "\"data_alloc_bitmap_leaks_released\":") orelse 0 });
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the KVDA kill row");
}

test "smoke: THE KVDA PAGE SINK — A holds the leases with its pages on volume 0 and every carve's delta in ring 0; B pulls grants (200 MiB dd), its renewals declare its windows (the joiner-side grant ledger retires the consumed ones) → kill -9 A with B's window outstanding → remount A: the same-node takeover folds the kept window onto the pages, the arm LOADS them, B's reclaim declaration ADOPTS its window — relearned 0 → B keeps minting from it, both files whole from A → fsck clean (C6/C13)" {
    try kvdaKillRow(false);
}

test "smoke: THE KVDA PAGE SINK's fallback — the same row with the dying holder's page writes SKIPPED (the seam): the successor's stale pages carry no carve and the kept window was checkpointed away, so B's declaration RE-LEARNS its window (`block_grant_relearned_from_windows` ≥ 1 — the delta the pages otherwise make 0); no double allocation, both files whole, fsck clean" {
    try kvdaKillRow(true);
}

// ---------------------------------------------------------------------------
// THE JOINER-SIDE GRANT LEDGER (rung 9 F17's sibling; charter §5.5 "every
// live writer declares its held windows on each renewal").
// ---------------------------------------------------------------------------

/// The row's ledger capacity (the seam): a joiner that writes 24 floor
/// grants' worth drives the holder's table three times past it unless
/// every consumed grant retired at a renewal declaration.
const grant_ledger_row_cap: u64 = 8;
/// 1 MiB blocks: one floor grant (64 blocks) is one 64 MiB file, so 24
/// files = 24 grants = 1.5 GiB over the set's two 2 GiB volumes.
const grant_ledger_row_block: u64 = 1 << 20;
const grant_ledger_row_files: u64 = 24;

test "smoke: THE JOINER-SIDE GRANT LEDGER — the holder's ledger capped at 8 open grants (the seam), 1 MiB blocks; B writes 24 files of one floor grant each (24 grants, three times the cap): every consumed grant RETIRES at B's renewal declaration (`block_grant_consumed_retired` ≥ files − cap = 16, the table's structural bound), `block_grant_deferred` 0 at A, `block_grant_topup_deferred` 0 and `block_grant_exhausted` 0 at B, `open_grants` ≤ the cap per volume throughout, B's window ≤ GRANTS_MAX ranges; every file whole from A" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    var cap_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_BLOCK_GRANT_LEDGER_CAP", try std.fmt.bufPrint(&cap_text, "{d}", .{grant_ledger_row_cap}));
    var bs_buf: [16]u8 = undefined;
    try rig.formatSetWith(false, &.{ "--block-size", try std.fmt.bufPrint(&bs_buf, "{d}", .{grant_ledger_row_block}) });
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{});
    _ = try b.awaitReady();
    // B's files, one floor grant each, with the holder's table watched
    // between files (never above the cap).
    var i: u64 = 0;
    var max_open: u64 = 0;
    while (i < grant_ledger_row_files) : (i += 1) {
        var script_buf: [192]u8 = undefined;
        const out = try b.onMount(try std.fmt.bufPrint(&script_buf, "mkdir -p g && dd if=/dev/zero of=g/f{d} bs=1M count=64 status=none && sync g/f{d}", .{ i, i }));
        gpa.free(out);
        const ta = try rig.stats();
        defer gpa.free(ta);
        var pos: usize = 0;
        while (std.mem.indexOfPos(u8, ta, pos, "\"open_grants\":")) |at| {
            const n = numberAfter(ta[at..], "\"open_grants\":") orelse 0;
            max_open = @max(max_open, n);
            pos = at + "\"open_grants\":".len;
        }
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"block_grant_deferred\":"));
    }
    const ta = try rig.stats();
    defer gpa.free(ta);
    const tb = try b.stats();
    defer gpa.free(tb);
    const grants = numberAfter(ta, "\"block_grants\":") orelse 0;
    const retired = numberAfter(ta, "\"block_grant_consumed_retired\":") orelse 0;
    try testing.expect(grants >= grant_ledger_row_files);
    try testing.expect(max_open <= grant_ledger_row_cap);
    // The structural bound: the table holds ≤ cap open grants, so every
    // grant past the cap had to RETIRE for `deferred` to read 0.
    try testing.expect(retired >= grant_ledger_row_files - grant_ledger_row_cap);
    try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"block_grant_deferred\":"));
    // Declarations ride the renewal beat (one per beat per volume): a
    // count, never a per-file law — the mechanism ran on both sides.
    try testing.expect((numberAfter(ta, "\"block_grant_windows_declared\":") orelse 0) >= 1);
    try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"block_grant_topup_deferred\":"));
    try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"block_grant_exhausted\":"));
    try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"block_grant_topup_refused\":"));
    try testing.expect((numberAfter(tb, "\"block_grant_windows_declared_by_writer\":") orelse 0) >= 1);
    try expectMustStayZero(ta);
    try expectMustStayZero(tb);
    {
        const from_a = try rig.onMount("ls g | wc -l; cmp g/f0 g/f23 && stat -c %s g/f23");
        defer gpa.free(from_a);
        var want_buf: [32]u8 = undefined;
        try testing.expectEqualStrings(try std.fmt.bufPrint(&want_buf, "{d}\n{d}\n", .{ grant_ledger_row_files, 64 << 20 }), from_a);
    }
    std.debug.print("\nsmoke: grant ledger: {d} grants carved for B over {d} files at a cap of {d}; consumed retired {d}, open grants peaked at {d}, deferred 0 at A; B: top-ups {d}, deferred 0, exhausted 0, declarations {d}\n", .{ grants, grant_ledger_row_files, grant_ledger_row_cap, retired, max_open, numberAfter(tb, "\"block_grant_topups\":") orelse 0, numberAfter(tb, "\"block_grant_windows_declared_by_writer\":") orelse 0 });
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

/// Poll `host`'s `.stats` until `key` reads ≥ `want` (a climbing counter)
/// or the deadline passes; answers the last tail read.
fn awaitAtLeast(host: anytype, key: []const u8, want: u64, deadline_ns: u64) ![]u8 {
    const deadline = clock.monotonicNs() + deadline_ns;
    while (true) {
        try host.expectAlive(key);
        const tail = (try host.statsTry()) orelse {
            if (clock.monotonicNs() >= deadline) return error.StatsUnreadable;
            sleepMs(200);
            continue;
        };
        if ((numberAfter(tail, key) orelse 0) >= want or clock.monotonicNs() >= deadline) return tail;
        testing.allocator.free(tail);
        sleepMs(200);
    }
}

/// The ships B lands on f's slot: past `N_floor` at the cold seed (17) and
/// twice the holder's own ops on the slot (the create and the layout
/// publish), with room for the judge's half-window rotation.
const handover_row_ships: u64 = 48;
/// Files A mints over its rotor: past the rotor's width (M ≤ MINT_SPREAD
/// = 64), so the pigeonhole puts two on one routing slot.
const handover_row_files: usize = 72;
/// The times-drain cadence IS the flush interval: held a minute away so
/// the parked stamp is still parked when the handover runs (the row ends
/// long before; `sync` and the umount barrier on their own).
const handover_row_flush_ms: u64 = 60_000;

test "smoke: THE HANDOVER DRAINS PARKED STAMPS (rung 10 item 4) — A (the manager) parks a size-neutral write's mtime on a file of slot S (the drain cadence held a minute away); B's 48 setattr ships on ANOTHER file of S dominate A's judge (holder ops read net of served ships) → A OFFERS the slot to B, pushed on B's renewal grant → B ACCEPTS over the wire → A's flush-then-transfer runs step 0: the parked stamp journaled BEFORE the gate (`meta_kv_times_handover_drained` ≥ 1, `meta_kv_times_echo_foreign_dropped` 0 on both) → B, the new holder, serves exactly the mtime A had promised; fsck clean" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    // The offer rides B's renewal beat: a short lease keeps the beat short.
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    var flush_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", try std.fmt.bufPrint(&flush_text, "{d}", .{handover_row_flush_ms}));
    try rig.formatSetWith(false, &.{});
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    // A's endpoint record on tree 0 rides the same held cadence: B dials
    // the listener by name (`--join`).
    var ep_buf: [64]u8 = undefined;
    const ep = blk: {
        const ta = try rig.stats();
        defer gpa.free(ta);
        break :blk try std.fmt.bufPrint(&ep_buf, "{s}", .{json_scan.string(ta, "wire_endpoint") orelse return error.NoWireEndpoint});
    };
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{ "--join", ep });
    _ = try b.awaitReady();
    // A's striped files over its rotor: two of them share a routing slot
    // (the rotor wraps at M); the stamp parks on ONE, B ships on the OTHER
    // (a served op on the parked file itself would commit its record and
    // fold the stamp in — the ordinary retire, nothing left to carry).
    var pair_buf: [2][8]u8 = undefined;
    var pair: [2][]const u8 = undefined;
    {
        var script_buf: [256]u8 = undefined;
        const out = try rig.onMount(try std.fmt.bufPrint(&script_buf, "mkdir d && i=0; while [ $i -lt {d} ]; do dd if=/dev/zero of=d/f$i bs=32k count=1 status=none; i=$((i+1)); done; sync d; stat -c '%n %i' d/f*", .{handover_row_files}));
        defer gpa.free(out);
        var inos_of: [handover_row_files]u64 = undefined;
        var names_of: [handover_row_files][]const u8 = undefined;
        var n: usize = 0;
        var lines = std.mem.splitScalar(u8, out, '\n');
        while (lines.next()) |line| {
            if (line.len == 0) continue;
            var cols = std.mem.splitScalar(u8, line, ' ');
            names_of[n] = cols.next().?;
            inos_of[n] = try std.fmt.parseInt(u64, cols.next().?, 10);
            n += 1;
        }
        try testing.expectEqual(@as(usize, handover_row_files), n);
        var found = false;
        outer: for (0..n) |i| for (i + 1..n) |k| if (route_table.slotOf(inos_of[i]) == route_table.slotOf(inos_of[k])) {
            pair[0] = try std.fmt.bufPrint(&pair_buf[0], "{s}", .{names_of[i]});
            pair[1] = try std.fmt.bufPrint(&pair_buf[1], "{s}", .{names_of[k]});
            found = true;
            break :outer;
        };
        try testing.expect(found);
    }
    // A size-neutral overwrite of the first on a later coarse tick: the
    // stamp PARKS (no commit) and getattr serves it folded.
    const parked0 = blk: {
        const ta = try rig.stats();
        defer gpa.free(ta);
        break :blk numberAfter(ta, "\"fuse_write_times_parked\":") orelse 0;
    };
    var mtime_buf: [40]u8 = undefined;
    const mtime_a_owned = blk: {
        var script_buf: [160]u8 = undefined;
        const out = try rig.onMount(try std.fmt.bufPrint(&script_buf, "dd if=/dev/urandom of={s} bs=512 count=1 conv=notrunc status=none && stat -c %.9Y {s}", .{ pair[0], pair[0] }));
        defer gpa.free(out);
        break :blk try std.fmt.bufPrint(&mtime_buf, "{s}", .{std.mem.trimEnd(u8, out, "\n")});
    };
    {
        const ta = try rig.stats();
        defer gpa.free(ta);
        try testing.expect((numberAfter(ta, "\"fuse_write_times_parked\":") orelse 0) >= parked0 + 1);
        try testing.expect((numberAfter(ta, "\"meta_kv_times_echo_pending\":") orelse 0) >= 1);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"slot_handovers\":"));
    }
    // B's ships on the slot's OTHER file: record verbs served at A under
    // its door.
    {
        var script_buf: [224]u8 = undefined;
        const out = try b.onMount(try std.fmt.bufPrint(&script_buf, "i=0; while [ $i -lt {d} ]; do chmod 600 {s}; chmod 644 {s}; i=$((i+2)); done; echo ok", .{ handover_row_ships, pair[1], pair[1] }));
        defer gpa.free(out);
        try testing.expectEqualStrings("ok\n", out);
    }
    // The offer, the push, B's accept, A's transfer with step 0.
    {
        const moved = try awaitAtLeast(rig, "\"slot_handovers\":", 1, kill_row_wait_ns);
        defer gpa.free(moved);
        try testing.expect((numberAfter(moved, "\"slot_handovers\":") orelse 0) >= 1);
    }
    // A's ORDINARY drain after the move (`fsyncdir` runs it): a stamp
    // step 0 had left parked would meet the moved door here and be
    // dropped counted — the counter the row holds at 0.
    {
        const out = try rig.onMount("sync d");
        gpa.free(out);
    }
    const ta = try rig.stats();
    defer gpa.free(ta);
    std.debug.print("\nsmoke: handover row at A — record_served {d}, slot_ships {d}, offers {d} (idle {d}, dominated {d}, busy {d}, pushed {d}), handovers {d}, n_floor {d}, times pending {d}\n", .{ numberAfter(ta, "\"record_served\":") orelse 0, numberAfter(ta, "\"slot_ships\":") orelse 0, numberAfter(ta, "\"slot_offers\":") orelse 0, numberAfter(ta, "\"slot_offers_idle\":") orelse 0, numberAfter(ta, "\"slot_offers_dominated\":") orelse 0, numberAfter(ta, "\"slot_offers_busy\":") orelse 0, numberAfter(ta, "\"slot_offers_pushed\":") orelse 0, numberAfter(ta, "\"slot_handovers\":") orelse 0, numberAfter(ta, "\"slot_offer_n_floor\":") orelse 0, numberAfter(ta, "\"meta_kv_times_echo_pending\":") orelse 0 });
    try testing.expect((numberAfter(ta, "\"slot_handovers\":") orelse 0) >= 1);
    try testing.expect((numberAfter(ta, "\"slot_offers_pushed\":") orelse 0) >= 1);
    try testing.expect((numberAfter(ta, "\"slot_offers_dominated\":") orelse 0) >= 1);
    try testing.expect((numberAfter(ta, "\"meta_kv_times_handover_drained\":") orelse 0) >= 1);
    try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"meta_kv_times_echo_foreign_dropped\":"));
    try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"meta_kv_times_echo_pending\":"));
    try expectMustStayZero(ta);
    // B's accept is witnessed by A's transfer and grant above and by the
    // mtime B serves below (its accepted-offer counter follows the
    // publication of the installed lease, which may defer under load).
    const tb = try b.stats();
    defer gpa.free(tb);
    try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"meta_kv_times_echo_foreign_dropped\":"));
    try expectMustStayZero(tb);
    // The promise kept: the new holder serves A's parked mtime off the
    // record step 0 journaled; A reads the same under B's token.
    {
        var script_buf: [64]u8 = undefined;
        const script = try std.fmt.bufPrint(&script_buf, "stat -c %.9Y {s}", .{pair[0]});
        const from_b = try b.onMount(script);
        defer gpa.free(from_b);
        try testing.expectEqualStrings(mtime_a_owned, std.mem.trimEnd(u8, from_b, "\n"));
        const from_a = try rig.onMount(script);
        defer gpa.free(from_a);
        try testing.expectEqualStrings(mtime_a_owned, std.mem.trimEnd(u8, from_a, "\n"));
    }
    std.debug.print("\nsmoke: handover drains parked stamps: {d} ships served at A (slot_ships {d}), offers {d} (dominated {d}, pushed {d}), handovers {d}; step 0 drained {d} stamp(s), foreign-dropped 0 on both; B accepted {d}; mtime {s} served by B after the move\n", .{ handover_row_ships, numberAfter(ta, "\"slot_ships\":") orelse 0, numberAfter(ta, "\"slot_offers\":") orelse 0, numberAfter(ta, "\"slot_offers_dominated\":") orelse 0, numberAfter(ta, "\"slot_offers_pushed\":") orelse 0, numberAfter(ta, "\"slot_handovers\":") orelse 0, numberAfter(ta, "\"meta_kv_times_handover_drained\":") orelse 0, numberAfter(tb, "\"joined_wire_offers_accepted\":") orelse 0, mtime_a_owned });
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the handover row");
}

/// The shared-directory matrix's third daemon runs as another node (its
/// own UTS namespace, like the kill rows' B).
const stripe_peer_node_c = "sqz-nodec";
/// The storm's grain: how many creates a phase lets land past the counter
/// it waits for before the storm is stopped — enough that the merged
/// listing spans several readdir pages per stripe, small enough that the
/// three-way `ls`/`find`/`stat` comparison stays inside the stage's bound.
const stripe_matrix_phase_floor: u64 = 400;

/// A background create storm into `hot/` under the mount `$1`, names
/// `<prefix>$i`, until the stop sentinel `$2` exists; a refused create
/// (EAGAIN while the directory's holder is dead or mid-move) is retried
/// after a short pause; a retry that meets EEXIST landed the first time
/// (the ship applied, the reply did not return) and counts once; the
/// count of landed names is written to `$3` at the end. `true >` rather
/// than `: >`: a redirection failure on a SPECIAL builtin exits a POSIX
/// shell. The loop also ends when the test process (`$PPID` at spawn) is
/// gone: a runner killed past every defer must not leave a writer
/// spinning on a detached mount (the sentinel lives in the rig's work
/// directory, which the rig's teardown removes).
const stripe_storm_script = "p=$PPID; cd \"$1\" || exit 1; i=0; while [ ! -e \"$2\" ] && kill -0 \"$p\" 2>/dev/null; do if true > \"hot/$4$i\" 2>/dev/null || [ -e \"hot/$4$i\" ]; then i=$((i + 1)); else sleep 0.05; fi; done; echo $i > \"$3\"";

/// One storm's handles: the child (the leader of its own process group,
/// so a teardown reaches its `sleep` too), its sentinel and count paths.
/// `deinit` kills a storm `finish` did not reap: every exit path of the
/// row (success, a failed expectation, a timeout) ends its writers.
const Storm = struct {
    child: std.process.Child,
    stop: []const u8,
    count: []const u8,
    err_file: Io.File,

    fn start(rig: *Rig, mp: []const u8, tag: []const u8, prefix: []const u8) !Storm {
        const gpa = rig.gpa;
        const stop = try std.fmt.allocPrint(gpa, "{s}/storm-{s}.stop", .{ rig.work, tag });
        errdefer gpa.free(stop);
        const count = try std.fmt.allocPrint(gpa, "{s}/storm-{s}.count", .{ rig.work, tag });
        errdefer gpa.free(count);
        const err_path = try std.fmt.allocPrint(gpa, "{s}/storm-{s}.err", .{ rig.work, tag });
        defer gpa.free(err_path);
        Io.Dir.cwd().deleteFile(rig.io, stop) catch {};
        Io.Dir.cwd().deleteFile(rig.io, count) catch {};
        const err_file = try Io.Dir.cwd().createFile(rig.io, err_path, .{ .truncate = true });
        errdefer err_file.close(rig.io);
        const child = try std.process.spawn(rig.io, .{ .argv = &.{ "sh", "-c", stripe_storm_script, "storm", mp, stop, count, prefix }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .{ .file = err_file }, .pgid = 0 });
        return .{ .child = child, .stop = stop, .count = count, .err_file = err_file };
    }

    /// Raise the sentinel, wait the child out, read its count (a storm
    /// that died early names its cause on stderr, printed here).
    fn finish(self: *Storm, rig: *Rig) !u64 {
        const f = try Io.Dir.cwd().createFile(rig.io, self.stop, .{ .truncate = true });
        f.close(rig.io);
        const term = try self.child.wait(rig.io);
        const text = Io.Dir.cwd().readFileAlloc(rig.io, self.count, rig.gpa, .limited(64)) catch |e| {
            var err_path_buf: [256]u8 = undefined;
            const err_path = try std.fmt.bufPrint(&err_path_buf, "{s}.err", .{self.stop[0 .. self.stop.len - ".stop".len]});
            const err_text = Io.Dir.cwd().readFileAlloc(rig.io, err_path, rig.gpa, .limited(4096)) catch "";
            defer if (err_text.len != 0) rig.gpa.free(err_text);
            std.debug.print("\n--- storm {s} left no count ({f}; {t}) ---\n{s}\n", .{ self.stop, term, e, err_text });
            return e;
        };
        defer rig.gpa.free(text);
        return std.fmt.parseInt(u64, std.mem.trim(u8, text, " \n"), 10) catch error.StormCountUnparsed;
    }

    fn deinit(self: *Storm, rig: *Rig) void {
        if (self.child.id) |pid| _ = linux.kill(-pid, .KILL);
        self.child.kill(rig.io);
        self.err_file.close(rig.io);
        rig.gpa.free(self.stop);
        rig.gpa.free(self.count);
    }
};

/// Poll `host`'s stats until `key` reads exactly `want` (owned tail).
fn awaitExactly(host: anytype, key: []const u8, want: u64, deadline_ns: u64) ![]u8 {
    const deadline = clock.monotonicNs() + deadline_ns;
    while (true) {
        try host.expectAlive(key);
        const tail = (try host.statsTry()) orelse {
            if (clock.monotonicNs() >= deadline) return error.StatsUnreadable;
            sleepMs(200);
            continue;
        };
        if ((numberAfter(tail, key) orelse std.math.maxInt(u64)) == want or clock.monotonicNs() >= deadline) return tail;
        testing.allocator.free(tail);
        sleepMs(200);
    }
}

/// The three-way view of `hot/`: the count of names `ls` lists, the count
/// of files `find` reaches and a digest of `stat(2)` of every entry
/// (name, ino, links, size) — equal on every daemon when the striped
/// directory is one directory. The listings land in the work directory
/// (`$1`) first so a failed `ls`/`find` fails the script (`sh -e` judges a
/// pipeline by its last command) with its stderr instead of counting 0;
/// each daemon's under its own `tag`, so a disagreement can be shown
/// (`reportHotViews`).
fn hotView(host: anytype, tag: []const u8) ![]u8 {
    var buf: [640]u8 = undefined;
    const script = try std.fmt.bufPrint(&buf, "ls hot > \"$1/view-{0s}.ls\"; wc -l < \"$1/view-{0s}.ls\"; find hot -type f > \"$1/view-{0s}.find\"; wc -l < \"$1/view-{0s}.find\"; find hot -type f -exec stat -c '%n %i %h %s' {{}} + > \"$1/view-{0s}.stat\"; sort \"$1/view-{0s}.stat\" | sha256sum | cut -d' ' -f1; stat -c %h hot", .{tag});
    return host.onMountTimed(script, 300);
}

/// Wait for `host`'s open cross-owner intents to reach 0 (owned tail); a
/// wait that ends above 0 names the cross-owner ledger first.
fn awaitIntentsClosed(host: anytype, tag: []const u8) ![]u8 {
    const tail = try awaitExactly(host, "\"xv_cross_owner_intents_open\":", 0, 2 * kill_row_wait_ns);
    if ((numberAfter(tail, "\"xv_cross_owner_intents_open\":") orelse 0) != 0) {
        std.debug.print("\n--- {s}: cross-owner intents still open ---\n", .{tag});
        for ([_][]const u8{ "xv_cross_owner_intents_open", "xv_cross_owner_intents_stuck", "xv_cross_owner_intents_minted", "xv_cross_owner_intents_retired", "xv_started", "xv_completed", "xv_recovered", "xv_compensated", "xv_recovered_compensated", "xv_abandoned", "xv_cadence_passes", "xv_held_open", "xv_resumed", "xv_resumed_completed", "xv_resumed_gone", "xv_applied_attested", "slot_move_unresolved", "slot_move_reissues", "slot_door_coherence_refusals", "appender_recoveries", "membership_evictions", "recovery_ledger_polls", "recovery_intents_adopted", "xv_cross_owner_guards_parked", "xv_cross_owner_guards_swept_dead", "fuse_op_watchdog_overdue", "dir_stripe_migrated_names", "dir_stripe_migration_rounds" }) |k| {
            var kbuf: [64]u8 = undefined;
            std.debug.print("{s} {?d}\n", .{ k, numberAfter(tail, std.fmt.bufPrint(&kbuf, "\"{s}\":", .{k}) catch unreachable) });
        }
    }
    return tail;
}

/// One daemon's view for `reportHotViews`: its tag and mountpoint.
const HotViewOf = struct { tag: []const u8, mp: []const u8 };

/// A disagreement between views — two daemons', or `ls` against `find` on
/// one — named before the row fails: per daemon the names `ls` lists twice
/// or `find` does not reach (and the reverse), the names one daemon lists
/// and another does not, and `stat` of each such name on every mount now.
fn reportHotViews(rig: *Rig, views: []const HotViewOf) void {
    var script: std.Io.Writer.Allocating = .init(rig.gpa);
    defer script.deinit();
    const w = &script.writer;
    w.writeAll("odd=\"\"; for t in") catch return;
    for (views) |v| w.print(" {s}", .{v.tag}) catch return;
    w.writeAll("; do sort view-$t.ls > sorted-$t.ls; sed 's|^hot/||' view-$t.find | sort > sorted-$t.find; d=$(uniq -d sorted-$t.ls | head -20 | tr '\\n' ' '); lo=$(comm -23 sorted-$t.ls sorted-$t.find | head -20 | tr '\\n' ' '); fo=$(comm -13 sorted-$t.ls sorted-$t.find | head -20 | tr '\\n' ' '); echo \"$t: ls $(wc -l < sorted-$t.ls), find $(wc -l < sorted-$t.find); listed twice by ls: $d; ls not find: $lo; find not ls: $fo\"; odd=\"$odd $d $lo $fo\"; done; ") catch return;
    if (views.len >= 2) {
        w.print("for t in", .{}) catch return;
        for (views[1..]) |v| w.print(" {s}", .{v.tag}) catch return;
        w.print("; do uniq sorted-{0s}.ls > u-{0s}; uniq sorted-$t.ls > u-$t; x=$(comm -23 u-{0s} u-$t | head -20 | tr '\\n' ' '); y=$(comm -13 u-{0s} u-$t | head -20 | tr '\\n' ' '); echo \"{0s} lists, $t does not: $x; $t lists, {0s} does not: $y\"; odd=\"$odd $x $y\"; done; ", .{views[0].tag}) catch return;
    }
    w.writeAll("for n in $(echo $odd | tr ' ' '\\n' | sort -u); do for m in") catch return;
    for (views) |v| w.print(" {s}", .{v.mp}) catch return;
    w.writeAll("; do echo \"$m: $(stat -c '%n %i %h %s %Y' \"$m/hot/$n\" 2>&1)\"; done; done") catch return;
    const r = rig.sh(rig.work, script.written()) catch return;
    defer rig.gpa.free(r.stderr);
    defer rig.gpa.free(r.stdout);
    std.debug.print("\n--- hot views disagree ---\n{s}{s}\n", .{ r.stdout, r.stderr });
}

/// `ls` = `find` on one view (its first two lines).
fn lsEqualsFind(view: []const u8) bool {
    var lines = std.mem.splitScalar(u8, view, '\n');
    const ls_n = std.mem.trim(u8, lines.next() orelse return false, " ");
    const find_n = std.mem.trim(u8, lines.next() orelse return false, " ");
    return std.mem.eql(u8, ls_n, find_n);
}

test "smoke: THE SHARED HOT DIRECTORY ACROSS DAEMONS — A (the manager) mkdirs `hot`; B JOINED storms creates into it ⇒ A's judge: B dominates past N_floor, the OFFER rides B's renewal grant, B accepts, the slot MOVES (slot_offers_pushed ≥ 1, slot_handovers 1 at A; A's census flips nothing — one creator) → A and a THIRD daemon C storm into `hot`, now B's ⇒ two creators past N_floor: B FLIPS it over K = 64 stripes, 21 supplied by A and 21 by C over `SupplyStripeInos` (one batch each), 22 minted by B; every later create keys a stripe and ships into a striping domain feed no window (handovers stay 1) → `ls | wc -l`, `find` and the (name, ino) digest of every entry EQUAL from A, B and C → kill -9 B (the holder of `hot` and 22 stripes) mid-storm ⇒ A evicts B at T_owner and recovers its region (hot's slot and B's stripes the manager's), every open intent rolls forward (stuck 0), the storms continue → the view equal from A and C → kill -9 C (a SUPPLIER: 21 stripes are records in its slot) mid-storm, C storming too (it dies holding guard scopes parked at A) ⇒ A evicts it, sweeps its scopes and recovers its region, the supplied stripes stand, every name listed → umount A → fsck --offline --scrub 0 findings (C17 clean: no mapped stripe lost)" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSetWith(false, &.{});
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.node = kill_peer_node;
    try b.spawn(&.{});
    _ = try b.awaitReady();
    const c = try Peer.init(rig, "c");
    defer c.deinit();
    c.node = stripe_peer_node_c;
    try c.spawn(&.{});
    _ = try c.awaitReady();
    {
        const mk = try rig.onMount("mkdir hot");
        gpa.free(mk);
    }
    // Phase 1: B alone storms into A's directory — every create ONE
    // shipped insert served at A under its door; B dominates the slot's
    // window past N_floor, A offers, the offer rides B's renewal, B
    // accepts, A's flush-then-transfer moves the slot. One creator feeds
    // the census and never flips.
    var storm_b = try Storm.start(rig, b.mp, "b", "b");
    defer storm_b.deinit(rig);
    const t_phase1 = clock.monotonicNs();
    {
        const moved = try awaitAtLeast(rig, "\"slot_handovers\":", 1, kill_row_wait_ns);
        defer gpa.free(moved);
        try testing.expect((numberAfter(moved, "\"slot_handovers\":") orelse 0) >= 1);
        // B's acceptance is the grant that follows A's transfer: the probe
        // below must see B HOLDING the slot (a touch by A between the
        // release and the grant would first-touch it back).
        const accepted = try awaitAtLeast(b, "\"joined_wire_offers_accepted\":", 1, kill_row_wait_ns);
        defer gpa.free(accepted);
        try testing.expect((numberAfter(accepted, "\"joined_wire_offers_accepted\":") orelse 0) >= 1);
    }
    const nb = try storm_b.finish(rig);
    const handover_ms = (clock.monotonicNs() - t_phase1) / std.time.ns_per_ms;
    var offers_pushed: u64 = 0;
    var handovers: u64 = 0;
    {
        const ta = try rig.stats();
        defer gpa.free(ta);
        offers_pushed = numberAfter(ta, "\"slot_offers_pushed\":") orelse 0;
        handovers = numberAfter(ta, "\"slot_handovers\":") orelse 0;
        try testing.expect(offers_pushed >= 1);
        try testing.expect(handovers >= 1);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"dir_stripe_flips\":"));
        try testing.expect((numberAfter(ta, "\"xv_cross_owner_steps_served\":") orelse 0) >= 1);
    }
    // The directory's holder is B now: a create from A ships to B.
    {
        const steps_b0 = blk: {
            const tb = try b.stats();
            defer gpa.free(tb);
            break :blk numberAfter(tb, "\"xv_cross_owner_steps_served\":") orelse 0;
        };
        const out = try rig.onMount(": > hot/probe-a && echo ok");
        defer gpa.free(out);
        try testing.expectEqualStrings("ok\n", out);
        const tb = try b.stats();
        defer gpa.free(tb);
        const ta = try rig.stats();
        defer gpa.free(ta);
        std.debug.print("\nsmoke: shared hot dir: after the move — A: handovers {d}, leases held {d}, steps served {d}, intents minted {d}, routed mutations {d}; B: steps served {d} (was {d}), leases held {d}, offers accepted {d}, slot acquires {d}, intents minted {d}\n", .{ numberAfter(ta, "\"slot_handovers\":") orelse 0, numberAfter(ta, "\"slot_leases_held\":") orelse 0, numberAfter(ta, "\"xv_cross_owner_steps_served\":") orelse 0, numberAfter(ta, "\"xv_cross_owner_intents_minted\":") orelse 0, numberAfter(ta, "\"dir_stripe_routed_mutations\":") orelse 0, numberAfter(tb, "\"xv_cross_owner_steps_served\":") orelse 0, steps_b0, numberAfter(tb, "\"slot_leases_held\":") orelse 0, numberAfter(tb, "\"joined_wire_offers_accepted\":") orelse 0, numberAfter(tb, "\"slot_acquires\":") orelse 0, numberAfter(tb, "\"xv_cross_owner_intents_minted\":") orelse 0 });
        try testing.expect((numberAfter(tb, "\"xv_cross_owner_steps_served\":") orelse 0) > steps_b0);
    }
    std.debug.print("\nsmoke: shared hot dir: phase 1 — B's {d} creates moved the slot in {d} ms (offers pushed {d}, handovers {d}, flips at A 0)\n", .{ nb, handover_ms, offers_pushed, handovers });
    // Phase 2: A and C storm into `hot`, B's now — two creators past
    // N_floor at B's census ⇒ the flip at B: A and C each supply their
    // share over the wire, B mints the rest; the stripes land in their
    // suppliers' slots on the directory's volume.
    var storm_a = try Storm.start(rig, rig.mp, "a", "a");
    defer storm_a.deinit(rig);
    var storm_c = try Storm.start(rig, c.mp, "c", "c");
    defer storm_c.deinit(rig);
    const t_phase2 = clock.monotonicNs();
    {
        const flipped = try awaitAtLeast(b, "\"dir_stripe_flips\":", 1, kill_row_wait_ns);
        defer gpa.free(flipped);
        if ((numberAfter(flipped, "\"dir_stripe_flips\":") orelse 0) != 1) {
            const ta = try rig.stats();
            defer gpa.free(ta);
            const tc = try c.stats();
            defer gpa.free(tc);
            std.debug.print("\nno flip — B: steps served {d}, census dirs {d}, flip failures {d}, n_floor {d}, leases held {d}; A: intents minted {d}, steps shipped {d}, open {d}, stuck {d}; C: intents minted {d}, steps shipped {d}, open {d}\n", .{ numberAfter(flipped, "\"xv_cross_owner_steps_served\":") orelse 0, numberAfter(flipped, "\"dir_stripe_census_dirs\":") orelse 0, numberAfter(flipped, "\"dir_stripe_flip_failures\":") orelse 0, numberAfter(flipped, "\"slot_offer_n_floor\":") orelse 0, numberAfter(flipped, "\"slot_leases_held\":") orelse 0, numberAfter(ta, "\"xv_cross_owner_intents_minted\":") orelse 0, numberAfter(ta, "\"xv_cross_owner_steps_shipped\":") orelse 0, numberAfter(ta, "\"xv_cross_owner_intents_open\":") orelse 0, numberAfter(ta, "\"xv_cross_owner_intents_stuck\":") orelse 0, numberAfter(tc, "\"xv_cross_owner_intents_minted\":") orelse 0, numberAfter(tc, "\"xv_cross_owner_steps_shipped\":") orelse 0, numberAfter(tc, "\"xv_cross_owner_intents_open\":") orelse 0 });
        }
        try testing.expectEqual(@as(?u64, 1), numberAfter(flipped, "\"dir_stripe_flips\":"));
    }
    const flip_ms = (clock.monotonicNs() - t_phase2) / std.time.ns_per_ms;
    // Let the storms run on past the flip: every later create keys its
    // stripe (routed at the creator off B's token — a ship to the
    // stripe's holder, or local when the stripe is the creator's own).
    {
        const routed = try awaitAtLeast(rig, "\"dir_stripe_routed_mutations\":", stripe_matrix_phase_floor, kill_row_wait_ns);
        defer gpa.free(routed);
        const routed_c = try awaitAtLeast(c, "\"dir_stripe_routed_mutations\":", stripe_matrix_phase_floor, kill_row_wait_ns);
        defer gpa.free(routed_c);
    }
    const na1 = try storm_a.finish(rig);
    const nc1 = try storm_c.finish(rig);
    const share = sym_xv.stripe.supplyShare(sym_xv.stripe.K_MAX, 2);
    {
        const tb = try b.stats();
        defer gpa.free(tb);
        try testing.expectEqual(@as(?u64, 1), numberAfter(tb, "\"dir_stripe_flips\":"));
        try testing.expectEqual(@as(?u64, 1), numberAfter(tb, "\"dir_striped_dirs\":"));
        try testing.expectEqual(@as(?u64, 2), numberAfter(tb, "\"dir_stripe_supply_rpcs\":"));
        try testing.expectEqual(@as(?u64, 2 * share), numberAfter(tb, "\"dir_stripe_supplied\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"dir_stripe_supply_declined\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tb, "\"dir_stripe_dying_refusals\":"));
        const ta = try rig.stats();
        defer gpa.free(ta);
        const tc = try c.stats();
        defer gpa.free(tc);
        try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"dir_stripe_supplies_served\":"));
        try testing.expectEqual(@as(?u64, 1), numberAfter(tc, "\"dir_stripe_supplies_served\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"dir_stripe_flips\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(tc, "\"dir_stripe_flips\":"));
        // Ships into a striping domain feed no window: no further handover.
        try testing.expectEqual(handovers, numberAfter(ta, "\"slot_handovers\":") orelse 0);
        try testing.expect((numberAfter(ta, "\"dir_stripe_ships\":") orelse 0) >= 1);
        std.debug.print("smoke: shared hot dir: phase 2 — the flip at B {d} ms after A and C's storms began: K {d}, supplied {d} ({d} from A + {d} from C in one batch each), minted by B {d}; A created {d}, C {d}; handovers still {d}; stripe ships from A {d}, from C {d}\n", .{ flip_ms, sym_xv.stripe.K_MAX, numberAfter(tb, "\"dir_stripe_supplied\":") orelse 0, share, share, sym_xv.stripe.K_MAX - 2 * share, na1, nc1, numberAfter(ta, "\"slot_handovers\":") orelse 0, numberAfter(ta, "\"dir_stripe_ships\":") orelse 0, numberAfter(tc, "\"dir_stripe_ships\":") orelse 0 });
    }
    // The view from every daemon: one directory.
    const total1 = nb + na1 + nc1 + 1;
    {
        const va = try hotView(rig, "a");
        defer gpa.free(va);
        const vb = try hotView(b, "b");
        defer gpa.free(vb);
        const vc = try hotView(c, "c");
        defer gpa.free(vc);
        if (!std.mem.eql(u8, va, vb) or !std.mem.eql(u8, va, vc) or !lsEqualsFind(va)) reportHotViews(rig, &.{ .{ .tag = "a", .mp = rig.mp }, .{ .tag = "b", .mp = b.mp }, .{ .tag = "c", .mp = c.mp } });
        try testing.expectEqualStrings(va, vb);
        try testing.expectEqualStrings(va, vc);
        var lines = std.mem.splitScalar(u8, va, '\n');
        var nbuf: [16]u8 = undefined;
        try testing.expectEqualStrings(try std.fmt.bufPrint(&nbuf, "{d}", .{total1}), std.mem.trim(u8, lines.next().?, " "));
        try testing.expectEqualStrings(try std.fmt.bufPrint(&nbuf, "{d}", .{total1}), std.mem.trim(u8, lines.next().?, " "));
        std.debug.print("smoke: shared hot dir: {d} entries — ls, find and the (name, ino) digest equal from A, B and C\n", .{total1});
    }
    // Phase 3: kill -9 B — the holder of `hot` and 22 stripes — with A's
    // and C's storms running; A evicts B at T_owner and recovers its
    // region (the slot and the stripes are the manager's now), the
    // storms' refused creates retry, every open intent rolls forward.
    var storm_a2 = try Storm.start(rig, rig.mp, "a2", "x");
    defer storm_a2.deinit(rig);
    var storm_c2 = try Storm.start(rig, c.mp, "c2", "y");
    defer storm_c2.deinit(rig);
    {
        const warm = try awaitAtLeast(c, "\"dir_stripe_routed_mutations\":", stripe_matrix_phase_floor + 50, kill_row_wait_ns);
        gpa.free(warm);
    }
    const t_kill = clock.monotonicNs();
    try b.kill();
    const ta_rec = try awaitStats(rig, &.{ .{ .key = "\"appender_recoveries\":", .want = 1 }, .{ .key = "\"recovered_regions_released\":", .want = 1 } }, kill_row_wait_ns);
    defer gpa.free(ta_rec);
    const recovered_ms = (clock.monotonicNs() - t_kill) / std.time.ns_per_ms;
    try testing.expectEqual(@as(?u64, 1), numberAfter(ta_rec, "\"membership_evictions\":"));
    try testing.expectEqual(@as(?u64, 1), numberAfter(ta_rec, "\"appender_recoveries\":"));
    try testing.expectEqual(@as(?u64, 0), numberAfter(ta_rec, "\"appender_recovery_rollbacks\":"));
    {
        const settled = try awaitAtLeast(rig, "\"dir_stripe_routed_mutations\":", (numberAfter(ta_rec, "\"dir_stripe_routed_mutations\":") orelse 0) + 50, kill_row_wait_ns);
        gpa.free(settled);
    }
    const na2 = try storm_a2.finish(rig);
    const nc2 = try storm_c2.finish(rig);
    // Every intent the dead holder left open at its creators rolls forward
    // once the slots are the manager's: `open` returns to 0 on both, and
    // none stood open past the 45 s membership TTL (`xv_cross_owner_
    // intent_stuck` is a must-stay-0 tripwire at each creator).
    const t_open = clock.monotonicNs();
    {
        const oa = try awaitIntentsClosed(rig, "a");
        defer gpa.free(oa);
        try testing.expectEqual(@as(?u64, 0), numberAfter(oa, "\"xv_cross_owner_intents_open\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(oa, "\"xv_cross_owner_intents_stuck\":"));
        const oc = try awaitIntentsClosed(c, "c");
        defer gpa.free(oc);
        try testing.expectEqual(@as(?u64, 0), numberAfter(oc, "\"xv_cross_owner_intents_open\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(oc, "\"xv_cross_owner_intents_stuck\":"));
    }
    const open_ms = (clock.monotonicNs() - t_open) / std.time.ns_per_ms;
    // The view from the survivors: the same directory, every entry that
    // landed present (the recovered stripes read locally at A, under A's
    // tokens at C).
    {
        const va = try hotView(rig, "a");
        defer gpa.free(va);
        const vc = try hotView(c, "c");
        defer gpa.free(vc);
        if (!std.mem.eql(u8, va, vc) or !lsEqualsFind(va)) reportHotViews(rig, &.{ .{ .tag = "a", .mp = rig.mp }, .{ .tag = "c", .mp = c.mp } });
        try testing.expectEqualStrings(va, vc);
        var lines = std.mem.splitScalar(u8, va, '\n');
        const listed = try std.fmt.parseInt(u64, std.mem.trim(u8, lines.next().?, " "), 10);
        // Every counted name is listed; at most one more per storm — a
        // create refused EAGAIN after it landed (the reply lost to the
        // death) is counted only when the retry meets it, and a storm
        // stopped between the two leaves it uncounted.
        try testing.expect(listed >= total1 + na2 + nc2 and listed <= total1 + na2 + nc2 + 2);
        try testing.expectEqualStrings(std.mem.trim(u8, lines.next().?, " "), std.mem.trim(u8, std.mem.sliceTo(va, '\n'), " "));
        std.debug.print("smoke: shared hot dir: phase 3 — B killed; A recovered its region {d} ms after (evictions 1, recoveries 1, rollbacks 0); A created {d} and C {d} more across the death (retried, never counted twice); {d} entries listed (ls = find), digests equal from A and C; open intents 0 on both {d} ms after the storms stopped, stuck 0\n", .{ recovered_ms, na2, nc2, listed, open_ms });
    }
    // Phase 4: kill -9 C — a SUPPLIER: 21 of `hot`'s stripes are records
    // in C's slot, minted at its `SupplyStripeInos` and durable before its
    // reply. A evicts C and recovers its region; the supplied stripes
    // stand as the manager's: A's storm keeps keying into them, every
    // name stays listed, and the offline census finds every mapped stripe's
    // record (C17's missing-stripe is the loss this phase guards).
    var storm_a3 = try Storm.start(rig, rig.mp, "a3", "z");
    defer storm_a3.deinit(rig);
    // C storms too, through its kill: it dies holding travelling guard
    // scopes parked at A (its creates into A's stripes) and A must still
    // evict it, sweep the scopes and recover its region.
    var storm_c3 = try Storm.start(rig, c.mp, "c3", "w");
    defer storm_c3.deinit(rig);
    {
        const tc = try c.stats();
        defer gpa.free(tc);
        try testing.expectEqual(@as(?u64, 1), numberAfter(tc, "\"dir_stripe_supplies_served\":"));
        const ta = try rig.stats();
        defer gpa.free(ta);
        const warm = try awaitAtLeast(rig, "\"dir_stripe_routed_mutations\":", (numberAfter(ta, "\"dir_stripe_routed_mutations\":") orelse 0) + 50, kill_row_wait_ns);
        gpa.free(warm);
    }
    const t_kill_c = clock.monotonicNs();
    try c.kill();
    // C's creates fail on its detached mount from here; the count is what
    // landed before the kill.
    const nc3 = try storm_c3.finish(rig);
    const ta_rec2 = try awaitStats(rig, &.{ .{ .key = "\"appender_recoveries\":", .want = 2 }, .{ .key = "\"recovered_regions_released\":", .want = 2 } }, kill_row_wait_ns);
    defer gpa.free(ta_rec2);
    const recovered_c_ms = (clock.monotonicNs() - t_kill_c) / std.time.ns_per_ms;
    try testing.expectEqual(@as(?u64, 2), numberAfter(ta_rec2, "\"membership_evictions\":"));
    try testing.expectEqual(@as(?u64, 0), numberAfter(ta_rec2, "\"appender_recovery_rollbacks\":"));
    {
        const settled = try awaitAtLeast(rig, "\"dir_stripe_routed_mutations\":", (numberAfter(ta_rec2, "\"dir_stripe_routed_mutations\":") orelse 0) + 50, kill_row_wait_ns);
        gpa.free(settled);
    }
    const na3 = try storm_a3.finish(rig);
    {
        const oa = try awaitIntentsClosed(rig, "a");
        defer gpa.free(oa);
        try testing.expectEqual(@as(?u64, 0), numberAfter(oa, "\"xv_cross_owner_intents_open\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(oa, "\"xv_cross_owner_intents_stuck\":"));
        const va = try hotView(rig, "a");
        defer gpa.free(va);
        if (!lsEqualsFind(va)) reportHotViews(rig, &.{.{ .tag = "a", .mp = rig.mp }});
        var lines = std.mem.splitScalar(u8, va, '\n');
        const listed = try std.fmt.parseInt(u64, std.mem.trim(u8, lines.next().?, " "), 10);
        // Phase 3's names (≤ 2 uncounted), A's and C's new ones (≤ 1 more
        // each: C's create in flight at its kill lands if its step reached
        // A before the death).
        try testing.expect(listed >= total1 + na2 + nc2 + na3 + nc3 and listed <= total1 + na2 + nc2 + na3 + nc3 + 4);
        try testing.expectEqualStrings(std.mem.trim(u8, lines.next().?, " "), std.mem.trim(u8, std.mem.sliceTo(va, '\n'), " "));
        std.debug.print("smoke: shared hot dir: phase 4 — C (a supplier of 21 stripes) killed mid-storm; A recovered its region {d} ms after (evictions 2, recoveries 2, rollbacks 0); A created {d} more and C {d} before its kill; {d} entries listed (ls = find); open intents 0\n", .{ recovered_c_ms, na3, nc3, listed });
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the shared hot directory row");
}

test "smoke: A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY (rung 10 recovery item 6) — A (the manager) mkdirs `hot` and flips it over K = 64 stripes (the explicit command: A holds the map and every stripe); B JOINED storms creates into it (every one shipped to A, keyed to its stripe) → kill -9 A mid-storm ⇒ B PARKS at T_self (park expiries 0) → remount A (the D0 ladder's same-host successor, term 2; its mount replays its own region — the map, the stripes, the shipped inserts) → B's reclaim lands in the grace window, the storm resumes into the same stripes → `ls` = `find` and the stat digest equal from A and B, every counted name listed → umount B, A → fsck --offline --scrub 0 findings (C17 clean)" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSetWith(false, &.{});
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.node = kill_peer_node;
    try b.spawn(&.{});
    _ = try b.awaitReady();
    {
        const mk = try rig.onMount("mkdir hot");
        gpa.free(mk);
        var sbuf: [256]u8 = undefined;
        const out = try rig.onMount(try setStripesScript(&sbuf, "hot", 64));
        gpa.free(out);
        const ta = try rig.stats();
        defer gpa.free(ta);
        try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"dir_stripe_flips\":"));
    }
    var storm_b = try Storm.start(rig, b.mp, "b", "b");
    defer storm_b.deinit(rig);
    {
        const warm = try awaitAtLeast(b, "\"dir_stripe_routed_mutations\":", stripe_matrix_phase_floor, kill_row_wait_ns);
        gpa.free(warm);
    }
    const t_kill = clock.monotonicNs();
    try rig.kill();
    _ = try b.awaitLog("appender park: T_self reached with the home volume 0's manager unreachable — parked", 0, kill_row_wait_ns);
    const parked_ms = (clock.monotonicNs() - t_kill) / std.time.ns_per_ms;
    const mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[mark..], "appender 0 Live as the manager (term 2") != null);
    }
    const reclaimed = try awaitStats(b, &.{.{ .key = "\"appender_parked\":", .want = 0 }}, kill_row_wait_ns);
    defer gpa.free(reclaimed);
    const reclaim_ms = (clock.monotonicNs() - t_kill) / std.time.ns_per_ms;
    try testing.expectEqual(@as(?u64, 0), numberAfter(reclaimed, "\"appender_park_expiries\":"));
    {
        const settled = try awaitAtLeast(b, "\"dir_stripe_routed_mutations\":", (numberAfter(reclaimed, "\"dir_stripe_routed_mutations\":") orelse 0) + 50, kill_row_wait_ns);
        gpa.free(settled);
    }
    const nb = try storm_b.finish(rig);
    {
        const ob = try awaitExactly(b, "\"xv_cross_owner_intents_open\":", 0, 2 * kill_row_wait_ns);
        defer gpa.free(ob);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ob, "\"xv_cross_owner_intents_stuck\":"));
        const va = try hotView(rig, "a");
        defer gpa.free(va);
        const vb = try hotView(b, "b");
        defer gpa.free(vb);
        if (!std.mem.eql(u8, va, vb) or !lsEqualsFind(va)) reportHotViews(rig, &.{ .{ .tag = "a", .mp = rig.mp }, .{ .tag = "b", .mp = b.mp } });
        try testing.expectEqualStrings(va, vb);
        var lines = std.mem.splitScalar(u8, va, '\n');
        const listed = try std.fmt.parseInt(u64, std.mem.trim(u8, lines.next().?, " "), 10);
        // Every counted name; at most one more (a create whose reply the
        // death lost, counted only when its retry met it).
        try testing.expect(listed >= nb and listed <= nb + 1);
        try testing.expectEqualStrings(std.mem.trim(u8, lines.next().?, " "), std.mem.trim(u8, std.mem.sliceTo(va, '\n'), " "));
        std.debug.print("\nsmoke: dead manager holding a striped dir: B parked {d} ms after A's kill, reclaimed {d} ms after it (park expiries 0); B created {d} across the death; {d} entries listed (ls = find), the digest equal from A and B; open intents 0\n", .{ parked_ms, reclaim_ms, nb, listed });
    }
    try b.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try b.waitExit());
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the dead manager holding a striped directory");
}

/// The striped directory's population: 10,000 names over K = 64 stripes
/// (≈ 156 per stripe — the readdir merge reads every stripe's page set
/// several times over; the rename storm's 1,000 moves cross stripes with
/// probability 63/64).
const stripe_row_files: usize = 10_000;
const stripe_row_renames: usize = 1_000;

/// `setfattr -n user.squeezefs.stripes -v K <dir>` without `attr` in the
/// image: python's `os.setxattr` is the handler's own xattr path.
fn setStripesScript(buf: []u8, dir: []const u8, k: u32) ![]const u8 {
    return std.fmt.bufPrint(buf, "python3 -c 'import os,sys; os.setxattr(sys.argv[1], \"user.squeezefs.stripes\", sys.argv[2].encode())' {s} {d}", .{ dir, k });
}

fn getStripesScript(buf: []u8, dir: []const u8) ![]const u8 {
    return std.fmt.bufPrint(buf, "python3 -c 'import os,sys; sys.stdout.write(os.getxattr(sys.argv[1], \"user.squeezefs.stripes\").decode())' {s}", .{dir});
}

test "smoke: A STRIPED DIRECTORY ON ONE DAEMON — mkdir hot → the explicit flip (`user.squeezefs.stripes` = 64 through setxattr; getxattr answers 64, a plain directory ENODATA) → 10,000 creates key their stripes (dir_stripe_routed_mutations ≥ 10,000; D's own tree holds the 66 markers alone) → `ls | wc -l` = 10,000 and `find` = 10,001 (the K-way merge, every name once) → a 1,000-move rename storm across stripes, every target present, every source gone → stat's nlink folds a subdirectory in → rm -rf: the striped rmdir is ONE intent → umount → fsck --offline --scrub 0 findings (C17 clean)" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    try rig.formatSetWith(false, &.{});
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    var sbuf: [512]u8 = undefined;
    // The flip: ENODATA before, 64 after; a second flip refuses EEXIST.
    {
        const out = try rig.onMount("mkdir hot plain && if python3 -c 'import os,sys; os.getxattr(sys.argv[1], \"user.squeezefs.stripes\")' hot 2>/dev/null; then echo had-k; else echo no-k; fi");
        defer gpa.free(out);
        try testing.expectEqualStrings("no-k\n", out);
    }
    {
        const out = try rig.onMount(try setStripesScript(&sbuf, "hot", 64));
        gpa.free(out);
        const k = try rig.onMount(try getStripesScript(&sbuf, "hot"));
        defer gpa.free(k);
        try testing.expectEqualStrings("64\n", k);
        const again = try rig.sh(rig.mp, try setStripesScript(&sbuf, "hot", 8));
        defer rig.freeRun(again);
        try testing.expect(!again.term.success());
        try testing.expect(std.mem.indexOf(u8, again.stderr, "File exists") != null);
        const plain = try rig.sh(rig.mp, try getStripesScript(&sbuf, "plain"));
        defer rig.freeRun(plain);
        try testing.expect(!plain.term.success());
    }
    // 10,000 creates through the shell, then the counts.
    {
        var script: std.Io.Writer.Allocating = .init(gpa);
        defer script.deinit();
        try script.writer.print("cd hot && i=0; while [ $i -lt {d} ]; do : > f$i; i=$((i+1)); done; cd ..; ls hot | wc -l; find hot | wc -l; find hot -name 'f*' | wc -l", .{stripe_row_files});
        const out = try rig.onMountTimed(script.written(), 600);
        defer gpa.free(out);
        var lines = std.mem.splitScalar(u8, std.mem.trimEnd(u8, out, "\n"), '\n');
        try testing.expectEqualStrings(try std.fmt.bufPrint(&sbuf, "{d}", .{stripe_row_files}), std.mem.trim(u8, lines.next().?, " "));
        try testing.expectEqualStrings(try std.fmt.bufPrint(&sbuf, "{d}", .{stripe_row_files + 1}), std.mem.trim(u8, lines.next().?, " "));
        try testing.expectEqualStrings(try std.fmt.bufPrint(&sbuf, "{d}", .{stripe_row_files}), std.mem.trim(u8, lines.next().?, " "));
    }
    {
        const ta = try rig.stats();
        defer gpa.free(ta);
        try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"dir_stripe_flips\":"));
        try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"dir_striped_dirs\":"));
        try testing.expect((numberAfter(ta, "\"dir_stripe_routed_mutations\":") orelse 0) >= stripe_row_files);
        try testing.expect((numberAfter(ta, "\"dir_stripe_readdir_merges\":") orelse 0) >= 1);
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"dir_stripe_dying_refusals\":"));
    }
    // The rename storm across stripes: every target present, every source
    // gone, the count unchanged.
    {
        var script: std.Io.Writer.Allocating = .init(gpa);
        defer script.deinit();
        try script.writer.print("cd hot && i=0; while [ $i -lt {d} ]; do mv f$i r$i; i=$((i+1)); done; i=0; while [ $i -lt {d} ]; do test -e r$i; test ! -e f$i; i=$((i+1)); done; ls | wc -l", .{ stripe_row_renames, stripe_row_renames });
        const out = try rig.onMountTimed(script.written(), 600);
        defer gpa.free(out);
        try testing.expectEqualStrings(try std.fmt.bufPrint(&sbuf, "{d}\n", .{stripe_row_files}), out);
    }
    // stat: the fold — a subdirectory inside bumps D's nlink.
    {
        const out = try rig.onMount("stat -c %h hot; mkdir hot/sub; stat -c %h hot; rmdir hot/sub; stat -c %h hot");
        defer gpa.free(out);
        try testing.expectEqualStrings("2\n3\n2\n", out);
    }
    // rm -rf: every name through its stripe, then the striped rmdir.
    {
        const out = try rig.onMountTimed("rm -rf hot && test ! -e hot && ls | wc -l", 600);
        defer gpa.free(out);
        try testing.expectEqualStrings("1\n", out);
        const ta = try rig.stats();
        defer gpa.free(ta);
        try testing.expectEqual(@as(?u64, 1), numberAfter(ta, "\"dir_stripe_rmdirs\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(ta, "\"dir_striped_dirs\":"));
        try expectMustStayZero(ta);
        std.debug.print("\nsmoke: striped directory on one daemon — flips {d}, routed lookups {d}, routed mutations {d}, readdir merges {d}, verdict hits {d} / reads {d}, rmdirs {d}\n", .{ numberAfter(ta, "\"dir_stripe_flips\":") orelse 0, numberAfter(ta, "\"dir_stripe_routed_lookups\":") orelse 0, numberAfter(ta, "\"dir_stripe_routed_mutations\":") orelse 0, numberAfter(ta, "\"dir_stripe_readdir_merges\":") orelse 0, numberAfter(ta, "\"dir_stripe_verdict_hits\":") orelse 0, numberAfter(ta, "\"dir_stripe_verdict_reads\":") orelse 0, numberAfter(ta, "\"dir_stripe_rmdirs\":") orelse 0 });
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the striped directory row");
}

test "smoke: THE `-o ro` READER OF A LIVE SET — C mounts `-o ro` beside A (the manager) and B (joined): no flock, no claim, no region; a member-reader + token client (TTLs 0, reader_staleness_bound_ms 0) reading B's 2,000 files and A's tree under tokens (0 local reads) → B writes ⇒ C's next read is fresh → A and B leave ⇒ C becomes the QUIESCENT reader of their last checkpoint → A mounts again ⇒ C fail-stops ESTALE at its next op (`.stats` still readable) → remount C ⇒ a token reader again — B's membership leave is LOST (`SQUEEZEFS_TEST_MEMBERSHIP_LEAVE_LOST`): its row outlives it, and A's successor must not expect a member whose pid is proven dead (decision 0003 §AG.5), or its grace refuses C's remount" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    var env_b = try envWith(gpa, &rig.env, &.{.{ "SQUEEZEFS_TEST_MEMBERSHIP_LEAVE_LOST", "1" }});
    defer env_b.deinit();
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.env = &env_b;
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
        if (std.mem.count(u8, r.stdout, "Stale file handle") != 2) std.debug.print("\nsmoke: -o ro: C after the fail-stop answered:\n{s}\n", .{r.stdout});
        try testing.expectEqual(@as(usize, 2), std.mem.count(u8, r.stdout, "Stale file handle"));
        const text = try c.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "a writer appeared (ledger_advance) under the quiescent reader — every metadata op now answers ESTALE") != null);
        std.debug.print("smoke: -o ro: A mounted again — C fail-stopped (ESTALE on ls and cat; .stats readable)\n", .{});
    }
    // B's row outlived its lost leave; A's successor proved its pid dead
    // and opened no grace for it (the remount below is a fresh join).
    {
        const ta = try rig.onMount("cat .stats");
        defer gpa.free(ta);
        try testing.expect((numberAfter(ta, "\"membership_grace_dead_rows\":") orelse 0) >= 1);
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

/// GNU tar's write(2) grain on extraction. `extract_file` issues one
/// `write(2)` per record-contiguous span of an entry's data (the record is
/// `-b N` blocks of `tar_block`; `tar --show-defaults` names N, GNU tar's
/// default 20 = 10,240 B), so an entry whose data straddles a record
/// boundary lands as TWO writes — the first exactly the bytes to the
/// record's end. For this tree (a 512 B header + 1,000 B of data padded to
/// 1,024 B per entry, the `./` directory header first) the data of every
/// 20th file — f19, f39, …, f199, … — starts at a record's last block:
/// 512 + 488, 75 of the 1,500 (strace-proven; `docs/evidence/rung-09-
/// gates.md` §7). A crash between the two leaves a legitimate POSIX state
/// — the first write landed, the second never issued or never acked — so
/// the oracle accepts a file cut THERE and nowhere else. The table is
/// derived from the ARCHIVE (`tar -R -tf`: each entry's header block),
/// never assumed from the name.
const TarWriteGrain = struct {
    record_blocks: u64,
    /// The length of tar's first `write(2)` for `f<idx>`: the entry's size
    /// when its data lies inside one record.
    first_write: [kill_tree_files]u16,

    /// `-bN` from `tar --show-defaults`.
    fn recordBlocks(defaults: []const u8) !u64 {
        var it = std.mem.tokenizeAny(u8, defaults, " \n");
        while (it.next()) |word| {
            if (std.mem.startsWith(u8, word, "-b")) return std.fmt.parseInt(u64, word["-b".len..], 10);
        }
        return error.TarDefaultsUnparsed;
    }

    /// The first write of an entry of `size` whose header sits at
    /// `header_block`: its data starts at the next block and the write
    /// spans to the record's end.
    fn firstWrite(header_block: u64, size: u64, record_blocks: u64) u64 {
        const data_block = header_block + 1;
        return @min(size, (record_blocks - data_block % record_blocks) * tar_block);
    }

    /// From `tar -R -tf` (`block N: ./fIDX` per member); every file of the
    /// tree named exactly once.
    fn parse(listing: []const u8, record_blocks: u64) !TarWriteGrain {
        var g: TarWriteGrain = .{ .record_blocks = record_blocks, .first_write = @splat(0) };
        var named: usize = 0;
        var lines = std.mem.splitScalar(u8, listing, '\n');
        while (lines.next()) |line| {
            const head = "block ";
            if (!std.mem.startsWith(u8, line, head)) continue;
            const colon = std.mem.indexOfScalar(u8, line, ':') orelse return error.TarListingUnparsed;
            const block = try std.fmt.parseInt(u64, line[head.len..colon], 10);
            const member = "./f";
            if (line.len < colon + ": ".len) return error.TarListingUnparsed;
            const name = line[colon + ": ".len ..];
            if (!std.mem.startsWith(u8, name, member)) continue;
            const idx = try std.fmt.parseInt(usize, name[member.len..], 10);
            if (idx >= kill_tree_files or g.first_write[idx] != 0) return error.TarListingUnparsed;
            g.first_write[idx] = @intCast(firstWrite(block, kill_tree_file_len, record_blocks));
            named += 1;
        }
        if (named != kill_tree_files) return error.TarListingUnparsed;
        return g;
    }

    /// How many entries tar extracts in two writes.
    fn twoWriteEntries(self: *const TarWriteGrain) usize {
        var n: usize = 0;
        for (self.first_write) |w| n += @intFromBool(w < kill_tree_file_len);
        return n;
    }
};

test "smoke: tar's write(2) grain — the record blocks from --show-defaults, the first write from the header block (data at a record's last block = one block, else the size), the -R listing parsed once per member" {
    try testing.expectEqual(@as(u64, 20), try TarWriteGrain.recordBlocks("--format=gnu -f- -b20 --quoting-style=escape\n"));
    try testing.expectError(error.TarDefaultsUnparsed, TarWriteGrain.recordBlocks("--format=gnu -f-\n"));
    // ./ at block 0, f0's header at 1 (data 2–3), f1 at 4, … f19 at 58:
    // data at 59 = the record's last block → 512; f20 at 61 → whole.
    try testing.expectEqual(@as(u64, 1000), TarWriteGrain.firstWrite(1, 1000, 20));
    try testing.expectEqual(@as(u64, 512), TarWriteGrain.firstWrite(58, 1000, 20));
    try testing.expectEqual(@as(u64, 1000), TarWriteGrain.firstWrite(61, 1000, 20));
    // A 1,024 B entry at the record's last block is 512 + 512; a 300 B one
    // there fits its first write.
    try testing.expectEqual(@as(u64, 512), TarWriteGrain.firstWrite(58, 1024, 20));
    try testing.expectEqual(@as(u64, 300), TarWriteGrain.firstWrite(58, 300, 20));
    // The full tree in archive order: 75 two-write entries, all ≡ 19 (mod 20).
    var listing: std.Io.Writer.Allocating = .init(testing.allocator);
    defer listing.deinit();
    try listing.writer.writeAll("block 0: ./\n");
    for (0..kill_tree_files) |i| try listing.writer.print("block {d}: ./f{d}\n", .{ 1 + 3 * i, i });
    const g = try TarWriteGrain.parse(listing.written(), 20);
    try testing.expectEqual(@as(usize, 75), g.twoWriteEntries());
    for (g.first_write, 0..) |w, i| try testing.expectEqual(@as(u16, if (i % 20 == 19) 512 else 1000), w);
    // A listing that names a member twice or misses one is refused.
    try testing.expectError(error.TarListingUnparsed, TarWriteGrain.parse("block 0: ./\nblock 1: ./f0\nblock 4: ./f0\n", 20));
    try testing.expectError(error.TarListingUnparsed, TarWriteGrain.parse("block 0: ./\nblock 1: ./f0\n", 20));
}

/// One extracted entry's state after the crash, at tar's write(2) grain.
const EntryState = enum { whole, empty, first_write_only };

/// THE write(2)-grain oracle for one entry (charter §1.10 "zero acked
/// loss"): `got` against the body the name promises; `acked` = tar's
/// write(s) returned before the kill (its stderr names the unacked set);
/// `first_write` = tar's first `write(2)` length for the entry
/// (`TarWriteGrain`). An acked entry is whole or LOST; an unacked one is
/// whole (landed, the reply lost), empty (its create landed, no write did)
/// or cut exactly at its first write (the second never landed) — any other
/// content is TORN: a prefix inside a single write, or bytes that are not
/// the body's.
fn judgeAtWriteGrain(got: []const u8, want: []const u8, acked: bool, first_write: u64) error{ AckedWriteLost, TornFile }!EntryState {
    const prefix_ok = got.len <= want.len and std.mem.eql(u8, got, want[0..got.len]);
    if (prefix_ok and got.len == want.len) return .whole;
    if (acked) return error.AckedWriteLost;
    if (got.len == 0) return .empty;
    if (prefix_ok and got.len == first_write) return .first_write_only;
    return error.TornFile;
}

/// The body the kill tree's `f<idx>` promises: `file-NNNNN:` + the letter.
fn killTreeBody(idx: usize, want: *[kill_tree_file_len]u8) !void {
    const head = try std.fmt.bufPrint(want, "file-{d:0>5}:", .{idx});
    @memset(want[head.len..], 'a' + @as(u8, @intCast(idx % 26)));
}

test "smoke: the write(2)-grain oracle — acked ⇒ whole or lost; unacked ⇒ whole, empty or exactly tar's first write; a prefix inside a write, or at a boundary the entry does not have, is torn" {
    var want: [kill_tree_file_len]u8 = undefined;
    try killTreeBody(199, &want);
    try testing.expectEqualStrings("file-00199:rrr", want[0..14]);
    // Whole is whole either way.
    try testing.expectEqual(EntryState.whole, try judgeAtWriteGrain(&want, &want, true, 512));
    try testing.expectEqual(EntryState.whole, try judgeAtWriteGrain(&want, &want, false, 1000));
    // The gate's finding: 512 B of a two-write entry tar was NOT acked on.
    try testing.expectEqual(EntryState.first_write_only, try judgeAtWriteGrain(want[0..512], &want, false, 512));
    // The same 512 B of an entry tar WAS acked on: an acked write lost.
    try testing.expectError(error.AckedWriteLost, judgeAtWriteGrain(want[0..512], &want, true, 512));
    // 512 B of a ONE-write entry (first write = the size): torn inside a write.
    try testing.expectError(error.TornFile, judgeAtWriteGrain(want[0..512], &want, false, 1000));
    // A prefix that is not the first write's length is torn even on a two-write entry.
    try testing.expectError(error.TornFile, judgeAtWriteGrain(want[0..700], &want, false, 512));
    try testing.expectError(error.TornFile, judgeAtWriteGrain(want[0..488], &want, false, 512));
    // Empty: the create landed, no write did — unacked only.
    try testing.expectEqual(EntryState.empty, try judgeAtWriteGrain("", &want, false, 512));
    try testing.expectError(error.AckedWriteLost, judgeAtWriteGrain("", &want, true, 1000));
    // Bytes that are not the body's (a swapped neighbour): torn at any
    // length, lost when acked.
    var swapped: [kill_tree_file_len]u8 = undefined;
    try killTreeBody(198, &swapped);
    try testing.expectError(error.TornFile, judgeAtWriteGrain(&swapped, &want, false, 512));
    try testing.expectError(error.AckedWriteLost, judgeAtWriteGrain(&swapped, &want, true, 512));
    try testing.expectError(error.TornFile, judgeAtWriteGrain(swapped[0..512], &want, false, 512));
    // Longer than the body: torn.
    const longer: [kill_tree_file_len + 1]u8 = @splat('r');
    try testing.expectError(error.TornFile, judgeAtWriteGrain(&longer, &want, false, 512));
}

/// The kill row's body (the coalesced and the strict cadence rows):
/// format → mount → `tar -xf` of the 1,500-file tree in a child → `kill
/// -9` once `kill_after_files` are visible → the dead connection unmounted
/// → remount (the window replays) → the tree judged at tar's write(2)
/// grain against tar's own ack ledger (its stderr) and the archive's write
/// boundaries (`TarWriteGrain`) → the gauges → a clean cycle.
fn killRowTarX(strict: bool) !void {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    // The strict cadence: ONE barrier per group before any ack (the knob's
    // A/B control arm), named by the mount line.
    if (strict) try rig.env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "0");
    const cadence_word: []const u8 = if (strict) "strict cadence" else "coalesced cadence";
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
    // tar's write(2) boundaries, from the archive it will extract.
    const grain = blk: {
        const defaults = try rig.shOk(rig.work, "tar --show-defaults");
        defer gpa.free(defaults);
        const listing = try rig.shOk(rig.work, "tar -R -tf ktree.tar");
        defer gpa.free(listing);
        break :blk try TarWriteGrain.parse(listing, try TarWriteGrain.recordBlocks(defaults));
    };
    try rig.spawnMount(&.{});
    const ready1 = try rig.awaitReady(0);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[0..ready1], cadence_word) != null);
    }
    // tar -x in a child; kill the daemon once enough files exist. tar's
    // stderr IS its ack ledger: the entries it could not open or write
    // when the daemon died are the unacked ones (read after the wait).
    var tar_argv_buf: [64]u8 = undefined;
    const tar_path = try std.fmt.bufPrint(&tar_argv_buf, "{s}/ktree.tar", .{rig.work});
    var tar_err_buf: [192]u8 = undefined;
    const tar_err_path = try std.fmt.bufPrint(&tar_err_buf, "{s}/tar.err", .{rig.work});
    const tar_err_file = try Io.Dir.cwd().createFile(io, tar_err_path, .{ .truncate = true });
    defer tar_err_file.close(io);
    var tar = try std.process.spawn(io, .{ .argv = &.{ "tar", "-xf", tar_path, "-C", rig.mp }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .{ .file = tar_err_file } });
    var seen: usize = 0;
    {
        // A TIGHT poll (readdir back to back, no sleep): the kill should
        // land as soon as the `kill_after_files`-th entry is visible — while
        // its two writes are still in flight — so the record-boundary state
        // is exercised, not merely tolerated (met 3 times in 60 rows, all
        // under the strict cadence whose barrier widens the gap; the 5 ms
        // cadence before met it once in weeks of gates).
        const deadline = clock.monotonicNs() + 60 * std.time.ns_per_s;
        while (clock.monotonicNs() < deadline) {
            var dir = try Io.Dir.cwd().openDir(io, rig.mp, .{ .iterate = true });
            defer dir.close(io);
            seen = try countFiles(gpa, io, dir);
            if (seen >= kill_after_files) break;
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
    const tar_err = try Io.Dir.cwd().readFileAlloc(io, tar_err_path, gpa, .limited(1 << 20));
    defer gpa.free(tar_err);
    var unacked = try tarUnackedNames(gpa, tar_err, "tar: ./");
    defer freeNames(gpa, &unacked);
    // A tar that finished clean never met the kill.
    try testing.expect(unacked.items.len > 0);
    std.debug.print("\nsmoke: kill -9 sent with {d} of {d} files visible ({s}); tar {f}, {d} entries unacked\n", .{ seen, kill_tree_files, cadence_word, tar_term, unacked.items.len });

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
        try testing.expect(std.mem.indexOf(u8, seg, cadence_word) != null);
        replayed = numberAfter(seg, "replayed ") orelse 0;
    }
    // The tree at tar's write(2) grain: an entry tar was ACKED on is
    // present and whole; an unacked one is whole (landed, the reply lost),
    // empty (its create landed, no write did) or exactly tar's FIRST write
    // (a record-boundary entry whose second write never landed); anything
    // else is torn.
    var whole: usize = 0;
    var empty: usize = 0;
    var first_only: usize = 0;
    var present: usize = 0;
    var present_set: [kill_tree_files]bool = @splat(false);
    {
        var dir = try Io.Dir.cwd().openDir(io, rig.mp, .{ .iterate = true });
        defer dir.close(io);
        var it = dir.iterate();
        while (try it.next(io)) |ent| {
            if (ent.kind != .file) continue;
            present += 1;
            const got = try dir.readFileAlloc(io, ent.name, gpa, .limited(1 << 20));
            defer gpa.free(got);
            // The expected body from the name: `file-NNNNN:` + the letter.
            const idx = try std.fmt.parseInt(usize, ent.name[1..], 10);
            if (idx >= kill_tree_files) return error.UnexpectedName;
            present_set[idx] = true;
            const acked = !namesContain(unacked.items, ent.name);
            var want: [kill_tree_file_len]u8 = undefined;
            try killTreeBody(idx, &want);
            const state = judgeAtWriteGrain(got, &want, acked, grain.first_write[idx]) catch |e| {
                std.debug.print("\n--- {s}: {d} bytes, {s} by tar, its first write(2) {d} B — {t} ---\n", .{ ent.name, got.len, if (acked) "ACKED" else "unacked", grain.first_write[idx], e });
                return e;
            };
            switch (state) {
                .whole => whole += 1,
                .empty => empty += 1,
                .first_write_only => {
                    std.debug.print("smoke: {s}: {d} bytes = tar's first write(2) of the entry, its second ({d} B) never acked — the write(2)-grain state the oracle accepts\n", .{ ent.name, got.len, kill_tree_file_len - got.len });
                    first_only += 1;
                },
            }
        }
    }
    // Every entry tar was acked on (not in its unacked set) is present.
    for (present_set, 0..) |is_present, idx| {
        if (is_present) continue;
        var name_buf: [16]u8 = undefined;
        const name = try std.fmt.bufPrint(&name_buf, "f{d}", .{idx});
        if (namesContain(unacked.items, name)) continue;
        std.debug.print("\n--- {s}: ACKED by tar (its open returned), absent on the remount — an acked create lost ---\n", .{name});
        return error.AckedCreateLost;
    }
    try testing.expect(present >= kill_after_files);
    try testing.expect(whole > 0);
    std.debug.print("smoke: after kill -9 ({s}): replayed {d} journal entries; {d} files present ({d} whole, {d} empty — created, write not yet issued, {d} at tar's first write(2) boundary), {d} never created; every entry tar was acked on is whole; {d} of {d} entries are two-write entries under tar's {d}-block record\n", .{ cadence_word, replayed, present, whole, empty, first_only, kill_tree_files - present, grain.twoWriteEntries(), kill_tree_files, grain.record_blocks });
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

test "smoke: kill -9 mid-tar -x → remount replays the window → judged at tar's write(2) grain: every write tar was acked on is present whole, an unacked file is whole, empty or cut exactly at tar's first write of a record-boundary entry, nothing else; a clean cycle after it" {
    try killRowTarX(false);
}

test "smoke: kill -9 mid-tar -x under the STRICT cadence (SQUEEZEFS_META_FLUSH_INTERVAL_MS=0: the barrier before every ack) → the same write(2)-grain contract — every acked write present whole, the unacked tail whole, empty or at tar's first write; a clean cycle after it" {
    try killRowTarX(true);
}

test "smoke: --daemon --supervise --log-file: the invoking process returns 0 only after ready, the log file is 0600 and carries the ready line, umount ends the daemon and the supervisor; a refused daemon mount reports its reason to the invoker" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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
    // The supervisor's own first line lands AFTER the ready handoff that
    // returned the invoker (a separate process, scheduled in its own time:
    // under a loaded venue it trails the invoker's exit by seconds), so its
    // appearance is awaited, never asserted on the snapshot above.
    _ = try rig.awaitLog("supervisor: watching daemon ", ready_i, ready_wait_ns);
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

/// A process whose cwd is the mount (the holder of the busy-umount rows):
/// `sleep` parks with the mount root as its cwd until killed.
const Holder = struct {
    child: std.process.Child,
    pid: linux.pid_t,
    gone: bool = false,

    fn spawn(rig: *Rig) !Holder {
        const c = try std.process.spawn(rig.io, .{ .argv = &.{ "sleep", "300" }, .cwd = .{ .path = rig.mp }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .ignore });
        return .{ .child = c, .pid = c.id.? };
    }

    /// Kill and reap (idempotent: the row's `defer` meets an already-gone holder).
    fn kill(self: *Holder, io: Io) void {
        if (self.gone) return;
        self.gone = true;
        _ = linux.kill(self.pid, .KILL);
        _ = self.child.wait(io) catch {};
    }
};

/// The supervisor's verdict lines after `from`: none of the escalation
/// ladder may appear on an orderly cycle (charter §1.1: a daemon is never
/// "unresponsive" for being asked to unmount).
fn expectSupervisorSilent(text: []const u8) !void {
    inline for (.{ "missed a probe", "heartbeat stale", "unresponsive for", "is dead (wait status", "detaching lazily" }) |needle| {
        if (std.mem.indexOf(u8, text, needle)) |i| {
            std.debug.print("\n--- the supervisor/transport fired on an orderly cycle: '{s}' ---\n{s}\n", .{ needle, text[i..@min(text.len, i + 400)] });
            return error.SupervisorFired;
        }
    }
}

test "smoke: umount of a BUSY mount (another process's cwd inside it) — the verb REFUSES loud within a tick naming the holder's pid (EBUSY, exit 1), the daemon keeps serving (stat, a write), nothing detaches, the supervisor stays silent; a bare SIGTERM is refused the same way (counted in .stats); after the holder leaves the next umount is the clean one (`clean unmount`, `exited (wait status 0x0)`)" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(true);
    {
        const r = try rig.squeezefs(&.{ "mount", "--daemon", "--supervise", "--log-file", rig.log_path, "-o", "default_permissions,allow_other", rig.meta_uri, rig.mp });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\ndaemon mount: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }
    const text0 = try rig.readLog();
    defer gpa.free(text0);
    const ready_i = std.mem.indexOf(u8, text0, "ready: ") orelse return error.NoReadyLine;
    const pid: linux.pid_t = @intCast(numberAfter(text0[ready_i..], "(pid ") orelse return error.NoPid);
    _ = try rig.awaitLog("supervisor: watching daemon ", ready_i, ready_wait_ns);
    {
        const out = try rig.shOk(rig.mp, "echo before > f && test \"$(cat f)\" = before");
        defer gpa.free(out);
    }
    var holder = try Holder.spawn(rig);
    defer holder.kill(io);
    // The holder's cwd is the mount root: readable through /proc.
    {
        var pb: [64:0]u8 = undefined;
        var lb: [linux.PATH_MAX]u8 = undefined;
        const link = try std.fmt.bufPrintSentinel(&pb, "/proc/{d}/cwd", .{holder.pid}, 0);
        const rc = linux.readlink(link.ptr, &lb, lb.len);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
        try testing.expectEqualStrings(rig.mp, lb[0..rc]);
    }
    var pid_text: [16]u8 = undefined;
    const pid_s = try std.fmt.bufPrint(&pid_text, "{d}", .{holder.pid});
    // 1. The verb refuses: EBUSY within one lock-plane tick, the holder
    //    named, exit 1 — never 0, never 29 s, never a dead daemon.
    {
        const t0 = clock.monotonicNs();
        const r = try rig.squeezefs(&.{ "umount", rig.mp });
        defer rig.freeRun(r);
        const took_ms = (clock.monotonicNs() - t0) / std.time.ns_per_ms;
        if (r.term.success() or took_ms > 5000) std.debug.print("\n--- umount of the busy mount: rc {f} after {d} ms (expected exit 1 within a tick); stderr:\n{s}\n", .{ r.term, took_ms, r.stderr });
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(took_ms <= 5000);
        if (std.mem.indexOf(u8, r.stderr, "busy") == null or std.mem.indexOf(u8, r.stderr, pid_s) == null) std.debug.print("\n--- the refusal did not name the holder {d}:\n{s}\n", .{ holder.pid, r.stderr });
        try testing.expect(std.mem.indexOf(u8, r.stderr, "busy") != null);
        try testing.expect(std.mem.indexOf(u8, r.stderr, pid_s) != null);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "cwd") != null);
    }
    // The daemon is alive and the mount serves: the control inode answers
    // and a write lands.
    try testing.expect(alive(pid));
    try testing.expectEqual(@as(usize, 1), try fuseMountsOf(gpa, io, rig.mp));
    {
        const out = try rig.shOk(rig.mp, "test -s .stats && echo during > g && test \"$(cat g)\" = during && test \"$(cat f)\" = before");
        defer gpa.free(out);
    }
    // 2. A bare SIGTERM (an operator's `kill`) meets the daemon's own
    //    census: refused, logged, counted; the mount keeps serving.
    {
        const mark = try rig.logLen();
        _ = linux.kill(pid, .TERM);
        _ = try rig.awaitLog("is busy", mark, 10 * std.time.ns_per_s);
        try testing.expect(alive(pid));
        const out = try rig.shOk(rig.mp, "test \"$(cat g)\" = during");
        defer gpa.free(out);
        const st = try rig.stats();
        defer gpa.free(st);
        try testing.expectEqual(@as(?u64, 1), numberAfter(st, "\"transport_umount_busy_refusals\":"));
    }
    {
        const during = try rig.readLog();
        defer gpa.free(during);
        try expectSupervisorSilent(during[ready_i..]);
        try testing.expect(std.mem.indexOf(u8, during[ready_i..], "stop requested") != null);
    }
    // 3. The holder leaves; the orderly unmount is the clean one.
    holder.kill(io);
    {
        const t0 = clock.monotonicNs();
        const r = try rig.squeezefs(&.{ "umount", rig.mp });
        defer rig.freeRun(r);
        const took_ms = (clock.monotonicNs() - t0) / std.time.ns_per_ms;
        if (!r.term.success()) std.debug.print("\n--- the clean umount failed ({f}) after {d} ms:\n{s}\n", .{ r.term, took_ms, r.stderr });
        try testing.expect(r.term.success());
        try testing.expect(took_ms <= 20_000);
    }
    {
        const deadline = clock.monotonicNs() + exit_wait_ns;
        while (alive(pid) and clock.monotonicNs() < deadline) sleepMs(20);
        try testing.expect(!alive(pid));
    }
    _ = try rig.awaitLog("exited (wait status 0x0); done", ready_i, 20 * std.time.ns_per_s);
    const after = try rig.readLog();
    defer gpa.free(after);
    try testing.expect(std.mem.indexOf(u8, after[ready_i..], "clean unmount") != null);
    try expectSupervisorSilent(after[ready_i..]);
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    try testing.expect(!status_verb.writerGuardHeld(rig.img));
}

test "smoke: the stop's own sender reading `.stats` is no holder — a process opens `<mp>/.stats` and SIGTERMs the daemon with the fd open (what `squeezefs umount` does: its verdict poll opens `.stats` on its signal's heels — the matrix row's `is busy (0 holder(s))`): the daemon's census exempts the sender's control-document fds and unmounts — never `is busy`, never the lazy detach — and exits 0" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(false);
    try rig.spawnMount(&.{});
    const ready = try rig.awaitReady(0);
    const stats_path = try std.fmt.allocPrintSentinel(gpa, "{s}/.stats", .{rig.mp}, 0);
    defer gpa.free(stats_path);
    var opened: [2]linux.fd_t = undefined;
    var go: [2]linux.fd_t = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&opened, .{ .CLOEXEC = true })));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&go, .{ .CLOEXEC = true })));
    const daemon_pid = rig.child_pid;
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) {
        // The sender, dumpable so the daemon's census can read its fds (a
        // hardened test process is not): `.stats` open across its own
        // SIGTERM until the parent says go.
        _ = linux.close(opened[0]);
        _ = linux.close(go[1]);
        _ = linux.prctl(@backingInt(linux.PR.SET_DUMPABLE), 1, 0, 0, 0);
        const fd_rc = linux.open(stats_path.ptr, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(fd_rc) != .SUCCESS) linux.exit(2);
        _ = linux.kill(daemon_pid, .TERM);
        var one: u8 = 1;
        _ = linux.write(opened[1], @ptrCast(&one), 1);
        _ = linux.read(go[0], @ptrCast(&one), 1);
        _ = linux.close(@intCast(fd_rc));
        linux.exit(0);
    }
    const sender: linux.pid_t = @intCast(rc);
    _ = linux.close(opened[1]);
    _ = linux.close(go[0]);
    var byte: u8 = 0;
    _ = linux.read(opened[0], @ptrCast(&byte), 1);
    _ = linux.close(opened[0]);
    // The daemon logs its verdict after its census; the sender closes then.
    const verdict = rig.awaitLog("stop requested", ready, 10 * std.time.ns_per_s);
    _ = linux.close(go[1]);
    var status: i32 = 0;
    _ = linux.waitpid(sender, &status, 0);
    _ = try verdict;
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        if (std.mem.indexOf(u8, text[ready..], "is busy")) |i| std.debug.print("\n--- the daemon refused its own sender's stop ---\n{s}\n", .{text[ready + i - @min(i, 120) .. @min(text.len, ready + i + 300)]});
        try testing.expect(std.mem.indexOf(u8, text[ready..], "is busy") == null);
    }
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    const after = try rig.readLog();
    defer gpa.free(after);
    try testing.expect(std.mem.indexOf(u8, after[ready..], "unmounting") != null);
    try testing.expect(std.mem.indexOf(u8, after[ready..], "detaching lazily") == null);
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
}

/// The reads a holder completed on its cwd (`<work>/count`), 0 before its
/// first.
fn holderCount(rig: *Rig) u64 {
    var pb: [192]u8 = undefined;
    var buf: [32]u8 = undefined;
    const path = std.fmt.bufPrint(&pb, "{s}/count", .{rig.work}) catch return 0;
    const text = Io.Dir.cwd().readFile(rig.io, path, &buf) catch return 0;
    return std.fmt.parseInt(u64, std.mem.trim(u8, text, " \n"), 10) catch 0;
}

/// Bounded poll until the holder's count passes `floor`.
fn awaitHolderReads(rig: *Rig, floor: u64, deadline_ns: u64) !u64 {
    const deadline = clock.monotonicNs() + deadline_ns;
    while (true) {
        const n = holderCount(rig);
        if (n > floor) return n;
        if (clock.monotonicNs() >= deadline) {
            std.debug.print("\n--- the holder made no progress past {d} reads ---\n", .{floor});
            return error.Timeout;
        }
        sleepMs(20);
    }
}

test "smoke: `umount --lazy` of a BUSY mount — the verb returns at once naming the holders it leaves, the mountpoint is gone from the table, the holder KEEPS reading its detached cwd (every ls and .stats served), the supervisor switches to the heartbeat and never warns; when the holder leaves the kernel ends the connection and the daemon tears down clean (`exited (wait status 0x0)`); `--lazy` on an unheld mount is the orderly stop" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(true);
    // The supervisor looks every quarter second (the probe seam): step 3's
    // switch to the heartbeat lands at its first look past the detach,
    // never after up to three 5 s intervals.
    try rig.env.put("SQUEEZEFS_TEST_SUPERVISOR_PROBE_MS", "250");
    {
        const r = try rig.squeezefs(&.{ "mount", "--daemon", "--supervise", "--log-file", rig.log_path, "-o", "default_permissions,allow_other", rig.meta_uri, rig.mp });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\ndaemon mount: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }
    const text0 = try rig.readLog();
    defer gpa.free(text0);
    const ready_i = std.mem.indexOf(u8, text0, "ready: ") orelse return error.NoReadyLine;
    const pid: linux.pid_t = @intCast(numberAfter(text0[ready_i..], "(pid ") orelse return error.NoPid);
    _ = try rig.awaitLog("supervisor: watching daemon ", ready_i, ready_wait_ns);
    // The holder: cwd = the mount root, reading it (ls + the control
    // inode) until the stop file appears OUTSIDE the mount; its count of
    // served reads lands beside the stop file.
    var script: [512]u8 = undefined;
    // `-d work` too: a row that fails before the stop file leaves no
    // holder spinning on a deleted work dir.
    const holder_script = try std.fmt.bufPrint(&script, "n=0; while [ -d {s} ] && [ ! -e {s}/stop ]; do ls . >/dev/null || exit 2; cat .stats >/dev/null || exit 3; n=$((n+1)); echo $n > {s}/count; sleep 0.05; done; exit 0", .{ rig.work, rig.work, rig.work });
    var holder = try std.process.spawn(io, .{ .argv = &.{ "sh", "-c", holder_script }, .cwd = .{ .path = rig.mp }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .inherit });
    const holder_pid = holder.id.?;
    var stop_buf: [192]u8 = undefined;
    const stop_path = try std.fmt.bufPrint(&stop_buf, "{s}/stop", .{rig.work});
    defer Io.Dir.cwd().writeFile(io, .{ .sub_path = stop_path, .data = "" }) catch {};
    const before = try awaitHolderReads(rig, 0, 10 * std.time.ns_per_s);
    var pid_text: [16]u8 = undefined;
    const pid_s = try std.fmt.bufPrint(&pid_text, "{d}", .{holder_pid});
    // 1. The lazy detach: at once, the holder named, the path free.
    const lazy_mark = try rig.logLen();
    {
        const t0 = clock.monotonicNs();
        const r = try rig.squeezefs(&.{ "umount", "--lazy", rig.mp });
        defer rig.freeRun(r);
        const took_ms = (clock.monotonicNs() - t0) / std.time.ns_per_ms;
        if (!r.term.success() or took_ms > 5000) std.debug.print("\n--- umount --lazy: rc {f} after {d} ms; stdout:\n{s}\nstderr:\n{s}\n", .{ r.term, took_ms, r.stdout, r.stderr });
        try testing.expect(r.term.success());
        try testing.expect(took_ms <= 5000);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "detached") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, pid_s) != null);
    }
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    try testing.expect(alive(pid));
    _ = try rig.awaitLog("detached lazily with", lazy_mark, 10 * std.time.ns_per_s);
    // 2. The holder keeps being served on the detached superblock.
    const after_detach = try awaitHolderReads(rig, before + 10, 20 * std.time.ns_per_s);
    try testing.expect(after_detach > before);
    // 3. The supervisor notices the detach, switches probes, stays silent.
    _ = try rig.awaitLog("is no longer attached", lazy_mark, 3 * cli_supervisor.PROBE_INTERVAL_NS);
    _ = try awaitHolderReads(rig, after_detach, 20 * std.time.ns_per_s);
    try testing.expect(alive(pid));
    {
        const during = try rig.readLog();
        defer gpa.free(during);
        try expectSupervisorSilent(during[ready_i..]);
    }
    // 4. The holder leaves: the kernel ends the connection, the daemon
    //    tears down clean, the supervisor reaps 0x0.
    try Io.Dir.cwd().writeFile(io, .{ .sub_path = stop_path, .data = "" });
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try holder.wait(io));
    _ = try rig.awaitLog("the FUSE session ended (unmounted): tearing down", lazy_mark, 20 * std.time.ns_per_s);
    _ = try rig.awaitLog("exited (wait status 0x0); done", lazy_mark, 20 * std.time.ns_per_s);
    try testing.expect(!alive(pid));
    {
        const after = try rig.readLog();
        defer gpa.free(after);
        try testing.expect(std.mem.indexOf(u8, after[lazy_mark..], "clean unmount") != null);
        try expectSupervisorSilent(after[ready_i..]);
    }
    try testing.expect(!status_verb.writerGuardHeld(rig.img));
    // 5. `--lazy` on an unheld mount: the orderly stop, nothing lazy.
    Io.Dir.cwd().deleteFile(io, stop_path) catch {};
    const mark2 = try rig.logLen();
    {
        const r = try rig.squeezefs(&.{ "mount", "--daemon", "--supervise", "--log-file", rig.log_path, rig.meta_uri, rig.mp });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
    }
    const ready2 = try rig.awaitReady(mark2);
    {
        const r = try rig.squeezefs(&.{ "umount", "-l", rig.mp });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\n--- umount -l (unheld): {f}\n{s}\n", .{ r.term, r.stderr });
        try testing.expect(r.term.success());
    }
    _ = try rig.awaitLog("lazy stop requested (SIGUSR2): unmounting", ready2, 10 * std.time.ns_per_s);
    _ = try rig.awaitLog("exited (wait status 0x0); done", ready2, 20 * std.time.ns_per_s);
    const final = try rig.readLog();
    defer gpa.free(final);
    try testing.expect(std.mem.indexOf(u8, final[ready2..], "clean unmount") != null);
    try testing.expect(std.mem.indexOf(u8, final[ready2..], "detached lazily") == null);
    try expectSupervisorSilent(final[ready2..]);
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
}

test "smoke: a LATE bring-up refusal — the kmbuf ring answers ENOMEM (the seam) after the data plane registered its budget sources: the daemon exits 1 (never a signal) with the typed refusal naming the region's pages and the levers that shrink it, nothing stays mounted or held, and the R5 sampler, ticking at its stop under its seam, reads no freed source; a clean mount → umount under the same sampler seam exits 0" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    // The registration is reached only where the probe confirmed kmbuf.
    if (transport.probes.driver.probeKmbuf(@intCast(derive.facts().page_size)).opcodes() == null) return testkit.skip(@src(), .no_kernel_feature, "no kmbuf register opcode on this kernel");
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(true);
    // The sampler's stop runs one tick over every registered source: a
    // teardown that frees a source before stopping the sampler dies on the
    // read (`samplerMain → current` over the freed plane: a fault or a
    // safety panic) instead of racing the 1 Hz tick.
    try rig.env.put("SQUEEZEFS_TEST_MEM_SAMPLER_TICK_ON_STOP", "1");
    // 1. The refusal: the plane is open (its tiers and pools registered)
    //    when the session's first worker meets ENOMEM after the retry
    //    ladder; the exit is a clean 1 and the stderr line is the remedy.
    {
        try rig.env.put("SQUEEZEFS_TEST_KMBUF_REGISTER_ENOMEM", "1");
        const r = try rig.squeezefs(&.{ "mount", rig.meta_uri, rig.mp });
        defer rig.freeRun(r);
        if (r.term != .exited) std.debug.print("\n--- the refused mount died ({f}) instead of exiting ---\n{s}\n", .{ r.term, r.stderr });
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "squeezefs mount: the FUSE session could not arm at ") != null);
        // The precise levers (decision 0003 §AG.4): the region's pages,
        // never a contiguous-page shortage.
        try testing.expect(std.mem.indexOf(u8, r.stderr, ": kmbuf ring registration ENOMEM: a queue's region takes ") != null);
        for ([_][]const u8{ "SQUEEZEFS_FUSE_MAX_WRITE", "SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", "SQUEEZEFS_FUSE_BUFFER_MODE=user" }) |lever| try testing.expect(std.mem.indexOf(u8, r.stderr, lever) != null);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "contiguous") == null);
        // The retry ladder ran its bounded course before the refusal.
        try testing.expectEqual(@as(usize, transport.kmbuf.REGISTER_RETRIES), std.mem.count(u8, r.stderr, "refused ENOMEM — retry "));
        try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
        try testing.expect(!status_verb.writerGuardHeld(rig.img));
    }
    // 2. The same sampler seam over a clean cycle: the stop's tick reads
    //    live sources on the serve-path teardown too; exit 0.
    try rig.env.put("SQUEEZEFS_TEST_KMBUF_REGISTER_ENOMEM", "0");
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
}

/// A data volume of three 4 MiB(+4 KiB) chunks: two reserved, one usable.
const data_volume_len: u64 = 3 * ((4 << 20) + 4096) + 4096;

test "smoke: a SET format — meta + 2 data files + lz4 + aes-256-gcm + a cache path → both data superblocks written and read back → status lists both tags → a re-format refuses on the meta AND on a data volume → a mount with no key refuses naming every source → with the key it mounts, logs the set config and the key id, serves, unmounts → renumbered or blank data devices refuse the mount by name" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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

const transform_mod = @import("../data/transform.zig");
const bench_terms = @import("bench.zig").StatsTerms;

/// The transformed append row's appends: 72 B lines, each its own fsync'd
/// O_APPEND write (one drain per append).
const xform_appends = 40;

test "smoke: THE TRANSFORMED APPEND on a set without a cache path (decision 0003 §AK) — lz4, then aes-256-gcm: a file past the inline bound grows by 40 fsync'd 72 B appends and reads back exact (the write side seeded a transformed block's image through a block-sized buffer the chunk-sized read window refused: EIO at the first write past 4 KiB); a transformed block cannot be patched in place, so each drain lands the block's framed image whole — trimmed to its live bytes: ≤ the stored bound of the file's bytes per drain (8,192 B on aes-256-gcm for a 7,880 B file), where the untrimmed image of an encrypted set was the whole block; a read past the image's plaintext within EOF serves zeros" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    for ([_]transform_mod.Encryption{ .none, .aes256gcm }) |enc| {
        const rig = try Rig.init(gpa, io);
        defer rig.deinit();
        var kp_buf: [160]u8 = undefined;
        const kp = try std.fmt.bufPrint(&kp_buf, "{s}/set.key", .{rig.work});
        const cfg: transform_mod.VolumeTransform = .{ .compression = if (enc == .none) .lz4 else .none, .encryption = enc, .block_size = @intCast(4 * derive.MiB) };
        if (enc == .none) {
            try rig.formatSetWith(false, &.{ "--compress", "lz4" });
            try rig.spawnMount(&.{});
        } else {
            const out = try rig.shOk(rig.work, "umask 077; head -c 48 /dev/urandom | base64 > set.key; chmod 600 set.key");
            gpa.free(out);
            try rig.formatSetWith(false, &.{ "--encrypt", "aes-256-gcm", "--encrypt-key", kp });
            try rig.spawnMount(&.{ "--encrypt-key", kp });
        }
        _ = try rig.awaitReady(0);
        // Past the inline bound: 4,096 B inline, then 904 B that promote the file.
        const sp = try rig.stats();
        defer gpa.free(sp);
        {
            const out = try rig.onMountTimed("head -c 5000 /dev/urandom > \"$1/ref\" && dd if=\"$1/ref\" of=f bs=4096 status=none && cmp f \"$1/ref\" && echo same", 60);
            defer gpa.free(out);
            try testing.expectEqualStrings("same\n", out);
        }
        // The promotion's own-block DMA counts the bytes it landed (the
        // stored image, grain-padded): the device-write ledger closes.
        {
            const sq = try rig.stats();
            defer gpa.free(sq);
            const own = (numberAfter(sq, "\"own_block_dma_bytes\":") orelse 0) - (numberAfter(sp, "\"own_block_dma_bytes\":") orelse 0);
            try testing.expect(own != 0 and own % 4096 == 0 and own <= transform_mod.storedImageBound(cfg, 4096));
            if (enc != .none) try testing.expectEqual(transform_mod.storedImageBound(cfg, 4096), own);
        }
        const st0 = try rig.stats();
        defer gpa.free(st0);
        {
            var script: std.Io.Writer.Allocating = .init(gpa);
            defer script.deinit();
            try script.writer.print("i=0; while [ $i -lt {d} ]; do printf '%071d\\n' $i >> \"$1/ref\"; printf '%071d\\n' $i | dd of=f oflag=append conv=notrunc,fsync status=none; i=$((i+1)); done; cmp f \"$1/ref\" && echo same", .{xform_appends});
            const out = try rig.onMountTimed(script.written(), 120);
            defer gpa.free(out);
            try testing.expectEqualStrings("same\n", out);
        }
        const st1 = try rig.stats();
        defer gpa.free(st1);
        const device = bench_terms.parse(st1).device_write_bytes - bench_terms.parse(st0).device_write_bytes;
        const per_drain = transform_mod.storedImageBound(cfg, 5000 + xform_appends * 72);
        std.debug.print("\nsmoke[xform append {t}]: {d} appends of 72 B drained as {d} B of images (×{d} per appended byte; the bound {d} B per drain)\n", .{ enc, xform_appends, device, device / (xform_appends * 72), per_drain });
        try testing.expect(device > 0);
        try testing.expect(device <= xform_appends * per_drain);
        // A read past the image's plaintext within EOF: zeros.
        {
            const out = try rig.onMountTimed("truncate -s 300000 f && cp \"$1/ref\" \"$1/ref2\" && truncate -s 300000 \"$1/ref2\" && dd if=f of=\"$1/tail\" bs=4096 skip=60 count=1 iflag=direct status=none && cmp -n 4096 \"$1/tail\" /dev/zero && cmp f \"$1/ref2\" && echo same", 60);
            defer gpa.free(out);
            try testing.expectEqualStrings("same\n", out);
        }
    }
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
/// Then the shared-mode classifier's repro (fstests generic/013,
/// `write_shared_class_violation`): an 8 MiB striped file shrunk to
/// 1.3 MB — the kept block's zero tail `[1.3 MB, 4 MiB)` is a write the
/// plane folds and uploads with the BLOCK END as its map reach floor —
/// fsync (the upload's merge lands), then ONE extending 4 KiB write at
/// 1.5 MiB, past the new EOF and inside that block. The map merge used
/// to publish its reach floor (4 MiB) as the inode's shape, so the write
/// read as Shared on a size the file no longer had: `expectMustStayZero`
/// on the pass's tail is the assertion; the mirror pins the bytes.
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
    \\dd if="$W/image" of=shrunk bs=1M count=8 status=none
    \\dd if="$W/image" of="$W/shrunk" bs=1M count=8 status=none
    \\sync shrunk
    \\truncate -s 1300000 shrunk
    \\truncate -s 1300000 "$W/shrunk"
    \\sync shrunk
    \\head -c 4096 /dev/urandom > "$W/page"
    \\dd if="$W/page" of=shrunk bs=4k seek=384 count=1 conv=notrunc status=none
    \\dd if="$W/page" of="$W/shrunk" bs=4k seek=384 count=1 conv=notrunc status=none
    \\test "$(stat -c %s shrunk)" = 1576960
    \\sync shrunk
    \\cmp shrunk "$W/shrunk"
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
    // The commit and CQE halves' TRIPWIRES only: `transport_park_tick_late_
    // commits` / `_late_cqes` count replies and completions that arrived
    // after a tick's deadline (late, not a fault).
    "\"transport_park_tick_commit_lost_wakes\":",
    "\"transport_park_tick_cqe_lost_wakes\":",
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
        const promotions = promotionsToStriped(tail);
        const clips = numberAfter(tail, "\"fuse_truncate_striped_clips\":") orelse 0;
        // Every big file (`big`, the cp, the storm's) left the ring for its
        // own block exactly once — by its next write past the block (the
        // filesystem's promotion) or by the plane's promotion dispatch at
        // ring pressure or fsync, whichever ran first (the race is the
        // schedule's: the storm's ring pressure promotes residents before
        // their writers' next write). Two striped shrinks in stage C: `big`
        // 200 → 150 MiB and `shrunk` 8 MiB → 1.3 MB.
        std.debug.print("smoke[{s}]: write routing striped {d} / staged {d}, promotions to striped {d} by the write path + {d} by the plane's dispatch, striped clips {d}\n", .{ label, striped, staged, promotions.by_write, promotions.by_plane, clips });
        try testing.expect(striped > 0);
        try testing.expect(promotions.total() >= 1 + 1 + storm_files);
        try testing.expectEqual(@as(u64, 2), clips);
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
    const destroys0 = blk: {
        const tail = try rig.statsTail();
        defer gpa.free(tail);
        break :blk numberAfter(tail, "\"fuse_reclaim_destroys\":") orelse 0;
    };
    {
        const out = try rig.onMount("rm -f big big100 shrunk storm0 storm1 storm2 storm3 storm4 storm5 storm6 storm7 && echo rm-ok");
        defer gpa.free(out);
    }
    // What stays is the small files: striped on the set without staging
    // (df returns to the baseline plus them, exactly); in the ring on the
    // staging set — EXCEPT the ones the plane's dispatch packed at the
    // storm's ring pressure (the schedule that wins the promotions above),
    // whose pack blocks are used space. There the rm's frees are judged
    // by the online census once the reclaim drained — a big file's block
    // left tracked is `[C2] leaked`, a count off is C3/C6 — and df by the
    // pack census's bound.
    if (cache) {
        // The unlinked files are destroyed at their kernel FORGET (corpses
        // until then, their blocks referenced): every one first, then the
        // reclaim drained.
        const removed: u64 = 3 + storm_files;
        {
            const destroyed = try awaitAtLeast(rig, "\"fuse_reclaim_destroys\":", destroys0 + removed, 20 * std.time.ns_per_s);
            defer gpa.free(destroyed);
            try testing.expect((numberAfter(destroyed, "\"fuse_reclaim_destroys\":") orelse 0) >= destroys0 + removed);
        }
        const tail = try rig.quiescentStatsTail();
        gpa.free(tail);
        var got = try fsckJsonTarget(rig, rig.mp, &.{"--online"});
        defer got.report.deinit(gpa);
        if (got.report.findings.len != 0) for (got.report.findings) |f| std.debug.print("  unexpected: {f}\n", .{f});
        try testing.expectEqual(@as(usize, 0), got.report.findings.len);
        const used = try rig.usedKib();
        std.debug.print("smoke[{s}]: after rm: fsck --online 0 findings over {d} block(s); {d} KiB used above the baseline (the packs the dispatch filled, bound {d} blocks)\n", .{ label, got.report.counters.blocks_checked, used -| baseline, pack_census_bound });
        try testing.expect(used >= baseline and used - baseline <= pack_census_bound * matrix_block_kib);
    } else {
        try rig.awaitUsedKib(baseline + small_n * (4 << 10));
    }
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
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const rig = try Rig.init(testing.allocator, io);
    defer rig.deinit();
    try dataPlanePass(rig, true);
}

test "smoke: the data plane WITHOUT a cache path — staging and the NVMe tier OFF, every write beyond inline striped; the same pass with 100 one-block files" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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

/// The promotions a file takes out of the ring (or inline) to its own
/// block: the filesystem's at a write past the block, the plane's dispatch
/// (ring pressure, fsync, dismount) onto its own-block arm.
const Promotions = struct {
    by_write: u64,
    by_plane: u64,

    fn total(self: Promotions) u64 {
        return self.by_write + self.by_plane;
    }
};

fn promotionsToStriped(tail: []const u8) Promotions {
    return .{ .by_write = numberAfter(tail, "\"fuse_write_promotions_to_striped\":") orelse 0, .by_plane = numberAfter(tail, "\"layout_promoted_block\":") orelse 0 };
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
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    var env = try testing.environ.createMap(gpa);
    defer env.deinit();
    const raw = env.get(smoke_loops_knob) orelse return testkit.skip(@src(), .no_seam, smoke_loops_knob ++ " unset: the looped single-stage repro runs only when the harness arms it");
    const loops = std.fmt.parseInt(u32, std.mem.trim(u8, raw, " \t"), 10) catch return testkit.skip(@src(), .no_seam, smoke_loops_knob ++ " is not an integer");
    if (loops == 0) return testkit.skip(@src(), .no_seam, smoke_loops_knob ++ " is 0");
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
    // Every storm pass writes FRESH files (the previous pass's removed
    // first: the storm's destroys ride the loop too), so each pass takes
    // its eight promotions out of the ring, by either arm.
    pass = 0;
    var promoted = blk: {
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        break :blk promotionsToStriped(tail);
    };
    while (pass < loops) : (pass += 1) {
        const rm = try rig.onMountTimed("rm -f storm0 storm1 storm2 storm3 storm4 storm5 storm6 storm7\nsync -f .\n", stage_loop_timeout_s);
        gpa.free(rm);
        const t0 = clock.monotonicNs();
        const out = try rig.onMountTimed(stage_storm_script, stage_loop_timeout_s);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "stage-storm-ok") != null);
        const tail = try rig.quiescentStatsTail();
        defer gpa.free(tail);
        try expectMustStayZero(tail);
        try expectLedgersClosed(tail);
        try expectOpLatencyWithin(tail, op_latency_bound);
        const now = promotionsToStriped(tail);
        try testing.expect(now.total() - promoted.total() >= storm_files);
        std.debug.print("smoke[loop]: storm pass {d}/{d}: {d} MB/s aggregate in {d} ms; promotions {d} by the write path + {d} by the plane; channel ticks {d}, watchdog overdue {d}, joins {d}, close waits {d}\n", .{ pass + 1, loops, mbPerS(out, "storm_ns=", storm_files * (storm_mib << 20)), (clock.monotonicNs() - t0) / std.time.ns_per_ms, now.by_write - promoted.by_write, now.by_plane - promoted.by_plane, numberAfter(tail, "\"channel_ticked_reregisters\":") orelse 0, numberAfter(tail, "\"fuse_op_watchdog_overdue\":") orelse 0, numberAfter(tail, "\"write_pipeline_joins\":") orelse 0, numberAfter(tail, "\"rewrite_shadow_close_waits\":") orelse 0 });
        promoted = now;
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

/// OUR oracle after a kill (charter §1.10 "fsck clean after every kill"):
/// `squeezefs fsck --offline --scrub` over the unmounted set — the D0
/// guarded open replays the ring, then C1–C17 and the scrub must report
/// `findings: 0 (clean)` with exit 0.
fn fsckClean(rig: *Rig, label: []const u8) !Recovery {
    const r = try rig.squeezefs(&.{ "fsck", "--offline", "--scrub", rig.meta_uri });
    defer rig.freeRun(r);
    if (!r.term.success() or std.mem.indexOf(u8, r.stdout, "findings: 0 (clean)") == null) {
        std.debug.print("\n--- fsck --offline after {s} ({f}) ---\n{s}\n--- stderr ---\n{s}\n", .{ label, r.term, r.stdout, r.stderr });
        return error.FsckNotClean;
    }
    std.debug.print("smoke[kill]: fsck --offline --scrub after {s}: {s}", .{ label, r.stdout[0 .. std.mem.indexOfScalar(u8, r.stdout, '\n') orelse r.stdout.len] });
    std.debug.print(" → findings: 0 (clean)\n", .{});
    // fsck's guarded open IS the post-kill recovery (the replay, the ring)
    // and its close checkpoints: the recovery numbers are its log's.
    return recoveryOf(r.stderr);
}

fn recoveryOf(seg: []const u8) !Recovery {
    const torn_at = std.mem.indexOf(u8, seg, "content records, ") orelse return error.NoReplayLine;
    return .{
        .replayed = numberAfter(seg, "replayed ") orelse 0,
        .torn = numberAfter(seg[torn_at..], "content records, ") orelse 0,
        .ring_records = numberAfter(seg, "recovered ") orelse 0,
        .ring_torn = if (std.mem.indexOf(u8, seg, "record(s) (")) |i| (numberAfter(seg[i..], "), ") orelse 0) else 0,
    };
}

/// Remount with the C8 oracle armed: zero drift (C8) and nothing leaked
/// or lost (C2) on both volumes. `recovered` = the replay fsck's guarded
/// open already performed (its close checkpointed, so THIS open is clean
/// by design); null = this open replays and its numbers are read here.
fn remountAfterKill(rig: *Rig, recovered: ?Recovery) !Recovery {
    const mark = try rig.logLen();
    try rig.spawnMount(&.{});
    const ready = try rig.awaitReady(mark);
    const text = try rig.readLog();
    defer rig.gpa.free(text);
    const seg = text[mark..ready];
    if (recovered == null) try testing.expect(std.mem.indexOf(u8, seg, ", clean unmount") == null);
    if (std.mem.count(u8, seg, " 0 drift row(s)") != 2 or std.mem.count(u8, seg, ": 0 leaked block(s) freed, 0 lost block(s)") != 2) {
        std.debug.print("\n--- the oracles after the kill ---\n{s}\n", .{seg});
        return error.DriftAfterKill;
    }
    return recovered orelse try recoveryOf(seg);
}

test "smoke: THE KILL MATRIX — kill -9 at 50/100/150 MiB of a 200 MiB dd and mid 8-way cp storm → fsck --offline clean → remount → every fsync'd file byte-exact, every page of an in-flight file exact or a hole, C8 zero drift, C2 nothing leaked or lost, rm → df baseline → fsck clean → status" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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
        const recovered = try fsckClean(rig, "the mid-dd kill");
        const rec = try remountAfterKill(rig, recovered);
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
        const recovered = try fsckClean(rig, "the storm kill");
        const rec = try remountAfterKill(rig, recovered);
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
    _ = try fsckClean(rig, "the clean unmount");
    {
        const report = try rig.statusReport();
        defer gpa.free(report);
        try testing.expectEqual(@as(u64, 0), refCensus(report).blocks);
        try testing.expect(std.mem.indexOf(u8, report, "not mounted") != null);
        std.debug.print("smoke[kill]: {s}", .{report[std.mem.lastIndexOf(u8, report, "summary:").?..]});
    }
}

// ---- fsck / scrub: OUR oracle over a real set (rung 8) ----

/// `squeezefs fsck …` through the binary; the report's JSON, the exit code
/// and both streams (printed on a surprise).
fn fsckJson(rig: *Rig, extra: []const []const u8) !FsckOutcome {
    var argv: [12][]const u8 = undefined;
    var n: usize = 0;
    argv[n] = "fsck";
    n += 1;
    argv[n] = "--offline";
    n += 1;
    argv[n] = "--json";
    n += 1;
    for (extra) |a| {
        argv[n] = a;
        n += 1;
    }
    argv[n] = rig.meta_uri;
    n += 1;
    const r = try rig.squeezefs(argv[0..n]);
    defer rig.freeRun(r);
    const code: u8 = switch (r.term) {
        .exited => |c| c,
        else => 255,
    };
    const report = json_scan_report.Report.parse(rig.gpa, r.stdout) catch {
        std.debug.print("\n--- fsck ({f}) printed no report ---\n{s}\n--- stderr ---\n{s}\n", .{ r.term, r.stdout, r.stderr });
        return error.FsckNoReport;
    };
    return .{ .report = report, .code = code };
}

const json_scan_report = @import("../ops/fsck/finding.zig");

/// The fresh file of the `sync -f` row: one MiB and a 1000-byte sub-page
/// tail — a partial first block at every legal block size ≥ 2 MiB (the
/// default 4 MiB here), so the bytes sit parked in the accumulation table
/// until a drain; the tail ends off any page boundary.
const syncfs_fresh_bytes: u64 = (1 << 20) + 1000;
/// The fsync'd file the row overwrites in place: one default block.
const syncfs_base_bytes: u64 = 4 << 20;
/// The in-place overwrite at the base file's head: two pages, size-neutral
/// (its mtime is a parked stamp under the coalesced cadence, §L.6).
const syncfs_patch_bytes: u64 = 8192;

test "smoke: `sync -f` IS A DURABILITY POINT FOR THE DAEMON'S WRITE CUSTODY — format (no cache path: every write beyond inline striped) → mount → the test process holds `f` and `g` OPEN and closes neither (no RELEASE, no FLUSH drains them; it spawns nothing while it holds them): 1 MiB + 1000 B into the fresh `f` (a partial block parked in RAM), 8 KiB over the head of the fsync'd `g` (a size-neutral write: its mtime parked) → syncfs(2) on the mount returns → kill -9 the daemon → fsck --offline clean → remount → `f` is the image byte for byte, `g` carries the overwrite and the mtime the holder saw" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(false);
    try rig.createImage();
    // The C8 oracle at every mount (the remount after the kill judges it).
    try rig.env.put("SQUEEZEFS_BLOCK_REFS_VERIFY", "1");
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    // The kernel forwards syncfs to a plain /dev/fuse server only once INIT
    // negotiated FUSE_HAS_SYNCFS (upstream 7.47; the venue's 7.2.6-sqz
    // through scripts/kernel/patches-7.2/0027): without it `sync -f` never
    // reaches the daemon and the row has nothing to judge.
    {
        const st = try rig.stats();
        defer gpa.free(st);
        if (numberAfter(st, "\"fuse_syncfs_armed\":") != 1) {
            const rel = try rig.shOk(rig.work, "uname -r");
            defer gpa.free(rel);
            try rig.umount();
            _ = try rig.waitChild();
            var detail: [192]u8 = undefined;
            return testkit.skip(@src(), .no_kernel_feature, std.fmt.bufPrint(&detail, "kernel {s}: FUSE_HAS_SYNCFS not offered — syncfs(2) never reaches a plain /dev/fuse server", .{std.mem.trimEnd(u8, rel, "\n")}) catch "a kernel without FUSE_HAS_SYNCFS");
        }
    }
    {
        var script: [256]u8 = undefined;
        const out = try rig.onMount(try std.fmt.bufPrint(&script, "dd if=\"$1/image\" of=g bs={d} count=1 conv=fsync status=none && echo base-ok", .{syncfs_base_bytes}));
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "base-ok") != null);
    }
    // The holder is THIS process: both fds stay open past the kill and no
    // fd of either file is ever closed before it — a close(2) of ANY fd (a
    // child's inherited copy too, closed at its exec even under CLOEXEC) is
    // a FLUSH, the no-open drain (decision 0003 §AF) — so nothing is
    // spawned while they are held and syncfs(2) is the only drain the
    // bytes meet.
    const head = try readImageRange(gpa, rig.image, 0, syncfs_fresh_bytes);
    defer gpa.free(head);
    const tail = try readImageRange(gpa, rig.image, (image_mib << 20) - syncfs_patch_bytes, syncfs_patch_bytes);
    defer gpa.free(tail);
    var path_buf: [256]u8 = undefined;
    const fd_f = try openRaw(try std.fmt.bufPrintSentinel(&path_buf, "{s}/f", .{rig.mp}, 0), .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true, .CLOEXEC = true });
    defer _ = linux.close(fd_f);
    const fd_g = try openRaw(try std.fmt.bufPrintSentinel(&path_buf, "{s}/g", .{rig.mp}, 0), .{ .ACCMODE = .RDWR, .CLOEXEC = true });
    defer _ = linux.close(fd_g);
    try pwriteAll(fd_f, head, 0);
    try pwriteAll(fd_g, tail, 0);
    var g_path_buf: [256]u8 = undefined;
    const g_path = try std.fmt.bufPrintSentinel(&g_path_buf, "{s}/g", .{rig.mp}, 0);
    const seen_mtime = try mtimeOf(g_path);
    // `sync -f <mnt>` is syncfs(2) on an fd of the mount.
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.syncfs(fd_f)));
    // The daemon served the syncfs and its census found the parked block.
    {
        const st = try rig.stats();
        defer gpa.free(st);
        std.debug.print("\nsmoke[sync -f]: fuse_syncfs {d}, custody inos {d}, parked blocks flushed {d}\n", .{ numberAfter(st, "\"fuse_syncfs\":") orelse 0, numberAfter(st, "\"syncfs_custody_inos\":") orelse 0, numberAfter(st, "\"syncfs_parked_blocks\":") orelse 0 });
        try testing.expect((numberAfter(st, "\"fuse_syncfs\":") orelse 0) >= 1);
        try testing.expect((numberAfter(st, "\"syncfs_parked_blocks\":") orelse 0) >= 1);
    }
    try rig.kill();
    const recovered = try fsckClean(rig, "the kill after sync -f");
    _ = try remountAfterKill(rig, recovered);
    var check: [768]u8 = undefined;
    const out = try rig.onMount(try std.fmt.bufPrint(&check, "if head -c {d} \"$1/image\" | cmp -s - f; then echo f-whole; else echo f-LOST; fi; if {{ tail -c {d} \"$1/image\"; head -c {d} \"$1/image\" | tail -c +{d}; }} | cmp -s - g; then echo g-whole; else echo g-LOST; fi", .{ syncfs_fresh_bytes, syncfs_patch_bytes, syncfs_base_bytes, syncfs_patch_bytes + 1 }));
    defer gpa.free(out);
    const after_mtime = try mtimeOf(g_path);
    std.debug.print("\nsmoke[sync -f]: after the kill and the remount: {s}; g's mtime {d}.{d:0>9} (the holder saw {d}.{d:0>9})\n", .{ std.mem.trimEnd(u8, out, "\n"), after_mtime.sec, after_mtime.nsec, seen_mtime.sec, seen_mtime.nsec });
    try testing.expect(std.mem.indexOf(u8, out, "f-whole") != null);
    try testing.expect(std.mem.indexOf(u8, out, "g-whole") != null);
    try testing.expectEqual(seen_mtime, after_mtime);
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

/// `open(2)` with mode 0644 for a create (the holder rows' fds, the image reads).
fn openRaw(path: [:0]const u8, flags: linux.O) !linux.fd_t {
    const rc = linux.open(path.ptr, flags, 0o644);
    if (linux.errno(rc) != .SUCCESS) {
        std.debug.print("\nopen {s}: {t}\n", .{ path, linux.errno(rc) });
        return error.OpenFailed;
    }
    return @intCast(rc);
}

/// A path's mtime by `statx` (no open, so no close and no FLUSH).
const Mtime = struct { sec: i64, nsec: u32 };

fn mtimeOf(path: [:0]const u8) !Mtime {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, path.ptr, 0, .{ .MTIME = true }, &st)) != .SUCCESS) return error.StatFailed;
    return .{ .sec = st.mtime.sec, .nsec = st.mtime.nsec };
}

/// The mount's `.stats` read by THIS process (no child: a row holding a
/// dirty fd spawns nothing — a child's exec closes its copy, a FLUSH).
fn statsDirect(rig: *Rig) ![]u8 {
    var pb: [256]u8 = undefined;
    const fd = try openRaw(try std.fmt.bufPrintSentinel(&pb, "{s}/.stats", .{rig.mp}, 0), .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
    defer _ = linux.close(fd);
    var out: std.ArrayList(u8) = .empty;
    errdefer out.deinit(rig.gpa);
    var buf: [64 << 10]u8 = undefined;
    while (true) {
        const n = linux.read(fd, &buf, buf.len);
        if (linux.errno(n) != .SUCCESS) return error.ReadFailed;
        if (n == 0) break;
        try out.appendSlice(rig.gpa, buf[0..n]);
    }
    return out.toOwnedSlice(rig.gpa);
}

/// `awaitStats` over `statsDirect`: every `(key, want)` holds in ONE read
/// or the deadline passed (owned; the last read either way).
fn awaitStatsDirect(rig: *Rig, pairs: []const struct { key: []const u8, want: u64 }, deadline_ns: u64) ![]u8 {
    const deadline = clock.monotonicNs() + deadline_ns;
    while (true) {
        const st = try statsDirect(rig);
        var ok = true;
        for (pairs) |p| ok = ok and (numberAfter(st, p.key) orelse std.math.maxInt(u64)) == p.want;
        if (ok or clock.monotonicNs() >= deadline) return st;
        rig.gpa.free(st);
        sleepMs(100);
    }
}

/// `len` bytes of the rig's image at `off` (owned).
fn readImageRange(gpa: std.mem.Allocator, path: [:0]const u8, off: u64, len: u64) ![]u8 {
    const fd = try openRaw(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
    defer _ = linux.close(fd);
    const out = try gpa.alloc(u8, @intCast(len));
    errdefer gpa.free(out);
    var done: usize = 0;
    while (done < out.len) {
        const n = linux.pread(fd, out[done..].ptr, out.len - done, @intCast(off + done));
        if (linux.errno(n) != .SUCCESS or n == 0) return error.ReadFailed;
        done += n;
    }
    return out;
}

/// Every byte of `data` at `off` (a short write continues).
fn pwriteAll(fd: linux.fd_t, data: []const u8, off: u64) !void {
    var done: usize = 0;
    while (done < data.len) {
        const n = linux.pwrite(fd, data[done..].ptr, data.len - done, @intCast(off + done));
        if (linux.errno(n) != .SUCCESS) return error.WriteFailed;
        if (n == 0) return error.WriteFailed;
        done += n;
    }
}

/// The close drain rows' set-up (decision 0003 §AF): `g` exists and is read
/// (on a no-open mount the daemon latched the connection at ready, so no
/// CREATE-opened file — `g`, `f` — is ever released with a RELEASE), then
/// 1 MiB + 1000 B into the fresh `f` (`>`: a CREATE and its close) and the
/// same appended to `g` (`>>`: an open with no OPEN, its close): a partial
/// block of each parked in RAM until a drain.
const close_drain_script = "head -c 4096 \"$1/image\" > g && cat g > /dev/null && head -c {d} \"$1/image\" > f && head -c {d} \"$1/image\" >> g && echo written";
const close_drain_check = "if head -c {d} \"$1/image\" | cmp -s - f; then echo f-whole; else echo f-LOST; fi; if {{ head -c 4096 \"$1/image\"; head -c {d} \"$1/image\"; }} | cmp -s - g; then echo g-whole; else echo g-LOST; fi";

/// One posture of the close drain row: write, close, (the control arm:
/// wait for its asynchronous RELEASE to drain the parked blocks), kill -9,
/// fsck, remount, both files whole.
fn closeDrainRow(no_open: bool) !void {
    const io = testing.io;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(false);
    try rig.createImage();
    try rig.env.put("SQUEEZEFS_FUSE_NO_OPEN", if (no_open) "1" else "0");
    try rig.env.put("SQUEEZEFS_BLOCK_REFS_VERIFY", "1");
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    var script: [512]u8 = undefined;
    {
        const out = try rig.onMount(try std.fmt.bufPrint(&script, close_drain_script, .{ syncfs_fresh_bytes, syncfs_fresh_bytes }));
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "written") != null);
    }
    const st = if (no_open) try rig.stats() else try awaitExactly(rig, "\"parked_buffers\":", 0, 20 * std.time.ns_per_s);
    defer gpa.free(st);
    const enosys = numberAfter(st, "\"fuse_open_enosys\":") orelse 0;
    const drains = numberAfter(st, "\"fuse_flush_drains\":") orelse 0;
    const clean = numberAfter(st, "\"transport_flush_inline_serves\":") orelse 0;
    std.debug.print("\nsmoke[close drain, no_open={}]: OPENs answered ENOSYS {d}, FLUSH drains {d}, FLUSHes answered on the worker {d}, owed {d}, parked buffers {d}\n", .{ no_open, enosys, drains, clean, numberAfter(st, "\"fuse_owed_drain_inos\":") orelse 0, numberAfter(st, "\"parked_buffers\":") orelse 0 });
    try testing.expectEqual(@as(?u64, 0), numberAfter(st, "\"parked_buffers\":"));
    if (no_open) {
        // The latch: the daemon's own open at ready — `cat g` sent none,
        // and no CREATE-opened file was released with a RELEASE.
        try testing.expectEqual(@as(u64, 1), enosys);
        try testing.expectEqual(@as(?u64, 0), numberAfter(st, "\"fuse_no_open_releases\":"));
        // `head` closing its stdout drained each partial block.
        try testing.expect(drains >= 2);
        try testing.expectEqual(@as(?u64, 0), numberAfter(st, "\"fuse_owed_drain_inos\":"));
    } else {
        try testing.expectEqual(@as(u64, 0), enosys);
        try testing.expectEqual(@as(u64, 0), drains);
    }
    try rig.kill();
    const recovered = try fsckClean(rig, "the kill after the close");
    _ = try remountAfterKill(rig, recovered);
    const out = try rig.onMount(try std.fmt.bufPrint(&script, close_drain_check, .{ syncfs_fresh_bytes, syncfs_fresh_bytes }));
    defer gpa.free(out);
    std.debug.print("smoke[close drain, no_open={}]: after the kill and the remount: {s}\n", .{ no_open, std.mem.trimEnd(u8, out, "\n") });
    try testing.expect(std.mem.indexOf(u8, out, "f-whole") != null);
    try testing.expect(std.mem.indexOf(u8, out, "g-whole") != null);
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

test "smoke: A NO-OPEN CLOSE IS THE WRITE-BACK DRAIN (decision 0003 §AF) — format (no cache path: a partial block parks in RAM) → mount with SQUEEZEFS_FUSE_NO_OPEN=1 (the daemon latches the connection at ready: its own `.stats` open, the ONE OPEN answered ENOSYS) → `g` written and read → 1 MiB + 1000 B into a fresh `f` (a CREATE: no RELEASE will ever come) and appended to `g` (no OPEN at all) by plain redirections → kill -9 the moment the last close returned: nothing parked, nothing owed, no RELEASE, ≥ 2 FLUSH drains, ONE OPEN → fsck --offline clean → remount → both files whole" {
    if (venueMissing(testing.io)) |why| return testkit.skipWhy(@src(), why);
    try closeDrainRow(true);
}

test "smoke: THE CONTROL ARM'S CLOSE DRAIN (SQUEEZEFS_FUSE_NO_OPEN=0) — the same writes; the asynchronous RELEASE drains the parked blocks after close(2) returned (observed: parked 0, no OPEN answered ENOSYS, no FLUSH drain) → kill -9 → fsck --offline clean → remount → both files whole" {
    if (venueMissing(testing.io)) |why| return testkit.skipWhy(@src(), why);
    try closeDrainRow(false);
}

/// The owed row's writer: an extending write that parks a partial block
/// (no cache path), the pattern the remount checks.
const owed_write_bytes: usize = (1 << 20) + 1000;

fn owedByte(i: usize) u8 {
    return @truncate(i *% 31 +% 7);
}

test "smoke: CUSTODY NO CLOSE COVERS IS DRAINED ONCE IDLE (decision 0003 §AF) — no-open, no cache path, the owed idle horizon at 0 (the seam), the connection latched at ready (no RELEASE would drain `m` either): a writer creates `m` and writes 1 MiB + 1000 B (a parked partial block) and holds its fd OPEN — no close, so no FLUSH follows the write: the class a mapping written back after its fd closed (`fuse_vma_close` at munmap) and an async write landing after its close share ⇒ the ino is owed → the reclaim cadence drains it (parked 0, owed 0, ONE idle drain, no FLUSH drain) → kill -9 with the fd still open → fsck --offline clean → remount → the bytes are there" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(false);
    try rig.env.put("SQUEEZEFS_FUSE_NO_OPEN", "1");
    try rig.env.put("SQUEEZEFS_TEST_FUSE_OWED_IDLE_MS", "0");
    try rig.env.put("SQUEEZEFS_BLOCK_REFS_VERIFY", "1");
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    var path_buf: [256]u8 = undefined;
    const path = try std.fmt.bufPrintSentinel(&path_buf, "{s}/m", .{rig.mp}, 0);
    const pattern = try gpa.alloc(u8, owed_write_bytes);
    defer gpa.free(pattern);
    for (pattern, 0..) |*b, i| b.* = owedByte(i);
    const fd = try openRaw(path, .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true, .CLOEXEC = true });
    // Held past the kill: never closed while the daemon lives.
    defer _ = linux.close(fd);
    try pwriteAll(fd, pattern, 0);
    {
        // Every word settled in one render: a render is no atomic cut, so
        // the render that first shows the drain's count can carry a word
        // read before the drain (`parked_buffers` 1 beside the count).
        const st = try awaitStatsDirect(rig, &.{
            .{ .key = "\"fuse_owed_idle_drains\":", .want = 1 },
            .{ .key = "\"fuse_owed_drain_inos\":", .want = 0 },
            .{ .key = "\"parked_buffers\":", .want = 0 },
            .{ .key = "\"parked_extent_bytes\":", .want = 0 },
            .{ .key = "\"overlay_open\":", .want = 0 },
            .{ .key = "\"fuse_flush_drains\":", .want = 0 },
        }, 30 * std.time.ns_per_s);
        defer gpa.free(st);
        std.debug.print("\nsmoke[owed idle drain]: idle drains {d}, owed {d}, parked buffers {d}, parked extent bytes {d}, open overlays {d}, FLUSH drains {d}\n", .{ numberAfter(st, "\"fuse_owed_idle_drains\":") orelse 0, numberAfter(st, "\"fuse_owed_drain_inos\":") orelse 0, numberAfter(st, "\"parked_buffers\":") orelse 0, numberAfter(st, "\"parked_extent_bytes\":") orelse 0, numberAfter(st, "\"overlay_open\":") orelse 0, numberAfter(st, "\"fuse_flush_drains\":") orelse 0 });
        try testing.expectEqual(@as(?u64, 1), numberAfter(st, "\"fuse_owed_idle_drains\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(st, "\"fuse_owed_drain_inos\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(st, "\"parked_buffers\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(st, "\"parked_extent_bytes\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(st, "\"overlay_open\":"));
        // No close drained it: the custody came after every FLUSH.
        try testing.expectEqual(@as(?u64, 0), numberAfter(st, "\"fuse_flush_drains\":"));
    }
    try rig.kill();
    const recovered = try fsckClean(rig, "the kill after the idle drain");
    _ = try remountAfterKill(rig, recovered);
    {
        const rfd = try openRaw(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
        defer _ = linux.close(rfd);
        const got = try gpa.alloc(u8, owed_write_bytes + 1);
        defer gpa.free(got);
        var done: usize = 0;
        while (done < got.len) {
            const n = linux.pread(rfd, got[done..].ptr, got.len - done, @intCast(done));
            if (linux.errno(n) != .SUCCESS) return error.ReadFailed;
            if (n == 0) break;
            done += n;
        }
        try testing.expectEqual(owed_write_bytes, done);
        for (got[0..done], 0..) |b, i| if (b != owedByte(i)) {
            std.debug.print("\nsmoke[owed idle drain]: byte {d} reads 0x{x} after the remount — the write was LOST\n", .{ i, b });
            return error.OwedWriteLost;
        };
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

/// The close-burst row: forked appenders sharing one fd.
const burst_children: usize = 200;

test "smoke: A CLOSE BURST COALESCES ITS DRAINS (decision 0003 §AF) — no-open, no cache path: `log` written past inline (its close drains), then a shell holds it open for append and 200 forked `/bin/echo` children write a line each through it — every child's exit closes its copy, a FLUSH with custody ⇒ ONE drains (the first), the rest defer to the reclaim cadence's next tick (drains ≤ 2 + the windows the burst spanned, drains + deferred ≥ 200; drained each, they re-uploaded a whole block per close: 57,000× amplification measured) → the deferred custody lands (owed 0, parked 0) → kill -9 → fsck --offline clean → remount → every line there" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(false);
    try rig.createImage();
    try rig.env.put("SQUEEZEFS_FUSE_NO_OPEN", "1");
    try rig.env.put("SQUEEZEFS_BLOCK_REFS_VERIFY", "1");
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    var script: [512]u8 = undefined;
    const t0 = clock.monotonicNs();
    {
        const out = try rig.onMount(try std.fmt.bufPrint(&script, "head -c 8192 \"$1/image\" > log && exec 3>>log && i=0 && while [ $i -lt {d} ]; do /bin/echo \"line $i of the close burst\" >&3; i=$((i + 1)); done && echo appended", .{burst_children}));
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "appended") != null);
    }
    const burst_ns = clock.monotonicNs() - t0;
    {
        const st = try rig.stats();
        defer gpa.free(st);
        const drains = numberAfter(st, "\"fuse_flush_drains\":") orelse 0;
        const deferred = numberAfter(st, "\"fuse_flush_drains_deferred\":") orelse 0;
        // One drain per coalescing window the burst spanned (the window is
        // the reclaim tick), the prefill's and the last close's on top —
        // counted on the burst's own clock, so a loaded host only widens it.
        const bound = 2 + burst_ns / handler.watchdog.TICK_NS; // `mount.RECLAIM_CADENCE_NS`
        std.debug.print("\nsmoke[close burst]: {d} children in {d} ms: FLUSH drains {d} (bound {d}), deferred {d}, answered on the worker {d}\n", .{ burst_children, burst_ns / std.time.ns_per_ms, drains, bound, deferred, numberAfter(st, "\"transport_flush_inline_serves\":") orelse 0 });
        try testing.expect(drains >= 1 and drains <= bound);
        // Every child's write met its close: drained or deferred.
        try testing.expect(drains + deferred >= burst_children);
    }
    // The deferred custody lands — the cadence's next tick, or the last
    // close's own drain once a window passed. Both words at once: the
    // cadence harvests an owed ino BEFORE its drain lands, so owed reads 0
    // while the block is still parked for the drain's length.
    {
        const st = try awaitStats(rig, &.{ .{ .key = "\"fuse_owed_drain_inos\":", .want = 0 }, .{ .key = "\"parked_buffers\":", .want = 0 } }, 30 * std.time.ns_per_s);
        defer gpa.free(st);
        try testing.expectEqual(@as(?u64, 0), numberAfter(st, "\"fuse_owed_drain_inos\":"));
        try testing.expectEqual(@as(?u64, 0), numberAfter(st, "\"parked_buffers\":"));
    }
    try rig.kill();
    const recovered = try fsckClean(rig, "the kill after the close burst");
    _ = try remountAfterKill(rig, recovered);
    {
        const out = try rig.onMount(try std.fmt.bufPrint(&script, "{{ head -c 8192 \"$1/image\"; i=0; while [ $i -lt {d} ]; do echo \"line $i of the close burst\"; i=$((i + 1)); done; }} | cmp -s - log && echo log-whole", .{burst_children}));
        defer gpa.free(out);
        std.debug.print("smoke[close burst]: after the kill and the remount: {s}\n", .{std.mem.trimEnd(u8, out, "\n")});
        try testing.expect(std.mem.indexOf(u8, out, "log-whole") != null);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

test "smoke: O_TRUNC UNDER NO-OPEN TRUNCATES (decision 0003 §AF: INIT drops FUSE_ATOMIC_O_TRUNC, so the open's truncate is a SETATTR(size 0) under the size setter's guard — with the flag kept, the kernel skips that SETATTR and no OPEN carries it) — every TTL class at 60 s: on A, `: > f` over a non-empty file empties it; B JOINS; A writes `d/a`, B empties it with `: >` (B's SETATTR ships to the holder) ⇒ both read size 0 and no byte; B writes `d/b`, A empties it ⇒ both read 0" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_FUSE_NO_OPEN", "1");
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    for ([_][]const u8{ "SQUEEZEFS_FUSE_ATTR_TTL_MS", "SQUEEZEFS_FUSE_ENTRY_TTL_MS", "SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS", "SQUEEZEFS_FUSE_NEGATIVE_TTL_MS" }) |k| try rig.env.put(k, "60000");
    try rig.formatSet(true);
    try rig.createImage();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    // One daemon: a striped file and an inline one, each emptied by `: >`.
    {
        const out = try rig.onMount("head -c 3000000 \"$1/image\" > big && echo hello > small && cat big small > /dev/null && : > big && : > small && stat -c %s big small && cat big small | wc -c && mkdir d && echo ok");
        defer gpa.free(out);
        try testing.expectEqualStrings("0\n0\n0\nok\n", out);
    }
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{});
    _ = try b.awaitReady();
    // A's file, B's truncate (a cross-owner SETATTR).
    {
        const w = try rig.onMount("head -c 3000000 \"$1/image\" > d/a && stat -c %s d/a");
        defer gpa.free(w);
        try testing.expectEqualStrings("3000000\n", w);
        const seen = try b.onMount("stat -c %s d/a && head -c 16 d/a | wc -c");
        defer gpa.free(seen);
        try testing.expectEqualStrings("3000000\n16\n", seen);
        const t = try b.onMount(": > d/a && stat -c %s d/a && cat d/a | wc -c");
        defer gpa.free(t);
        try testing.expectEqualStrings("0\n0\n", t);
        // A's kernel learns it from its own notifier (0003 §AB.2 (2)):
        // bounded, a lost invalidation keeps the size for the 60 s TTL.
        const a = try rig.onMount("for i in $(seq 100); do [ \"$(stat -c %s d/a)\" = 0 ] && break; sleep 0.02; done; stat -c %s d/a && cat d/a | wc -c");
        defer gpa.free(a);
        try testing.expectEqualStrings("0\n0\n", a);
    }
    // B's file, A's truncate.
    {
        const w = try b.onMount("echo from-b > d/b && cat d/b");
        defer gpa.free(w);
        try testing.expectEqualStrings("from-b\n", w);
        const t = try rig.onMount("for i in $(seq 100); do [ -e d/b ] && break; sleep 0.02; done; cat d/b > /dev/null && : > d/b && stat -c %s d/b");
        defer gpa.free(t);
        try testing.expectEqualStrings("0\n", t);
        const seen = try b.onMount("for i in $(seq 100); do [ \"$(stat -c %s d/b)\" = 0 ] && break; sleep 0.02; done; stat -c %s d/b && cat d/b | wc -c");
        defer gpa.free(seen);
        try testing.expectEqualStrings("0\n0\n", seen);
    }
    {
        const ta = try rig.stats();
        defer gpa.free(ta);
        const tb = try b.stats();
        defer gpa.free(tb);
        std.debug.print("\nsmoke[O_TRUNC no-open]: OPENs answered ENOSYS A {d} / B {d}\n", .{ numberAfter(ta, "\"fuse_open_enosys\":") orelse 0, numberAfter(tb, "\"fuse_open_enosys\":") orelse 0 });
        try testing.expect((numberAfter(ta, "\"fuse_open_enosys\":") orelse 0) >= 1);
        try testing.expect((numberAfter(tb, "\"fuse_open_enosys\":") orelse 0) >= 1);
    }
}

/// `.stats` opened, fstat'ed and read whole through the same fd: the bytes
/// and the size the kernel reported for that generation (owned).
const VirtualRead = struct { ino: u64, size: u64, bytes: []u8 };

fn readVirtualWhole(gpa: std.mem.Allocator, path: [:0]const u8) !VirtualRead {
    const fd = try openRaw(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
    defer _ = linux.close(fd);
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, linux.STATX.BASIC_STATS, &st)) != .SUCCESS) return error.StatFailed;
    var out: std.ArrayList(u8) = .empty;
    errdefer out.deinit(gpa);
    var buf: [64 << 10]u8 = undefined;
    while (true) {
        const n = linux.read(fd, &buf, buf.len);
        if (linux.errno(n) != .SUCCESS) return error.ReadFailed;
        if (n == 0) break;
        try out.appendSlice(gpa, buf[0..n]);
    }
    return .{ .ino = st.ino, .size = st.size, .bytes = try out.toOwnedSlice(gpa) };
}

test "smoke: THE VIRTUAL INODES WITHOUT AN OPEN (decision 0003 §AF) — under no-open `.stats` is read through the page cache with no generation pin and no FOPEN_DIRECT_IO: 200 reads while a create storm moves every counter are each ONE whole JSON document whose length is exactly the st_size the same fd's fstat reported; two sequential opens read two generations (different inos, different bytes, each exact); a truncating open and a write are EACCES" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_FUSE_NO_OPEN", "1");
    try rig.formatSet(false);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const mk = try rig.onMount("mkdir hot && echo ok");
    gpa.free(mk);
    var storm = try Storm.start(rig, rig.mp, "virtual", "v");
    defer storm.deinit(rig);
    var path_buf: [256]u8 = undefined;
    const path = try std.fmt.bufPrintSentinel(&path_buf, "{s}/.stats", .{rig.mp}, 0);
    var prev: ?VirtualRead = null;
    defer if (prev) |p| gpa.free(p.bytes);
    var generations: u64 = 0;
    for (0..200) |_| {
        const r = try readVirtualWhole(gpa, path);
        errdefer gpa.free(r.bytes);
        try testing.expectEqual(r.size, @as(u64, r.bytes.len));
        // A torn or cut document fails the parse — a STRICT one: no key
        // repeats inside its object.
        expectStrictStats(gpa, r.bytes) catch |e| {
            std.debug.print("\n--- a torn .stats generation ({d} of {d} B): {t} ---\n{s}\n", .{ r.bytes.len, r.size, e, r.bytes[r.bytes.len -| 200..] });
            return e;
        };
        if (prev) |p| {
            if (p.ino != r.ino) generations += 1;
            gpa.free(p.bytes);
        }
        prev = r;
    }
    // Two sequential opens: two generations, each exact.
    const a = try readVirtualWhole(gpa, path);
    defer gpa.free(a.bytes);
    const b = try readVirtualWhole(gpa, path);
    defer gpa.free(b.bytes);
    try testing.expect(a.ino != b.ino);
    try testing.expect(!std.mem.eql(u8, a.bytes, b.bytes));
    try testing.expectEqual(a.size, @as(u64, a.bytes.len));
    try testing.expectEqual(b.size, @as(u64, b.bytes.len));
    const landed = try storm.finish(rig);
    std.debug.print("\nsmoke[virtual no-open]: 200 whole .stats documents across {d} generation changes while {d} creates landed\n", .{ generations, landed });
    try testing.expect(generations >= 199);
    try testing.expect(landed > 0);
    // 0400 under default_permissions refuses a non-root writer at open in
    // the kernel; root (this test) opens locally — no OPEN refuses it — and
    // every mutation is the daemon's EACCES: a truncating open's SETATTR, a
    // write.
    const rc = linux.open(path.ptr, .{ .ACCMODE = .WRONLY, .TRUNC = true, .CLOEXEC = true }, 0);
    if (linux.errno(rc) == .SUCCESS) _ = linux.close(@intCast(rc));
    try testing.expectEqual(linux.E.ACCES, linux.errno(rc));
    {
        const fd = try openRaw(path, .{ .ACCMODE = .WRONLY, .CLOEXEC = true });
        defer _ = linux.close(fd);
        try testing.expectEqual(linux.E.ACCES, linux.errno(linux.write(fd, "x", 1)));
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

/// The frozen-daemon row's request timeout (the seam): the kernel's floor
/// — it raises any shorter timeout to its check period.
const frozen_timeout_s: u64 = fuse_geometry.REQUEST_TIMEOUT_CHECK_S;
/// The kernel's request-timeout check period: a request is aborted at the
/// first check at or after its age reaches the timeout — at most one
/// period late.
const frozen_check_s: u64 = fuse_geometry.REQUEST_TIMEOUT_CHECK_S;
/// What the host-wide sync may take once our FUSE_SYNCFS is aborted: the
/// abort's wake and the superblocks the sync visits after ours, judged at
/// the kernel's own cadence (one more check period).
const frozen_after_abort_s: u64 = frozen_check_s;
/// The host's own writeback before the sync reaches the FUSE superblocks
/// (every dirty page of every filesystem on the venue — not this row's to
/// bound); past it the row fails rather than hang.
const frozen_writeback_wait_ns: u64 = 120 * std.time.ns_per_s;

/// The first STOPPED task on the host other than `except` (`/proc/<pid>/stat`
/// state `T`/`t`), or null: a host-wide sync waits on every FUSE server, so
/// another frozen one (another row's, another run's) would be what the
/// frozen-daemon row measures.
fn otherStoppedTask(io: Io, except: linux.pid_t) !?linux.pid_t {
    var proc_dir = try Io.Dir.cwd().openDir(io, "/proc", .{ .iterate = true });
    defer proc_dir.close(io);
    var it = proc_dir.iterate();
    while (try it.next(io)) |e| {
        const pid = std.fmt.parseInt(linux.pid_t, e.name, 10) catch continue;
        if (pid == except) continue;
        var pb: [64]u8 = undefined;
        var buf: [512]u8 = undefined;
        const text = Io.Dir.cwd().readFile(io, std.fmt.bufPrint(&pb, "/proc/{d}/stat", .{pid}) catch continue, &buf) catch continue;
        const close = std.mem.lastIndexOfScalar(u8, text, ')') orelse continue;
        if (close + 2 < text.len and (text[close + 2] == 'T' or text[close + 2] == 't')) return pid;
    }
    return null;
}

/// Does `/proc/<pid>/stack` name `needle` — a kernel function the task is
/// blocked in (root reads it)?
fn stackHas(pid: linux.pid_t, needle: []const u8) bool {
    var pb: [64]u8 = undefined;
    var buf: [4096]u8 = undefined;
    const path = std.fmt.bufPrint(&pb, "/proc/{d}/stack", .{pid}) catch return false;
    const text = Io.Dir.cwd().readFile(testing.io, path, &buf) catch return false;
    return std.mem.indexOf(u8, text, needle) != null;
}

// [venue-exclusive] (tests/shard_plan.zig): the row's host-wide `sync`
// visits every FUSE mount on the host and SIGSTOPs a daemon — beside another
// shard's rows it would flush their parked state mid-row and wait on their
// daemons, and another row's stopped task voids it (`otherStoppedTask`).
test "smoke: [venue-exclusive] A FROZEN DAEMON NEVER HANGS THE HOST'S `sync` (0003 §Z) — format → mount under the request-timeout seam (15 s) → SIGSTOP the daemon → a host-wide `sync` from another process: the kernel aborts its FUSE_SYNCFS to the frozen mount within the timeout + one check period and the sync returns → the frozen mount is fail-stopped (ENOTCONN, `sync -f` reports it, still in the mount table) → SIGCONT: the daemon names the abort loudly, tears down and exits 1 → a remount without the seam negotiates the lattice's T_owner + T_park_max" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(false);
    var seam_buf: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_FUSE_REQUEST_TIMEOUT_S", try std.fmt.bufPrint(&seam_buf, "{d}", .{frozen_timeout_s}));
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    // A kernel that never forwards syncfs leaves a frozen daemon out of the
    // host's sync altogether: nothing to judge.
    const negotiated_s = blk: {
        const st = try rig.stats();
        defer gpa.free(st);
        if (numberAfter(st, "\"fuse_syncfs_armed\":") != 1) {
            const rel = try rig.shOk(rig.work, "uname -r");
            defer gpa.free(rel);
            try rig.umount();
            _ = try rig.waitChild();
            var detail: [192]u8 = undefined;
            return testkit.skip(@src(), .no_kernel_feature, std.fmt.bufPrint(&detail, "kernel {s}: FUSE_HAS_SYNCFS not offered — the host's sync never reaches a plain /dev/fuse server", .{std.mem.trimEnd(u8, rel, "\n")}) catch "a kernel without FUSE_HAS_SYNCFS");
        }
        break :blk numberAfter(st, "\"transport_request_timeout_s\":");
    };
    {
        const out = try rig.onMount("echo frozen > f && sync -f . && echo written");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "written") != null);
    }
    // A host-wide sync waits on EVERY FUSE server: another frozen one would
    // be what this row measures.
    if (try otherStoppedTask(io, rig.child_pid)) |pid| {
        try rig.umount();
        _ = try rig.waitChild();
        var detail: [160]u8 = undefined;
        return testkit.skip(@src(), .external_interference, std.fmt.bufPrint(&detail, "task {d} is stopped on this host: a host-wide sync would wait on it, not on this row's frozen daemon", .{pid}) catch "another task is stopped on this host");
    }
    // Freeze: every thread of the daemon stops; nothing answers the kernel.
    _ = linux.kill(rig.child_pid, .STOP);
    var resumed = false;
    defer if (!resumed) {
        _ = linux.kill(rig.child_pid, .CONT);
    };
    // The host-wide sync from another process. Its FUSE_SYNCFS to the frozen
    // mount is created once the host's writeback is done: the bound runs
    // from the first instant the sync is seen waiting in `fuse_sync_fs`.
    var sync_child = try std.process.spawn(io, .{ .argv = &.{"sync"}, .stdin = .ignore, .stdout = .ignore, .stderr = .inherit });
    const sync_pid = sync_child.id.?;
    const spawned_at = clock.monotonicNs();
    var waiting_at: ?u64 = null;
    var returned_at: ?u64 = null;
    while (true) {
        const now = clock.monotonicNs();
        if (!alive(sync_pid)) {
            returned_at = now;
            break;
        }
        if (waiting_at == null and stackHas(sync_pid, "fuse_sync_fs")) waiting_at = now;
        const deadline = if (waiting_at) |w| w + (frozen_timeout_s + frozen_check_s + frozen_after_abort_s) * std.time.ns_per_s else spawned_at + frozen_writeback_wait_ns;
        if (now >= deadline) break;
        sleepMs(20);
    }
    if (returned_at == null) {
        // Only an answer frees a sync whose request a server was handed (the
        // wait is uninterruptible): resume ours, then fail — waiting on the
        // sync only as long as ours can hold it (a foreign frozen server
        // that froze after the census holds it longer; it is left behind).
        std.debug.print("\nsmoke[frozen]: the host-wide sync was still held {d} ms after it began waiting on the frozen mount (spawned {d} ms before that; waiting seen: {})\n", .{ (clock.monotonicNs() - (waiting_at orelse spawned_at)) / std.time.ns_per_ms, ((waiting_at orelse spawned_at) - spawned_at) / std.time.ns_per_ms, waiting_at != null });
        _ = linux.kill(rig.child_pid, .CONT);
        resumed = true;
        const freed_by = clock.monotonicNs() + frozen_after_abort_s * std.time.ns_per_s;
        while (alive(sync_pid) and clock.monotonicNs() < freed_by) sleepMs(20);
        if (alive(sync_pid)) {
            std.debug.print("smoke[frozen]: the sync still waits after the daemon resumed — on another server (left behind)\n", .{});
        } else _ = try sync_child.wait(io);
        return error.HostSyncHeld;
    }
    _ = try sync_child.wait(io);
    const waited_from = waiting_at orelse {
        std.debug.print("\nsmoke[frozen]: the sync returned without ever waiting on the frozen mount\n", .{});
        return error.SyncNeverReachedTheMount;
    };
    const held_ms = (returned_at.? - waited_from) / std.time.ns_per_ms;
    std.debug.print("\nsmoke[frozen]: the host-wide sync returned {d} ms after it began waiting on the frozen mount ({d} ms after its spawn; bound {d} s)\n", .{ held_ms, (returned_at.? - spawned_at) / std.time.ns_per_ms, frozen_timeout_s + frozen_check_s + frozen_after_abort_s });
    // The kernel never aborts a request younger than the timeout (one poll
    // of slack: the request was created just before it was seen waiting).
    try testing.expect(held_ms + 1000 >= frozen_timeout_s * 1000);
    // The frozen mount is fail-stopped by the kernel: every access answers
    // ENOTCONN — and it stays in the table, the failure visible.
    try testing.expectEqual(@as(usize, 1), try fuseMountsOf(gpa, io, rig.mp));
    {
        var pb: [linux.PATH_MAX]u8 = undefined;
        const p = try std.fmt.bufPrintSentinel(&pb, "{s}/.stats", .{rig.mp}, 0);
        const rc = linux.open(p.ptr, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(rc) == .SUCCESS) _ = linux.close(@intCast(rc));
        try testing.expectEqual(linux.E.NOTCONN, linux.errno(rc));
        var cmd: [linux.PATH_MAX + 16]u8 = undefined;
        const r = try rig.sh(rig.work, try std.fmt.bufPrint(&cmd, "sync -f {s}", .{rig.mp}));
        defer rig.freeRun(r);
        try testing.expect(!r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stderr, "Transport endpoint is not connected") != null);
    }
    // Resumed, the daemon finds its connection ended under its live mount:
    // it says so loudly, tears down and exits 1 (fail-stopped).
    _ = linux.kill(rig.child_pid, .CONT);
    resumed = true;
    try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, try rig.waitChild());
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "the kernel ended the FUSE connection under the live mount") != null);
    }
    _ = linux.umount2(rig.mp.ptr, linux.MNT.DETACH);
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    try testing.expectEqual(@as(?u64, frozen_timeout_s), negotiated_s);
    // Without the seam the INIT reply carries the lattice's own bound.
    _ = rig.env.swapRemove("SQUEEZEFS_TEST_FUSE_REQUEST_TIMEOUT_S");
    const mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    {
        const st = try rig.stats();
        defer gpa.free(st);
        const lattice_s = fuse_geometry.requestTimeoutS(lease_clock.T_OWNER_MS, membership.park_reclaim.tParkMaxMs(extent_grant.failoverBoundMs(0, 0), membership.park_reclaim.graceMs(lease_clock.T_OWNER_MS)));
        try testing.expectEqual(@as(?u64, lattice_s), numberAfter(st, "\"transport_request_timeout_s\":"));
        const out = try rig.onMount("cat f");
        defer gpa.free(out);
        try testing.expectEqualStrings("frozen\n", out);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

test "smoke: FSCK AND SCRUB — format (a cache path) → fill (dirs, 200 packed small files, hard links, three fsync'd 20 MiB files, a clone) → umount → `fsck --offline --scrub` 0 findings over every block → mount, 8-way storm, kill -9 → `fsck --offline` 0 findings → remount, clean umount → a planted stray reference is exactly [C3]+[C8] on its block (exit 1), `--repair` plans, `--repair --apply` releases it, clean → a planted orphan inode is exactly [C9], destroyed on apply, clean → `merge-reports` of two residue shards ≡ the whole run" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet(true);
    try rig.createImage();
    const storm_sources = try rig.shOk(rig.work, "i=0; while [ $i -lt 8 ]; do dd if=image of=s$i bs=1M skip=$((i * 25)) count=50 status=none; i=$((i + 1)); done; echo ok");
    gpa.free(storm_sources);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    // The fill in ONE mount: 20 staged 64 KiB files FIRST, then three
    // 20 MiB striped files with fsync (the holder's own grant window
    // carves 64 bits at the frontier; the cursor stands at 15), then a
    // tree of directories, 180 more packed 64 KiB files and hard links —
    // and the clean umount promotes every staged resident through the
    // LIVE grant plane. This exact shape (20 × 64 KiB + 3 × 20 MiB `dd
    // conv=fsync` → `umount`) asserted at HEAD before rung 8's fix in
    // `block_alloc.nextFreshBlock` (the dismount pass ran after the wire
    // plane had disarmed the allocator's mint source, so the fresh cursor
    // met the carved window's SET bit): the rung-8 dismount defect's
    // Linux pin.
    {
        const out = try rig.onMount("mkdir -p d/e/f g && i=0; while [ $i -lt 20 ]; do dd if=\"$1/image\" of=d/e/small$i bs=64k skip=$i count=1 status=none; i=$((i + 1)); done; for n in 0 1 2; do dd if=\"$1/image\" of=big$n bs=1M skip=$((n * 20)) count=20 conv=fsync status=none; done; while [ $i -lt 200 ]; do dd if=\"$1/image\" of=d/e/small$i bs=64k skip=$i count=1 status=none; i=$((i + 1)); done; ln d/e/small1 g/link1; ln d/e/small2 g/link2; sync -f .; echo filled");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "filled") != null);
    }
    try rig.expectAlive("the staged + striped + packed fill and hard links");
    const mark_dismount = try rig.logLen();
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    {
        // Every staged resident promoted at the dismount, none failed, and
        // the allocator's anomaly tripwire never fired.
        const text = try rig.readLog();
        defer gpa.free(text);
        const tail = text[mark_dismount..];
        const line_at = std.mem.indexOf(u8, tail, "dismount promotion: ") orelse {
            std.debug.print("\n--- no dismount promotion line ---\n{s}\n", .{tail});
            return error.NoDismountPromotionLine;
        };
        const line = tail[line_at..];
        const promoted = numberAfter(line, "dismount promotion: ") orelse return error.NoDismountPromotionLine;
        try testing.expect(promoted >= 1);
        try testing.expect(std.mem.indexOf(u8, line[0..@min(line.len, 200)], ", 0 failed") != null);
        try testing.expect(std.mem.indexOf(u8, tail, "block_claim_anomalies") == null);
        std.debug.print("\nsmoke[fsck]: the dismount promoted {d} staged file(s) beside fresh striped blocks (the rung-8 defect's pin)\n", .{promoted});
    }
    // The clone (an offline verb: the same guarded open fsck takes) shares
    // big0's blocks — the refcount-2 shape C3/C8 must read as healthy.
    {
        const r = try rig.squeezefs(&.{ "clone", rig.meta_uri, "/big0", "/big0-clone" });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nclone: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }
    // 1. The healthy set: 0 findings, every block read (a plain set: the
    // honesty gauge says readability-only — no frame to verify).
    {
        var got = try fsckJson(rig, &.{"--scrub"});
        defer got.report.deinit(gpa);
        if (got.report.findings.len != 0) for (got.report.findings) |f| std.debug.print("  unexpected: {f}\n", .{f});
        try testing.expectEqual(@as(usize, 0), got.report.findings.len);
        try testing.expectEqual(@as(u8, 0), got.code);
        const c = got.report.counters;
        std.debug.print("\nsmoke[fsck]: healthy: {d} inodes, {d} dentry refs, {d} blocks, {d} refcounts; scrub {d} blocks (readability-only {d}) in {d} ms\n", .{ c.inodes_scanned, c.dentry_refs_indexed, c.blocks_checked, c.refcounts_checked, c.scrub_blocks_scanned, c.scrub_readability_only, c.scan_ms });
        // 4 directories + 200 packed files + 3 striped files + the clone,
        // beside the root; 2 hard links add names, never inodes.
        try testing.expect(c.inodes_scanned >= 208);
        try testing.expect(c.dentry_refs_indexed >= 210);
        // 3 × 20 MiB striped files = 15 blocks of 4 MiB + the packed blocks.
        try testing.expect(c.blocks_checked >= 16);
        try testing.expect(c.scrub_blocks_scanned >= 16);
        try testing.expectEqual(c.scrub_blocks_scanned, c.scrub_readability_only);
        try testing.expectEqual(@as(u64, 0), c.scrub_failures);
    }
    // 2. The storm kill → fsck clean → remount → clean umount.
    const mark_storm = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark_storm);
    {
        var storm_cmd: [1024]u8 = undefined;
        const cmd = try std.fmt.bufPrint(&storm_cmd, "i=0; while [ $i -lt 8 ]; do cp {s}/s$i {s}/storm$i & i=$((i + 1)); done; wait", .{ rig.work, rig.mp });
        var storm = try std.process.spawn(io, .{ .argv = &.{ "sh", "-c", cmd }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .ignore });
        var pb: [256:0]u8 = undefined;
        var claimed: u64 = 0;
        const deadline = clock.monotonicNs() + 60 * std.time.ns_per_s;
        while (clock.monotonicNs() < deadline) {
            claimed = 0;
            for (0..storm_files) |i| {
                const sp = try std.fmt.bufPrintSentinel(&pb, "{s}/storm{d}", .{ rig.mp, i }, 0);
                claimed += fileSize(sp);
            }
            if (claimed >= storm_kill_mib << 20) break;
            sleepMs(1);
        }
        try testing.expect(claimed >= storm_kill_mib << 20);
        _ = linux.kill(rig.child_pid, .KILL);
        try testing.expectEqual(std.process.Child.Term{ .signal = .KILL }, try rig.waitChild());
        _ = try storm.wait(io);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(rig.mp.ptr, 0)));
    }
    _ = try fsckClean(rig, "the storm kill (fsck smoke)");
    const mark_after_kill = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark_after_kill);
    {
        const out = try rig.onMount("rm -f storm* && echo ok");
        defer gpa.free(out);
    }
    try rig.expectAlive("the storm residue removal");
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the clean unmount (fsck smoke)");
    // 3. The planted defects: the exact classes, the plan, the apply, clean.
    // The planting run opens the set, binds the plane (the allocator seeded
    // from the durable ledger), plants, then judges: it sees the ledger's
    // drift. The reopened dry run sees every class the defect produces: a
    // stale durable reference is the ledger's drift [C8] AND the refcount the
    // next open seeds from it [C3]; a dropped dentry is [C9] alone.
    const Planted = struct { arm: []const u8, classes: []const json_scan_report.Class, verb: []const u8 };
    for ([_]Planted{
        .{ .arm = "stray-ref", .classes = &.{ .c3, .c8 }, .verb = "release_stale_block_refs" },
        .{ .arm = "orphan-inode", .classes = &.{.c9}, .verb = "destroy_unreferenced_inode" },
    }) |pl| {
        try rig.env.put("SQUEEZEFS_TEST_FSCK_INJECT", pl.arm);
        var planted = try fsckJson(rig, &.{"--repair"});
        defer planted.report.deinit(gpa);
        _ = rig.env.swapRemove("SQUEEZEFS_TEST_FSCK_INJECT");
        try testing.expectEqual(@as(u8, 1), planted.code);
        try testing.expect(planted.report.findings.len >= 1);
        try testing.expectEqual(pl.classes[pl.classes.len - 1], planted.report.findings[planted.report.findings.len - 1].class());
        std.debug.print("smoke[fsck]: planted {s} → {f}\n", .{ pl.arm, planted.report.findings[planted.report.findings.len - 1] });
        // The dry run planned the verb and mutated nothing: the finding
        // stands, now with every class the reopened set derives from it.
        var again = try fsckJson(rig, &.{});
        defer again.report.deinit(gpa);
        if (again.report.findings.len != pl.classes.len) for (again.report.findings) |f| std.debug.print("  {s}: {f}\n", .{ pl.arm, f });
        try testing.expectEqual(pl.classes.len, again.report.findings.len);
        for (pl.classes, again.report.findings) |want, f| try testing.expectEqual(want, f.class());
        try testing.expectEqual(@as(u8, 1), again.code);
        // The apply (every finding's verb), then clean.
        {
            const r = try rig.squeezefs(&.{ "fsck", "--offline", "--repair", "--apply", rig.meta_uri });
            defer rig.freeRun(r);
            var want_buf: [96]u8 = undefined;
            const want = try std.fmt.bufPrint(&want_buf, "repair (--apply): 0 planned, {d} applied, 0 refused", .{pl.classes.len});
            if (std.mem.indexOf(u8, r.stdout, want) == null or r.term != .exited) std.debug.print("\n--- apply ({f}) ---\n{s}\n{s}\n", .{ r.term, r.stdout, r.stderr });
            try testing.expect(std.mem.indexOf(u8, r.stdout, want) != null);
            try testing.expect(std.mem.indexOf(u8, r.stdout, pl.verb) != null);
            try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        }
        _ = try fsckClean(rig, pl.arm);
    }
    // 4. Two residue shards merge to the whole report.
    {
        var shard0 = try fsckJson(rig, &.{ "--shards", "0/2" });
        defer shard0.report.deinit(gpa);
        var shard1 = try fsckJson(rig, &.{ "--shards", "1/2" });
        defer shard1.report.deinit(gpa);
        var whole = try fsckJson(rig, &.{});
        defer whole.report.deinit(gpa);
        try testing.expectEqual(whole.report.counters.inodes_scanned, shard0.report.counters.inodes_scanned + shard1.report.counters.inodes_scanned);
        try testing.expect(shard0.report.shard != null and shard1.report.shard != null);
        for ([_]*json_scan_report.Report{ &shard0.report, &shard1.report }, [_][]const u8{ "shard0.json", "shard1.json" }) |rep, name| {
            var out: std.Io.Writer.Allocating = .init(gpa);
            defer out.deinit();
            try rep.writeJson(&out.writer);
            var pbuf: [256]u8 = undefined;
            const path = try std.fmt.bufPrint(&pbuf, "{s}/{s}", .{ rig.work, name });
            const f = try Io.Dir.cwd().createFile(io, path, .{ .truncate = true });
            defer f.close(io);
            try f.writeStreamingAll(io, out.written());
        }
        var p0: [256]u8 = undefined;
        var p1: [256]u8 = undefined;
        const r = try rig.squeezefs(&.{ "fsck", "merge-reports", "--json", try std.fmt.bufPrint(&p0, "{s}/shard0.json", .{rig.work}), try std.fmt.bufPrint(&p1, "{s}/shard1.json", .{rig.work}) });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, r.term);
        var merged = try json_scan_report.Report.parse(gpa, r.stdout);
        defer merged.deinit(gpa);
        try testing.expectEqual(@as(usize, 0), merged.findings.len);
        try testing.expectEqual(whole.report.counters.inodes_scanned, merged.counters.inodes_scanned);
        try testing.expect(merged.shard == null);
    }
}

// ---- the job fabric's Linux matrix (rung 8) ----

/// The matrix's data volumes: 4 GiB SPARSE each (ext4 on /build charges
/// only what is written — ≈ 1.8 GiB across the row), so the drain's
/// capacity closed form (`headroom ≥ 1 GiB`, `transient = 64 × workers ×
/// block`) admits a 1.5 GiB file's evacuation onto two survivors; at the
/// smoke's 2 GiB volumes the law refuses by design.
const matrix_vol_len: u64 = 4 << 30;

/// One live job's state through `job status <mountpoint>` (null = not in
/// the live table).
fn liveJobState(rig: *Rig, job_id: []const u8) !?[]const u8 {
    const r = try rig.squeezefs(&.{ "job", "status", rig.mp, job_id });
    defer rig.freeRun(r);
    if (!r.term.success()) return null;
    // `job-…: <type> <state>, …`
    const colon = std.mem.indexOf(u8, r.stdout, ": ") orelse return null;
    var it = std.mem.tokenizeAny(u8, r.stdout[colon + 2 ..], " ,");
    _ = it.next() orelse return null;
    const state = it.next() orelse return null;
    inline for (@typeInfo(jobs_records.JobState).@"enum".field_names) |name| {
        if (std.mem.eql(u8, state, name)) return name;
    }
    return null;
}

const jobs_records = @import("../ops/jobs/records.zig");
const rebalance_band_ppm = @import("../ops/jobs/rebalance.zig").REBALANCE_BAND_PPM;

/// Poll until the job is terminal (bounded); the final state.
fn awaitJobTerminal(rig: *Rig, job_id: []const u8, bound_ns: u64) ![]const u8 {
    const deadline = clock.monotonicNs() + bound_ns;
    while (true) {
        const st = (try liveJobState(rig, job_id)) orelse return error.JobVanished;
        if (std.mem.eql(u8, st, "completed") or std.mem.eql(u8, st, "cancelled") or std.mem.eql(u8, st, "failed")) return st;
        if (clock.monotonicNs() >= deadline) {
            std.debug.print("\n--- {s} still {s} after the bound ---\n", .{ job_id, st });
            return error.JobNotTerminal;
        }
        sleepMs(200);
    }
}

/// Poll until the job reads `want` (bounded).
fn awaitJobState(rig: *Rig, job_id: []const u8, want: []const u8, bound_ns: u64) !void {
    const deadline = clock.monotonicNs() + bound_ns;
    while (clock.monotonicNs() < deadline) {
        const st = (try liveJobState(rig, job_id)) orelse return error.JobVanished;
        if (std.mem.eql(u8, st, want)) return;
        sleepMs(50);
    }
    std.debug.print("\n--- {s} never read {s} ---\n", .{ job_id, want });
    return error.JobStateNotReached;
}

/// The `job-{16 hex}` word out of a verb's stdout (the first one).
fn jobIdIn(text: []const u8) ?[]const u8 {
    const i = std.mem.indexOf(u8, text, "job-") orelse return null;
    if (text.len < i + jobs_records.JOB_ID_TEXT_LEN) return null;
    const word = text[i .. i + jobs_records.JOB_ID_TEXT_LEN];
    return if (jobs_records.parseJobId(word) != null) word else null;
}

/// The `vol-{16 hex}` of the data volume whose record names `path`
/// (`status`'s row), or null.
fn volumeIdOfPath(rig: *Rig, path: []const u8) !?[]u8 {
    const report = try rig.statusReport();
    defer rig.gpa.free(report);
    var lines = std.mem.splitScalar(u8, report, '\n');
    while (lines.next()) |line| {
        if (std.mem.indexOf(u8, line, path) == null) continue;
        const i = std.mem.indexOf(u8, line, "vol-") orelse continue;
        if (line.len < i + 20) continue;
        return try rig.gpa.dupe(u8, line[i .. i + 20]);
    }
    return null;
}

test "smoke: THE JOBS MATRIX — format (1 MiB blocks, a cache path) → fill (packed, striped, a 1.5 GiB block-map file) → umount, clone, remount → `volume add-data <mp>` ONLINE (the new volume live, the rebalance on the fabric, `backend_fill_spread` ≤ 10 pp) → `volume remove-data <mp> d0` → `job pause/resume/throttle` live → the drain retires d0 (every block moved once; df within the window) → `undrain` refused after retire → `defrag --pack <mp>` on a half-empty pack → `remove-data d1` at 25 % → kill -9 mid-drain → remount adopts and completes it → `fsck --online` 0 findings → a second daemon enrolls as a REMOTE WORKER over TCP and runs a job → `fsck --offline` clean" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    var bs_buf: [16]u8 = undefined;
    // The matrix's volumes are wider than the smoke's (sparse; see
    // `matrix_vol_len`): sized before the ONE format reads the device
    // lengths into the superblocks.
    try rig.createMetaVolume();
    const d2 = try std.fmt.allocPrintSentinel(gpa, "{s}/d2.img", .{rig.work}, 0);
    defer gpa.free(d2);
    for ([_][:0]const u8{ rig.d0, rig.d1, d2 }) |dp| {
        const f = try Io.Dir.cwd().createFile(io, dp, .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, matrix_vol_len);
    }
    _ = linux.mkdir(rig.cache.ptr, 0o700);
    {
        const r = try rig.squeezefs(&.{ "format", "--block-size", try std.fmt.bufPrint(&bs_buf, "{d}", .{kvmap_block}), "--disk-cache-paths", rig.cache, rig.meta_uri, rig.data_uri });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }
    try rig.createImage();
    try rig.env.put("SQUEEZEFS_BLOCK_REFS_VERIFY", "1");

    // 1. The fill: 80 packed 64 KiB files (5 pack blocks at 1 MiB — enough
    //    that after the drain spreads them over two volumes one volume
    //    holds ≥ 3 victims: a LONE half-empty pack frees nothing and is
    //    not a victim), three fsync'd 20 MiB striped files, the 1.5 GiB
    //    block-map file; umount (the staged files promote into packs), the
    //    offline clone (a shared block), remount.
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    {
        const out = try rig.onMount("mkdir -p p && i=0; while [ $i -lt 80 ]; do dd if=\"$1/image\" of=p/small$i bs=64k skip=$i count=1 status=none; i=$((i + 1)); done; for n in 0 1 2; do dd if=\"$1/image\" of=big$n bs=1M skip=$((n * 20)) count=20 conv=fsync status=none; done; echo filled");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "filled") != null);
    }
    {
        const script = try kvmapWriteScript(gpa, 0, kvmap_file_mib / image_mib, false);
        defer gpa.free(script);
        const out = try rig.onMount(script);
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "kvmap-write-ok") != null);
    }
    {
        const out = try rig.onMount("sha256sum big big0 big1 big2 p/small* | sort -k2 > \"$1/before.sha\"; echo ok");
        defer gpa.free(out);
    }
    try rig.expectAlive("the matrix fill");
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    {
        const r = try rig.squeezefs(&.{ "clone", rig.meta_uri, "/big0", "/big0-clone" });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
    }
    const v0 = (try volumeIdOfPath(rig, rig.d0)) orelse return error.NoVolumeId;
    defer gpa.free(v0);
    const v1 = (try volumeIdOfPath(rig, rig.d1)) orelse return error.NoVolumeId;
    defer gpa.free(v1);
    var mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    const used_before = try rig.usedKib();

    // 2. ONLINE add-data: the device formatted + recorded + attached + armed
    //    in one verb; the rebalance runs on the fabric; every volume ends
    //    within 10 pp of the mean fill (the planner raises the under-filled
    //    rows to the band's floor, `mean − 10 pp`, and never pushes a
    //    source below the mean), so `backend_fill_spread` = max − min ≤
    //    2 × the band once the cadence refreshed the census.
    var rebalance_id_buf: [jobs_records.JOB_ID_TEXT_LEN]u8 = undefined;
    {
        const r = try rig.squeezefs(&.{ "volume", "add-data", rig.mp, d2 });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nadd-data: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "live") != null);
        const id = jobIdIn(r.stdout) orelse return error.NoRebalanceJob;
        @memcpy(&rebalance_id_buf, id);
    }
    const rebalance_id: []const u8 = &rebalance_id_buf;
    try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, rebalance_id, 600 * std.time.ns_per_s));
    {
        // The cadence refreshes the census every watchdog tick (5 s).
        const deadline = clock.monotonicNs() + 20 * std.time.ns_per_s;
        var spread: u64 = std.math.maxInt(u64);
        while (clock.monotonicNs() < deadline) {
            const st = try rig.stats();
            defer gpa.free(st);
            spread = numberAfter(st, "\"backend_fill_spread\":") orelse spread;
            if (spread <= 2 * rebalance_band_ppm) break;
            sleepMs(500);
        }
        std.debug.print("\nsmoke[jobs]: add-data live; the rebalance completed; backend_fill_spread {d} ppm (every volume within the {d} ppm band of the mean)\n", .{ spread, rebalance_band_ppm });
        try testing.expect(spread <= 2 * rebalance_band_ppm);
    }
    {
        const r = try rig.squeezefs(&.{ "job", "list", rig.mp });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "rebalance") != null);
    }
    // A terminal job refuses a control by verdict (exit 1, named).
    {
        const r = try rig.squeezefs(&.{ "job", "throttle", rig.mp, rebalance_id, "50" });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "terminal") != null);
    }

    // 3. remove-data d0, throttled to 25 % live: pause (observable),
    //    resume, throttle back to 100; the drain retires d0; undrain after
    //    retire refuses.
    var drain_id_buf: [jobs_records.JOB_ID_TEXT_LEN]u8 = undefined;
    {
        const r = try rig.squeezefs(&.{ "volume", "remove-data", rig.mp, v0 });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nremove-data: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "Draining") != null);
        const id = jobIdIn(r.stdout) orelse return error.NoDrainJob;
        @memcpy(&drain_id_buf, id);
    }
    const drain_id: []const u8 = &drain_id_buf;
    {
        const t = try rig.squeezefs(&.{ "job", "throttle", rig.mp, drain_id, "25" });
        defer rig.freeRun(t);
        try testing.expect(t.term.success());
        const r = try rig.squeezefs(&.{ "job", "pause", rig.mp, drain_id });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
    }
    try awaitJobState(rig, drain_id, "paused", 30 * std.time.ns_per_s);
    {
        const r = try rig.squeezefs(&.{ "job", "resume", rig.mp, drain_id });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        const t = try rig.squeezefs(&.{ "job", "throttle", rig.mp, drain_id, "100" });
        defer rig.freeRun(t);
        // The resumed drain may retire its last blocks before the throttle
        // lands (a 25 % throttle over a tail of a few blocks on a quiet
        // venue): a terminal job refuses control by design — that verdict
        // is the lane's, not a failure of the row.
        if (!t.term.success()) try testing.expect(std.mem.indexOf(u8, t.stderr, "the job is terminal") != null);
    }
    try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, drain_id, 900 * std.time.ns_per_s));
    {
        const r = try rig.squeezefs(&.{ "volume", "undrain", rig.mp, v0 });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "refused") != null);
    }
    {
        const st = try rig.stats();
        defer gpa.free(st);
        const moved = numberAfter(st, "\"evacuate_blocks_moved\":") orelse 0;
        const retired = numberAfter(st, "\"volume_retired\":") orelse 0;
        // `df`: the moved blocks are counted on their destinations; the
        // window's slack is at most one grant floor per volume. Polled: the
        // retire's terminal frees drain through the reclaimer after the job
        // reads completed (its park bound is 1 s).
        const slack_kib = 2 * 64 * (kvmap_block >> 10);
        var used_after = try rig.usedKib();
        const df_deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
        while ((used_after + slack_kib < used_before or used_after > used_before + slack_kib) and clock.monotonicNs() < df_deadline) {
            sleepMs(200);
            used_after = try rig.usedKib();
        }
        std.debug.print("smoke[jobs]: drain of {s}: {d} block(s) moved, volume_retired {d}; df used {d} → {d} KiB (slack ≤ {d})\n", .{ v0, moved, retired, used_before, used_after, slack_kib });
        try testing.expect(moved >= 1);
        try testing.expectEqual(@as(u64, 1), retired);
        if (used_after + slack_kib < used_before or used_after > used_before + slack_kib) {
            if (std.mem.indexOf(u8, st, "\"block_grant_holder\":")) |i| std.debug.print("    {s}\n", .{st[i..@min(st.len, i + 700)]});
            return error.DfBaselineBroken;
        }
    }

    // 4. `defrag --pack` on half-empty packs: 3 of every 4 packed files
    //    unlinked (every pack ≤ ¼ live), the compactor copies the live
    //    windows once into fewer blocks; the kept files stay byte-exact.
    {
        const out = try rig.onMount("i=0; while [ $i -lt 80 ]; do if [ $((i % 4)) -ne 0 ]; then rm p/small$i; fi; i=$((i + 1)); done; sync -f .; echo rm-ok");
        defer gpa.free(out);
        // The unlinked files are corpses until the reclaim cadence destroys
        // them (≤ one watchdog tick, 5 s): the census names their tenants
        // until then, so the compactor is given the destroyed shape.
        const deadline = clock.monotonicNs() + 30 * std.time.ns_per_s;
        var destroyed: u64 = 0;
        while (clock.monotonicNs() < deadline) {
            const st = try rig.stats();
            defer gpa.free(st);
            destroyed = numberAfter(st, "\"fuse_reclaim_destroys\":") orelse 0;
            if (destroyed >= 60) break;
            sleepMs(250);
        }
        try testing.expect(destroyed >= 60);
    }
    {
        const r = try rig.squeezefs(&.{ "defrag", "--pack", rig.mp });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\ndefrag --pack: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        const id = jobIdIn(r.stdout) orelse return error.NoPackJob;
        try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, id, 300 * std.time.ns_per_s));
    }
    {
        const st = try rig.stats();
        defer gpa.free(st);
        const compactions = numberAfter(st, "\"pack_compactions\":") orelse 0;
        std.debug.print("smoke[jobs]: defrag --pack: {d} pack compaction(s)\n", .{compactions});
        try testing.expect(compactions >= 1);
        const out = try rig.onMount("i=0; while [ $i -lt 80 ]; do grep \"p/small$i\\$\" \"$1/before.sha\" | sha256sum -c --quiet - ; i=$((i + 4)); done; echo pack-exact");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "pack-exact") != null);
    }

    // 5. remove-data d1 at 25 %, kill -9 mid-drain → remount: the adopted
    //    job regenerates its plan and completes; d1 retires.
    var drain2_buf: [jobs_records.JOB_ID_TEXT_LEN]u8 = undefined;
    {
        const r = try rig.squeezefs(&.{ "volume", "remove-data", rig.mp, v1 });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nremove-data d1: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        @memcpy(&drain2_buf, jobIdIn(r.stdout) orelse return error.NoDrainJob);
    }
    const drain2: []const u8 = &drain2_buf;
    {
        const t = try rig.squeezefs(&.{ "job", "throttle", rig.mp, drain2, "25" });
        defer rig.freeRun(t);
        try testing.expect(t.term.success());
    }
    {
        // Mid-drain: at least one task done, the job still running.
        const deadline = clock.monotonicNs() + 120 * std.time.ns_per_s;
        var mid = false;
        while (clock.monotonicNs() < deadline) {
            const r = try rig.squeezefs(&.{ "job", "status", rig.mp, drain2 });
            defer rig.freeRun(r);
            if (std.mem.indexOf(u8, r.stdout, " running, ")) |_| {
                if (std.mem.indexOf(u8, r.stdout, ", 0/")) |_| {} else {
                    mid = true;
                    break;
                }
            }
            sleepMs(100);
        }
        try testing.expect(mid);
    }
    rig.killed = true;
    _ = linux.kill(rig.child_pid, .KILL);
    try testing.expectEqual(std.process.Child.Term{ .signal = .KILL }, try rig.waitChild());
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(rig.mp.ptr, 0)));
    mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[mark..], "job(s) adopted") != null);
    }
    try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, drain2, 900 * std.time.ns_per_s));
    {
        const st = try rig.stats();
        defer gpa.free(st);
        std.debug.print("smoke[jobs]: kill -9 mid-drain → remount: {s} resumed from its regenerated plan and completed; volume_retired {d}\n", .{ drain2, numberAfter(st, "\"volume_retired\":") orelse 0 });
        try testing.expectEqual(@as(u64, 1), numberAfter(st, "\"volume_retired\":") orelse 0);
    }
    // 6. `fsck --online` on the live mount: 0 findings.
    {
        var got = try fsckJsonTarget(rig, rig.mp, &.{"--online"});
        defer got.report.deinit(gpa);
        if (got.report.findings.len != 0) for (got.report.findings) |f| std.debug.print("  unexpected: {f}\n", .{f});
        try testing.expectEqual(@as(usize, 0), got.report.findings.len);
        try testing.expectEqual(@as(u8, 0), got.code);
        std.debug.print("smoke[jobs]: fsck --online: {d} inode(s), {d} block(s), 0 findings\n", .{ got.report.counters.inodes_scanned, got.report.counters.blocks_checked });
    }
    // Every file byte-exact after two drains and a compaction.
    {
        const out = try rig.onMount("sha256sum big big0 big1 big2 p/small* | sort -k2 > \"$1/after.sha\"; { grep ' big[0-2]*$' \"$1/before.sha\"; for i in $(seq 0 4 76); do grep \" p/small$i\\$\" \"$1/before.sha\"; done; } | sort -k2 | cmp - \"$1/after.sha\"; cmp big0 big0-clone; echo all-exact");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "all-exact") != null);
    }

    // 7. A REMOTE WORKER: a second process enrolls over TCP (`job worker`),
    //    pulls a wire-executable job the admin lane submits, completes it
    //    remotely (`job_remote_workers` 1 while enrolled).
    {
        var worker = try std.process.spawn(io, .{ .argv = &.{ rig.bin, "job", "worker", rig.meta_uri }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .{ .file = rig.log_file.? }, .stderr = .{ .file = rig.log_file.? } });
        var enrolled = false;
        const deadline = clock.monotonicNs() + 30 * std.time.ns_per_s;
        while (clock.monotonicNs() < deadline) {
            const st = try rig.stats();
            defer gpa.free(st);
            if ((numberAfter(st, "\"job_remote_workers\":") orelse 0) == 1) {
                enrolled = true;
                break;
            }
            sleepMs(100);
        }
        try testing.expect(enrolled);
        const r = try rig.squeezefs(&.{ "defrag", "--rebalance", rig.mp });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        const id = jobIdIn(r.stdout) orelse return error.NoRebalanceJob;
        try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, id, 300 * std.time.ns_per_s));
        _ = linux.kill(worker.id.?, .TERM);
        try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try worker.wait(io));
        const st = try rig.stats();
        defer gpa.free(st);
        std.debug.print("smoke[jobs]: the remote worker enrolled over TCP (job_remote_enrollments {d}); after its leave job_remote_workers {d}\n", .{ numberAfter(st, "\"job_remote_enrollments\":") orelse 0, numberAfter(st, "\"job_remote_workers\":") orelse 99 });
        try testing.expect((numberAfter(st, "\"job_remote_enrollments\":") orelse 0) >= 1);
        try testing.expectEqual(@as(u64, 0), numberAfter(st, "\"job_remote_workers\":") orelse 99);
    }
    try rig.expectAlive("the matrix");
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the jobs matrix");
}

// ---- defrag --fold / --meta on a live mount ----

/// The D3 row's shape: three striped files, four sector-aligned 512 B
/// writes each into scattered pages of their first blocks, held OPEN
/// across the job (a RELEASE folds a lone parked write on its own).
const fold_files: usize = 3;
const fold_writes_per_file: usize = 4;
const fold_page: u64 = 4096;

/// The D3 row's holder: a forked child that opens `big0..big2`, writes the
/// 12 parked sectors and holds the fds until it is killed. It never forks
/// or execs, so no copy of its fds closes before it dies — under no-open
/// (decision 0003 §AF) the close of ANY copy is a FLUSH, the drain, and a
/// test process holding the fds while it spawns its probes (`cat .stats`,
/// `squeezefs defrag`: each child's exec closes its CLOEXEC copy) would
/// drain the extents the row exists to fold. The child runs raw syscalls
/// only (a forked copy of a threaded process allocates nothing).
const FoldHolder = struct {
    pid: linux.pid_t,
    stopped: bool = false,

    fn start(mp: []const u8) !FoldHolder {
        var paths: [fold_files][linux.PATH_MAX:0]u8 = undefined;
        for (0..fold_files) |n| _ = try std.fmt.bufPrintSentinel(&paths[n], "{s}/big{d}", .{ mp, n }, 0);
        var ready: [2]i32 = undefined;
        if (linux.errno(linux.pipe2(&ready, .{ .CLOEXEC = true })) != .SUCCESS) return error.PipeFailed;
        defer _ = linux.close(ready[0]);
        const rc = linux.fork();
        if (linux.errno(rc) != .SUCCESS) {
            _ = linux.close(ready[1]);
            return error.ForkFailed;
        }
        if (rc == 0) childMain(&paths, ready[1]);
        _ = linux.close(ready[1]);
        var verdict: [1]u8 = .{0};
        const n = linux.read(ready[0], &verdict, 1);
        const self: FoldHolder = .{ .pid = @intCast(rc) };
        if (linux.errno(n) != .SUCCESS or n != 1 or verdict[0] != 'r') {
            var h = self;
            h.stop();
            return error.HolderFailed;
        }
        return self;
    }

    fn childMain(paths: *const [fold_files][linux.PATH_MAX:0]u8, ready: i32) noreturn {
        var sector: [512]u8 = undefined;
        for (0..fold_files) |n| {
            const rc = linux.open(&paths[n], .{ .ACCMODE = .RDWR }, 0);
            if (linux.errno(rc) != .SUCCESS) fail(ready);
            const fd: linux.fd_t = @intCast(rc);
            for (0..fold_writes_per_file) |k| {
                @memset(&sector, @as(u8, @intCast(0x40 + n * 8 + k)));
                // Block k, page 3 + 2k, 512 B into the page: a sub-cap,
                // sector-aligned, unaligned-to-the-page write — the extent class.
                const off: u64 = @as(u64, k) * kvmap_block + (3 + 2 * @as(u64, k)) * fold_page + 512;
                if (linux.pwrite(fd, &sector, sector.len, @intCast(off)) != sector.len) fail(ready);
            }
        }
        _ = linux.write(ready, "r", 1);
        while (true) _ = linux.pause();
    }

    fn fail(ready: i32) noreturn {
        _ = linux.write(ready, "e", 1);
        linux.exit_group(1);
    }

    /// Kill and reap (idempotent).
    fn stop(self: *FoldHolder) void {
        if (self.stopped) return;
        self.stopped = true;
        _ = linux.kill(self.pid, .KILL);
        var status: i32 = 0;
        _ = linux.waitpid(self.pid, &status, 0);
    }
};

/// The D4 row's forest: `defrag_forest_files` files created, every tenth
/// kept, the survivors rewritten `defrag_rewrite_rounds` times — the
/// leaves then hold a few live records under a log that is mostly dead
/// puts (compaction candidates) and most leaves fold under ¼ (merge
/// candidates) at the 64 KiB node the row formats.
const defrag_forest_files: usize = 8000;
const defrag_rewrite_rounds: usize = 4;

fn defragForestScript(gpa: std.mem.Allocator) ![]u8 {
    var w: std.Io.Writer.Allocating = .init(gpa);
    defer w.deinit();
    // Four parallel lanes build the population; the rewrites follow.
    try w.writer.print("mkdir -p m; for lane in 0 1 2 3; do ( i=$lane; while [ $i -lt {d} ]; do printf 'm%s\\n' $i > m/f$i; i=$((i + 4)); done ) & done; wait; i=0; while [ $i -lt {d} ]; do if [ $((i % 10)) -ne 0 ]; then rm m/f$i; fi; i=$((i + 1)); done; r=0; while [ $r -lt {d} ]; do i=0; while [ $i -lt {d} ]; do printf 'm%s-r%s\\n' $i $r > m/f$i; i=$((i + 10)); done; r=$((r + 1)); done; ls m | wc -l", .{ defrag_forest_files, defrag_forest_files, defrag_rewrite_rounds, defrag_forest_files });
    return w.toOwnedSlice();
}

/// The whole `.stats` once the meta heap has no retirement awaiting its
/// barrier (`heap_pending_free` 0 — the compactions' and merges' old
/// extents return at the barriered cycles that follow), bounded; the
/// last read either way.
fn heapSettledStats(rig: *Rig) ![]u8 {
    const deadline = clock.monotonicNs() + 30 * std.time.ns_per_s;
    while (true) {
        const st = try rig.stats();
        if ((numberAfter(st, "\"heap_pending_free\":") orelse 0) == 0 or clock.monotonicNs() >= deadline) return st;
        rig.gpa.free(st);
        sleepMs(200);
    }
}

/// A bounded poll until the job reads `running` with ≥ 1 task done (the
/// kill row's "mid-job").
fn awaitJobMidRun(rig: *Rig, job_id: []const u8, bound_ns: u64) !void {
    const deadline = clock.monotonicNs() + bound_ns;
    while (clock.monotonicNs() < deadline) {
        const r = try rig.squeezefs(&.{ "job", "status", rig.mp, job_id });
        defer rig.freeRun(r);
        if (std.mem.indexOf(u8, r.stdout, " running, ") != null and std.mem.indexOf(u8, r.stdout, ", 0/") == null) return;
        if (std.mem.indexOf(u8, r.stdout, " completed") != null) return error.JobCompletedEarly;
        sleepMs(50);
    }
    return error.JobNeverMidRun;
}

test "smoke: DEFRAG --fold AND --meta ON A LIVE MOUNT — format (64 KiB meta nodes, 1 MiB blocks, a cache path) → three fsync'd striped files held OPEN with 12 sector-aligned 512 B writes parked as W2 extents (parked_extent_bytes ≥ 12 pages) → `defrag --fold <mp>` folds every one in place (12 page DMAs for 6 KiB of user bytes ≤ the §J 8× bound, parked_extent_bytes 0, every byte exact after the close) → a forest of 8,000 files cut to 800 and rewritten 4× → `defrag --meta --throttle 1` runs under the duty cycle: `job cancel` mid-run → cancelled; a second at throttle 1: `job pause` → paused → `resume` → running → kill -9 mid-job → remount adopts it and it completes by plan regeneration → compactions + merges ≥ 1, nodes walked after ≤ before, `fsck --online` 0 findings" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    var bs_buf: [16]u8 = undefined;
    try rig.formatSetWith(true, &.{ "--meta-node-kib", "64", "--block-size", try std.fmt.bufPrint(&bs_buf, "{d}", .{kvmap_block}) });
    try rig.createImage();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);

    // 1. D3: the striped files, then the parked writes from a HOLDER child
    //    that keeps the fds open until the job completes and never forks
    //    or execs (`FoldHolder`).
    {
        const out = try rig.onMount("for n in 0 1 2; do dd if=\"$1/image\" of=big$n bs=1M skip=$((n * 8)) count=8 conv=fsync status=none; done; echo filled");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "filled") != null);
    }
    var holder = try FoldHolder.start(rig.mp);
    defer holder.stop();
    const user_bytes: u64 = fold_files * fold_writes_per_file * 512;
    {
        const st = try rig.stats();
        defer gpa.free(st);
        const parked = numberAfter(st, "\"parked_extent_bytes\":") orelse 0;
        const parks = numberAfter(st, "\"extent_parks\":") orelse 0;
        std.debug.print("smoke[defrag]: {d} parked extent write(s) across {d} open file(s): parked_extent_bytes {d}, extent_parks {d}\n", .{ fold_files * fold_writes_per_file, fold_files, parked, parks });
        try testing.expect(parked >= fold_files * fold_writes_per_file * fold_page);
        try testing.expect(parks >= fold_files * fold_writes_per_file);
    }
    const ip_bytes0 = blk: {
        const st = try rig.stats();
        defer gpa.free(st);
        break :blk numberAfter(st, "\"fold_inplace_write_bytes\":") orelse 0;
    };
    {
        const r = try rig.squeezefs(&.{ "defrag", "--fold", "--throttle", "50", rig.mp });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\ndefrag --fold: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        const id = jobIdIn(r.stdout) orelse return error.NoFoldJob;
        try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, id, 120 * std.time.ns_per_s));
    }
    {
        const st = try rig.stats();
        defer gpa.free(st);
        const kicked = numberAfter(st, "\"defrag_folds_kicked\":") orelse 0;
        const parked = numberAfter(st, "\"parked_extent_bytes\":") orelse 99;
        const passes = numberAfter(st, "\"fold_inplace_passes\":") orelse 0;
        const ip_bytes = (numberAfter(st, "\"fold_inplace_write_bytes\":") orelse 0) - ip_bytes0;
        const declines = numberAfter(st, "\"fold_inplace_declines\":") orelse 0;
        std.debug.print("smoke[defrag]: --fold: {d} ino(s) kicked, {d} in-place pass(es), {d} B written in place for {d} B of user writes (×{d}; the §J bound 8×), parked_extent_bytes {d} after, in-place declines {d}\n", .{ kicked, passes, ip_bytes, user_bytes, ip_bytes / user_bytes, parked, declines });
        try testing.expectEqual(@as(u64, fold_files), kicked);
        try testing.expectEqual(@as(u64, 0), parked);
        try testing.expect(passes >= fold_files * fold_writes_per_file);
        try testing.expectEqual(user_bytes * fold_page / 512, ip_bytes);
        try testing.expect(ip_bytes * 1_000_000 / user_bytes <= extent_overlay.inPlaceBoundPpm(512, 512));
    }
    // End the holder (its exit closes the fds: nothing left parked to
    // drain) and verify every byte: the 512 B patterns, the untouched rest
    // of each page from the image.
    holder.stop();
    {
        // Byte-exact against the image everywhere BUT the 12 sectors; the
        // sectors carry their byte (the expected image rebuilt with `tr`
        // mapping 512 zeros onto the octal byte).
        var w: std.Io.Writer.Allocating = .init(gpa);
        defer w.deinit();
        for (0..fold_files) |n| {
            try w.writer.print("cp big{d} \"$1/got{d}\"; dd if=\"$1/image\" of=\"$1/want{d}\" bs=1M skip={d} count=8 status=none; ", .{ n, n, n, n * 8 });
            for (0..fold_writes_per_file) |k| {
                const off: u64 = @as(u64, k) * kvmap_block + (3 + 2 * @as(u64, k)) * fold_page + 512;
                try w.writer.print("printf '%0512d' 0 | tr 0 \"\\{o:0>3}\" | dd of=\"$1/want{d}\" bs=512 seek={d} count=1 conv=notrunc status=none; ", .{ 0x40 + n * 8 + k, n, off / 512 });
            }
            try w.writer.print("cmp \"$1/got{d}\" \"$1/want{d}\" && ", .{ n, n });
        }
        try w.writer.writeAll("echo fold-exact");
        const out = try rig.onMount(w.written());
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "fold-exact") != null);
    }

    // 2. D4: the forest, the before words.
    {
        const script = try defragForestScript(gpa);
        defer gpa.free(script);
        const out = try rig.onMountTimed(script, 600);
        defer gpa.free(out);
        try testing.expectEqualStrings("800\n", out);
    }
    try rig.expectAlive("the forest build");
    {
        var got = try fsckJsonTarget(rig, rig.mp, &.{"--online"});
        defer got.report.deinit(gpa);
        try testing.expectEqual(@as(usize, 0), got.report.findings.len);
    }
    // The meta volume's node census BEFORE: the online fsck just paged
    // every tree, so the resident leaves ARE the leaves; the heap words
    // (allocated = total − free) ride along.
    const before, const heap_before, const kicked0, const merges0, const laps0 = blk: {
        const st = try heapSettledStats(rig);
        defer gpa.free(st);
        const total = numberAfter(st, "\"heap_total_extents\":") orelse 0;
        const free = numberAfter(st, "\"heap_free_extents\":") orelse 0;
        break :blk .{ numberAfter(st, "\"resident_leaves\":") orelse 0, total - free, numberAfter(st, "\"defrag_meta_compactions_kicked\":") orelse 0, numberAfter(st, "\"defrag_meta_merges\":") orelse 0, numberAfter(st, "\"meta_kv_merge_laps\":") orelse 0 };
    };
    // 2a. Cancel mid-run at throttle 1 (every chunk parks 99× its work).
    {
        const r = try rig.squeezefs(&.{ "defrag", "--meta", "--throttle", "1", rig.mp });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        const id = jobIdIn(r.stdout) orelse return error.NoMetaJob;
        try awaitJobState(rig, id, "running", 30 * std.time.ns_per_s);
        const c = try rig.squeezefs(&.{ "job", "cancel", rig.mp, id });
        defer rig.freeRun(c);
        try testing.expect(c.term.success());
        try testing.expectEqualStrings("cancelled", try awaitJobTerminal(rig, id, 120 * std.time.ns_per_s));
    }
    // 2b. Pause → resume → kill -9 mid-job → remount adopts → completes.
    var mark: usize = 0;
    {
        const r = try rig.squeezefs(&.{ "defrag", "--meta", "--throttle", "1", rig.mp });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        const id = jobIdIn(r.stdout) orelse return error.NoMetaJob;
        try awaitJobState(rig, id, "running", 30 * std.time.ns_per_s);
        {
            const p = try rig.squeezefs(&.{ "job", "pause", rig.mp, id });
            defer rig.freeRun(p);
            try testing.expect(p.term.success());
        }
        try awaitJobState(rig, id, "paused", 60 * std.time.ns_per_s);
        {
            const p = try rig.squeezefs(&.{ "job", "resume", rig.mp, id });
            defer rig.freeRun(p);
            try testing.expect(p.term.success());
        }
        try awaitJobMidRun(rig, id, 120 * std.time.ns_per_s);
        try rig.kill();
        mark = try rig.logLen();
        try rig.spawnMount(&.{});
        _ = try rig.awaitReady(mark);
        {
            const text = try rig.readLog();
            defer gpa.free(text);
            try testing.expect(std.mem.indexOf(u8, text[mark..], "job(s) adopted") != null);
        }
        // The adopted job regenerates its plan at full duty: the remaining
        // candidates compact and merge.
        {
            const t = try rig.squeezefs(&.{ "job", "throttle", rig.mp, id, "100" });
            defer rig.freeRun(t);
            if (!t.term.success()) try testing.expect(std.mem.indexOf(u8, t.stderr, "the job is terminal") != null);
        }
        try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, id, 600 * std.time.ns_per_s));
    }
    // 2c. A third pass at full duty converges to nothing left to do.
    {
        const r = try rig.squeezefs(&.{ "defrag", "--meta", rig.mp });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        const id = jobIdIn(r.stdout) orelse return error.NoMetaJob;
        try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, id, 600 * std.time.ns_per_s));
    }
    {
        // AFTER: the online fsck pages every tree again (the remount's cache
        // was cold), then a settled read — the compactions' retired
        // extents return at the barriered cycles that follow the jobs.
        var got = try fsckJsonTarget(rig, rig.mp, &.{"--online"});
        defer got.report.deinit(gpa);
        const st = try heapSettledStats(rig);
        defer gpa.free(st);
        const kicked = (numberAfter(st, "\"defrag_meta_compactions_kicked\":") orelse 0) - kicked0;
        const merges = (numberAfter(st, "\"defrag_meta_merges\":") orelse 0) - merges0;
        const laps = (numberAfter(st, "\"meta_kv_merge_laps\":") orelse 0) - laps0;
        const after = numberAfter(st, "\"resident_leaves\":") orelse 0;
        const heap_after = (numberAfter(st, "\"heap_total_extents\":") orelse 0) - (numberAfter(st, "\"heap_free_extents\":") orelse 0);
        std.debug.print("smoke[defrag]: --meta over {d} files cut to 800 and rewritten {d}×: {d} compaction(s) kicked, {d} merge(s), {d} whole merge lap(s) (the cancelled, the kill-resumed and the converging job together); leaves {d} → {d} (interior {d}); meta heap {d} → {d} allocated extent(s), {d} pending free; merge candidates {d}; fsck --online {d} finding(s)\n", .{ defrag_forest_files, defrag_rewrite_rounds, kicked, merges, laps, before, after, numberAfter(st, "\"resident_interior\":") orelse 0, heap_before, heap_after, numberAfter(st, "\"heap_pending_free\":") orelse 0, numberAfter(st, "\"meta_kv_merge_candidates\":") orelse 0, got.report.findings.len });
        if (got.report.findings.len != 0) for (got.report.findings) |f| std.debug.print("  unexpected: {f}\n", .{f});
        try testing.expectEqual(@as(usize, 0), got.report.findings.len);
        try testing.expect(kicked >= 1);
        // Three jobs over one meta volume: the converging one's lap at
        // least (a cancel or the kill may land inside a lap).
        try testing.expect(laps >= 1);
        try testing.expect(after <= before);
        try testing.expectEqual(@as(u64, 0), numberAfter(st, "\"meta_kv_merge_candidates\":") orelse 0);
        try testing.expectEqual(@as(u64, 0), numberAfter(st, "\"invariant_tripwires\":") orelse 0);
    }
    {
        const out = try rig.onMount("test \"$(ls m | wc -l)\" = 800 && test \"$(cat m/f0)\" = m0-r3 && test \"$(cat m/f7990)\" = m7990-r3 && echo forest-ok");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "forest-ok") != null);
    }
    try rig.expectAlive("the defrag row");
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the defrag row");
}

const extent_overlay = @import("../data/extent_overlay.zig");

// ---- the online fsck: shards, the storm, the set ----

/// The online report (JSON) of `target` through the admin lane; a shard
/// when asked. The caller judges the code.
fn fsckOnline(rig: *Rig, target: []const u8, residue: ?[]const u8) !FsckOutcome {
    if (residue) |s| return fsckJsonTarget(rig, target, &.{ "--online", "--shards", s });
    return fsckJsonTarget(rig, target, &.{"--online"});
}

/// Write the online report's JSON to `<work>/<name>` (the merge's input).
fn fsckOnlineToFile(rig: *Rig, residue: []const u8, name: []const u8) !void {
    const r = try rig.squeezefs(&.{ "fsck", "--json", "--online", "--shards", residue, rig.mp });
    defer rig.freeRun(r);
    if (r.term != .exited or r.term.exited != 0) std.debug.print("\n--- fsck --online --shards {s} ({f}) ---\n{s}\n", .{ residue, r.term, r.stderr });
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, r.term);
    var pb: [256]u8 = undefined;
    const path = try std.fmt.bufPrint(&pb, "{s}/{s}", .{ rig.work, name });
    const f = try Io.Dir.cwd().createFile(rig.io, path, .{ .truncate = true });
    defer f.close(rig.io);
    try f.writeStreamingAll(rig.io, r.stdout);
}

test "smoke: ONLINE FSCK THROUGH THE ADMIN LANE — the fill (dirs, 200 packed files, hard links, three fsync'd 20 MiB files) → `fsck --online` whole = 0 findings → `--shards 0/3, 1/3, 2/3` online (each report names its shard, each judges its residue) → `merge-reports` of the three ≡ the whole run (0 findings, the inode census summed exactly) → an 8-way `cp` storm with three online runs INSIDE it: 0 findings each (the zero-FP law under churn) → the two-daemon set: B JOINED creates 2,000 files and a tree under A's root → `fsck --online` on A reads B's slots as foreign-live and B's ring as a window (foreign_slot_scoped > 0), 0 findings; on B's mount the lane refuses by verdict (not the coordinator)" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.createImage();
    const storm_sources = try rig.shOk(rig.work, "i=0; while [ $i -lt 8 ]; do dd if=image of=s$i bs=1M skip=$((i * 25)) count=50 status=none; i=$((i + 1)); done; echo ok");
    gpa.free(storm_sources);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    // 1. The fill.
    {
        const out = try rig.onMount("mkdir -p d/e/f g; i=0; while [ $i -lt 200 ]; do dd if=\"$1/image\" of=d/e/p$i bs=64k skip=$i count=1 status=none; i=$((i + 1)); done; ln d/e/p1 g/link1; ln d/e/p2 g/link2; for n in 0 1 2; do dd if=\"$1/image\" of=big$n bs=1M skip=$((n * 20)) count=20 conv=fsync status=none; done; echo filled");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "filled") != null);
    }
    // 2. Whole, then the three shards, then the merge ≡ the whole.
    var whole = try fsckOnline(rig, rig.mp, null);
    defer whole.report.deinit(gpa);
    if (whole.report.findings.len != 0) for (whole.report.findings) |f| std.debug.print("  unexpected: {f}\n", .{f});
    try testing.expectEqual(@as(usize, 0), whole.report.findings.len);
    try testing.expectEqual(@as(u8, 0), whole.code);
    try testing.expect(whole.report.shard == null);
    try testing.expect(whole.report.counters.inodes_scanned >= 208);
    var shard_inodes: u64 = 0;
    inline for (.{ "0/3", "1/3", "2/3" }, .{ "shard0.json", "shard1.json", "shard2.json" }, 0..) |s, name, k| {
        var got = try fsckOnline(rig, rig.mp, s);
        defer got.report.deinit(gpa);
        try testing.expectEqual(@as(usize, 0), got.report.findings.len);
        try testing.expectEqual(json_scan_report.Shard{ .k = k, .n = 3 }, got.report.shard.?);
        try testing.expect(got.report.counters.inodes_scanned < whole.report.counters.inodes_scanned);
        shard_inodes += got.report.counters.inodes_scanned;
        try fsckOnlineToFile(rig, s, name);
    }
    try testing.expectEqual(whole.report.counters.inodes_scanned, shard_inodes);
    {
        var p0: [256]u8 = undefined;
        var p1: [256]u8 = undefined;
        var p2: [256]u8 = undefined;
        const r = try rig.squeezefs(&.{ "fsck", "merge-reports", "--json", try std.fmt.bufPrint(&p0, "{s}/shard0.json", .{rig.work}), try std.fmt.bufPrint(&p1, "{s}/shard1.json", .{rig.work}), try std.fmt.bufPrint(&p2, "{s}/shard2.json", .{rig.work}) });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, r.term);
        var merged = try json_scan_report.Report.parse(gpa, r.stdout);
        defer merged.deinit(gpa);
        try testing.expectEqual(@as(usize, 0), merged.findings.len);
        try testing.expect(merged.shard == null);
        try testing.expectEqual(whole.report.counters.inodes_scanned, merged.counters.inodes_scanned);
        try testing.expectEqual(whole.report.counters.dentry_refs_indexed, merged.counters.dentry_refs_indexed);
        std.debug.print("smoke[fsck-online]: whole {d} inode(s), {d} dentry ref(s), {d} block(s); 3 shards online summed {d} inode(s); merge-reports ≡ whole (0 findings, the census exact)\n", .{ whole.report.counters.inodes_scanned, whole.report.counters.dentry_refs_indexed, whole.report.counters.blocks_checked, shard_inodes });
    }
    // 3. The storm with the online runs INSIDE it: 0 findings each; the
    //    shields' counters grow (the engagement law).
    {
        var storm_cmd: [1024]u8 = undefined;
        const cmd = try std.fmt.bufPrint(&storm_cmd, "i=0; while [ $i -lt 8 ]; do ( cp {s}/s$i {s}/storm$i && rm {s}/storm$i ) & i=$((i + 1)); done; wait", .{ rig.work, rig.mp, rig.mp });
        var storm = try std.process.spawn(io, .{ .argv = &.{ "sh", "-c", cmd }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .ignore, .stderr = .ignore });
        var runs: u32 = 0;
        var exempted: u64 = 0;
        while (runs < 3) : (runs += 1) {
            var got = try fsckOnline(rig, rig.mp, null);
            defer got.report.deinit(gpa);
            if (got.report.findings.len != 0) for (got.report.findings) |f| std.debug.print("  storm run {d} unexpected: {f}\n", .{ runs, f });
            try testing.expectEqual(@as(usize, 0), got.report.findings.len);
            try testing.expectEqual(@as(u8, 0), got.code);
            exempted += got.report.counters.current_era_exempted + got.report.counters.epoch_exempted + got.report.counters.inflight_exempted + got.report.counters.suspects_cleared;
        }
        _ = try storm.wait(io);
        std.debug.print("smoke[fsck-online]: 3 online runs inside the 8-way cp storm: 0 findings; shield engagement over the runs {d} (current-era + epoch + in-flight exemptions + suspects cleared)\n", .{exempted});
    }
    try rig.expectAlive("the storm");
    // 4. The two-daemon set. The rig's daemons share ONE cache path (the
    // set's format decision) but never one ring: each member binds its own
    // root under it (0003 §T.1) — B joins beside A's LIVE residents (the
    // fill's packed files, still staged) and recovers none of them.
    const a_staged = blk: {
        const sa = try rig.stats();
        defer gpa.free(sa);
        break :blk numberAfter(sa, "\"current_staged_write_bytes\":") orelse 0;
    };
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{});
    _ = try b.awaitReady();
    {
        const sb = try b.stats();
        defer gpa.free(sb);
        try testing.expectEqual(@as(u64, 0), numberAfter(sb, "\"staging_recovery_records\":") orelse 1);
        const sa = try rig.stats();
        defer gpa.free(sa);
        try testing.expectEqual(a_staged, numberAfter(sa, "\"current_staged_write_bytes\":") orelse 0);
    }
    {
        const script = try matrixCreateScript(gpa);
        defer gpa.free(script);
        const out = try b.onMount(script);
        defer gpa.free(out);
        try testing.expectEqualStrings("500\n", out);
        const mk = try rig.onMount("mkdir a-dir");
        gpa.free(mk);
        const tree_out = try b.onMount("cd a-dir && i=0; while [ $i -lt 100 ]; do printf 'x%s\\n' $i > x$i; i=$((i + 1)); done; ls | wc -l");
        defer gpa.free(tree_out);
        try testing.expectEqualStrings("100\n", tree_out);
    }
    {
        // The live set's block plane from A: `scrub --online` (C7 alone)
        // reads every block A's forest shows (B's custody is B's: the
        // fleet residue, judged by B's engine and merged here), 0 findings.
        var got = try verbJsonTarget(rig, "scrub", rig.mp, &.{"--online"});
        defer got.report.deinit(gpa);
        const c = got.report.counters;
        std.debug.print("smoke[fsck-online]: the two-daemon set from A: scrub --online read {d} block(s) ({d} B), {d} failure(s), {d} finding(s); fleet dispatched {d}\n", .{ c.scrub_blocks_scanned, c.scrub_bytes_scanned, c.scrub_failures, got.report.findings.len, blk: {
            const sa = try rig.stats();
            defer gpa.free(sa);
            break :blk numberAfter(sa, "\"job_fleet_shards_dispatched\":") orelse 0;
        } });
        if (got.report.findings.len != 0) for (got.report.findings[0..@min(got.report.findings.len, 12)]) |f| std.debug.print("  set unexpected: {f}\n", .{f});
        try testing.expectEqual(@as(usize, 0), got.report.findings.len);
        try testing.expectEqual(@as(u8, 0), got.code);
        try testing.expectEqual(@as(u64, 0), c.scrub_failures);
        try testing.expect(c.scrub_blocks_scanned >= 15);
    }
    {
        // The whole inode plane from A over the live, UNQUIESCED set: the
        // census scopes what is a lessee's (its slots, the names its
        // directories hold — a predecessor's directories included, the
        // slot being the container —, the blocks its records reference);
        // the name arms and the reference-count arms decline while a
        // lessee is live. 0 findings is the law (the zero-FP ladder).
        var got = try fsckOnline(rig, rig.mp, null);
        defer got.report.deinit(gpa);
        const c = got.report.counters;
        std.debug.print("smoke[fsck-online]: the two-daemon set from A: fsck --online {d} inode(s), {d} dentry ref(s), {d} finding(s); foreign-slot scoped {d}, foreign-dentry scoped {d}, window scoped {d}, slots covered {d}, block-plane foreign scoped {d}, alloc census declined {d}, name arms declined {d}\n", .{ c.inodes_scanned, c.dentry_refs_indexed, got.report.findings.len, c.inode_plane_foreign_slot_scoped, c.inode_plane_foreign_dentry_scoped, c.inode_plane_window_scoped, c.inode_plane_slots_covered, c.block_plane_foreign_scoped, c.alloc_census_declined, c.inode_plane_names_declined });
        if (got.report.findings.len != 0) {
            for (got.report.findings[0..@min(got.report.findings.len, 6)]) |f| std.debug.print("  set unexpected: {f}\n", .{f});
            const la = try rig.onMount("ls -lai . | head -20; echo ---; stat -c '%i %h %n' b0 b1 b2 b3");
            defer gpa.free(la);
            std.debug.print("--- A's root ---\n{s}\n", .{la});
        }
        try testing.expectEqual(@as(usize, 0), got.report.findings.len);
        try testing.expectEqual(@as(u8, 0), got.code);
        // B's slots are its own: A's census scopes them out rather than
        // judging a tree that changes under it; the name arms declined.
        try testing.expect(c.inode_plane_foreign_dentry_scoped > 0);
        try testing.expect(c.inode_plane_slots_covered < inos.ROUTING_WIDTH);
        try testing.expectEqual(@as(u64, 1), c.inode_plane_names_declined);
    }
    {
        // B is not the coordinator: its lane answers by verdict.
        const r = try rig.squeezefs(&.{ "fsck", "--online", b.mp });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "not the coordinator") != null);
    }
    try b.expectAlive("the set's fsck");
    {
        // Neither daemon tripped an invariant across the set's runs (the
        // coordinator's `fsck_findings` site included: 0 findings is the law).
        const sb = try b.stats();
        defer gpa.free(sb);
        try testing.expectEqual(@as(u64, 0), numberAfter(sb, "\"invariant_tripwires\":") orelse 0);
        const sa = try rig.stats();
        defer gpa.free(sa);
        try testing.expectEqual(@as(u64, 0), numberAfter(sa, "\"invariant_tripwires\":") orelse 0);
    }
    const mark_b_dismount = blk: {
        const t = try b.readLog();
        defer gpa.free(t);
        break :blk t.len;
    };
    try b.umount();
    try b.expectCleanExit("B's unmount");
    {
        // B's dismount promotes every staged resident it still holds
        // (charter §8 `umount`: "teardown promotes staged files first"),
        // none failed — a joiner's packed promotions commit through its
        // own slot.
        const text = try b.readLog();
        defer gpa.free(text);
        const tail = text[@min(mark_b_dismount, text.len)..];
        const line_at = std.mem.indexOf(u8, tail, "dismount promotion: ") orelse {
            std.debug.print("\n--- daemon b: no dismount promotion line ---\n{s}\n", .{tail});
            return error.NoDismountPromotionLine;
        };
        const line = tail[line_at..];
        if (std.mem.indexOf(u8, line[0..@min(line.len, 200)], ", 0 failed") == null) {
            std.debug.print("\n--- daemon b: the dismount promotion failed residents ---\n", .{});
            var it = std.mem.splitScalar(u8, tail, '\n');
            var n: usize = 0;
            while (it.next()) |l| if (std.mem.indexOf(u8, l, "promotion") != null or std.mem.indexOf(u8, l, "pack") != null) {
                std.debug.print("{s}\n", .{l[0..@min(l.len, 300)]});
                n += 1;
                if (n == 30) break;
            };
            return error.DismountPromotionFailed;
        }
    }
    try rig.expectAlive("the set");
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the online fsck row");
}

// ---- the fleet READ shard: B as the coordinator's worker ----

/// The fleet row's data: 1 GiB in 8 × 128 MiB fsync'd files, half written
/// on each daemon (the set's 4 MiB blocks: 256 blocks to scrub).
const fleet_files_per_side: usize = 4;
const fleet_file_mib: u64 = 128;
const fleet_blocks: u64 = 2 * fleet_files_per_side * fleet_file_mib / 4;

fn fleetFillScript(gpa: std.mem.Allocator, prefix: []const u8) ![]u8 {
    var w: std.Io.Writer.Allocating = .init(gpa);
    defer w.deinit();
    try w.writer.print("mkdir -p {s}; i=0; while [ $i -lt {d} ]; do dd if=\"$1/image\" of={s}/f$i bs=1M skip=$((i * 32)) count={d} conv=fsync status=none; i=$((i + 1)); done; echo filled", .{ prefix, fleet_files_per_side, prefix, fleet_file_mib });
    return w.toOwnedSlice();
}

/// Both daemons' fabric lines since the marks (a fleet stage's post-mortem).
fn printFleetLines(rig: *Rig, b: *Peer, a_from: usize, b_from: usize) !void {
    const gpa = rig.gpa;
    const ta = try rig.readLog();
    defer gpa.free(ta);
    const tb = try b.readLog();
    defer gpa.free(tb);
    inline for (.{ .{ "A", ta, a_from }, .{ "B", tb, b_from } }) |side| {
        std.debug.print("\n--- daemon {s}: the fabric since the stage began ---\n", .{side[0]});
        var it = std.mem.splitScalar(u8, side[1][@min(side[2], side[1].len)..], '\n');
        var n: usize = 0;
        while (it.next()) |l| if (std.mem.indexOf(u8, l, "fleet") != null or std.mem.indexOf(u8, l, "job fabric") != null or std.mem.indexOf(u8, l, "admin lane") != null or std.mem.indexOf(u8, l, "fsck") != null or std.mem.indexOf(u8, l, "shard") != null) {
            std.debug.print("{s}\n", .{l[0..@min(l.len, 300)]});
            n += 1;
            if (n == 40) break;
        };
    }
}

/// A bounded poll of a daemon's `.stats` word until it reads ≥ `want`.
fn awaitPeerAtLeast(b: *Peer, k: []const u8, want: u64, bound_ns: u64) !u64 {
    const deadline = clock.monotonicNs() + bound_ns;
    while (true) {
        if (try b.statsTry()) |st| {
            defer b.rig.gpa.free(st);
            const v = numberAfter(st, k) orelse 0;
            if (v >= want) return v;
        }
        try b.expectAlive(k);
        if (clock.monotonicNs() >= deadline) {
            std.debug.print("\n--- daemon {s}: {s} never reached {d} ---\n", .{ b.name, k, want });
            return error.Timeout;
        }
        sleepMs(100);
    }
}

/// The merged-census row's pack: 8 × 64 KiB staged files promoted by the
/// fsync lever into ONE pack block (the §J small-file pack of a 4 MiB block).
const merged_pack_files: usize = 8;

fn mergedPackScript(gpa: std.mem.Allocator, dir: []const u8, base_skip: usize) ![]u8 {
    var w: std.Io.Writer.Allocating = .init(gpa);
    defer w.deinit();
    try w.writer.print("mkdir -p {s}; i=0; while [ $i -lt {d} ]; do dd if=\"$1/image\" of={s}/f$i bs=64k skip=$(({d} + i)) count=1 conv=fsync status=none; i=$((i + 1)); done; sha256sum {s}/f* > \"$1/{s}.sha\"; echo filled", .{ dir, merged_pack_files, dir, base_skip, dir, dir });
    return w.toOwnedSlice();
}

/// `sha256sum -c` of the row's `<sha>` list minus the rm'd `f0` through
/// the mount at `mp`; the row fails with the reader's lines on a mismatch.
fn mergedPackReadOn(rig: *Rig, mp: []const u8, who: []const u8, sha: []const u8) !void {
    const gpa = rig.gpa;
    const r = try std.process.run(gpa, rig.io, .{ .argv = &.{ "sh", "-c", "grep -v '/f0$' \"$1/$2\" | sha256sum --quiet -c - 2>&1 && echo ok", "smoke", rig.work, sha }, .cwd = .{ .path = mp }, .environ_map = &rig.env });
    defer gpa.free(r.stdout);
    defer gpa.free(r.stderr);
    if (!std.mem.eql(u8, r.stdout, "ok\n")) std.debug.print("--- {s} reads the packed tenants of {s} ---\n{s}{s}\n", .{ who, sha, r.stdout, r.stderr });
    try testing.expectEqualStrings("ok\n", r.stdout);
    std.debug.print("smoke[merged-census]: {s}: {d} packed tenant(s) of {s} sha-equal\n", .{ who, merged_pack_files - 1, sha });
}

test "smoke: THE FLEET'S MERGED BLOCK CENSUS — mount A + B JOINED with the fleet on and B under the `SQUEEZEFS_TEST_SHIP_EVERY_FREE` seam (the reference's shape, 0002 row 500): B promotes 8 staged files into ONE pack by the fsync lever, `rm`s one tenant → the NONTERMINAL free ships to A, whose untracked arm frees and punches the pack under B's 7 live tenants (A: punches +1) → `fsck --online <A>` REPORTS it: B's custody census rides its proposal, the coordinator's merged lost arm names the freed-while-referenced block and its tenant, confirmed by a second census round (C2 lost, exit 1) → B remounted (its caches gone): the 7 tenants read back WRONG off the device → the seam OFF: the same `rm` stays local (A: punches unchanged), B remounted reads its 7 tenants sha-equal, `fsck --online` 0 findings" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.env.put("SQUEEZEFS_FLEET_JOBS", "1");
    try rig.formatSet(true);
    try rig.createImage();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    // B: the fsync lever (its staged files pack at the fsync) and the seam.
    var env_seam = try envWith(gpa, &rig.env, &.{ .{ "SQUEEZEFS_FSYNC_PROMOTE_STAGED", "1" }, .{ "SQUEEZEFS_TEST_SHIP_EVERY_FREE", "1" } });
    defer env_seam.deinit();
    var env_fixed = try envWith(gpa, &rig.env, &.{.{ "SQUEEZEFS_FSYNC_PROMOTE_STAGED", "1" }});
    defer env_fixed.deinit();
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.env = &env_seam;
    try b.spawn(&.{});
    _ = try b.awaitReady();
    _ = try b.awaitLog("enrolled at the coordinator as a fleet READ worker", 0, 30 * std.time.ns_per_s);
    // 1. The seam ON: the pack, the rm, the punch.
    {
        const script = try mergedPackScript(gpa, "seam", 0);
        defer gpa.free(script);
        const out = try b.onMount(script);
        defer gpa.free(out);
        try testing.expectEqualStrings("filled\n", out);
        const sb = try b.stats();
        defer gpa.free(sb);
        try testing.expect((numberAfter(sb, "\"fsync_promoted_files\":") orelse 0) >= merged_pack_files);
    }
    const punches0 = blk: {
        const sa = try rig.stats();
        defer gpa.free(sa);
        break :blk numberAfter(sa, "\"block_free_file_punches\":") orelse 0;
    };
    {
        const out = try b.onMount("rm seam/f0 && echo removed");
        defer gpa.free(out);
        try testing.expectEqualStrings("removed\n", out);
    }
    {
        // A executed the shipped nonterminal free: the pack punched under
        // B's seven live tenants.
        const sa = try awaitAtLeast(rig, "\"block_free_file_punches\":", punches0 + 1, 60 * std.time.ns_per_s);
        defer gpa.free(sa);
        const punches = numberAfter(sa, "\"block_free_file_punches\":") orelse 0;
        std.debug.print("smoke[merged-census]: the seam ON — B rm'd one of 8 tenants; A: block_free_file_punches {d} → {d}\n", .{ punches0, punches });
        try testing.expect(punches >= punches0 + 1);
    }
    {
        // The merged census REPORTS it while B is live (its slot trees
        // are out of A's forest; only its proposal carries the references).
        var got = try fsckJsonTarget(rig, rig.mp, &.{"--online"});
        defer got.report.deinit(gpa);
        const c = got.report.counters;
        var lost: u32 = 0;
        for (got.report.findings) |f| if (f == .c2_lost) {
            lost += 1;
        };
        std.debug.print("smoke[merged-census]: fsck --online on A: {d} finding(s), {d} C2 lost; {d} member binding(s) merged, {d} suspect(s) cleared by the second round; exit {d}\n", .{ got.report.findings.len, lost, c.block_plane_member_bindings, c.suspects_cleared, got.code });
        for (got.report.findings[0..@min(got.report.findings.len, 8)]) |f| std.debug.print("  reported: {f}\n", .{f});
        // One row per punched block (the arm names the block and its first
        // live tenant), at least the block the rm'd tenant shared.
        try testing.expect(lost >= 1);
        try testing.expectEqual(got.report.findings.len, lost);
        try testing.expectEqual(@as(u8, 1), got.code);
        try testing.expect(c.block_plane_member_bindings >= merged_pack_files - 1);
        try testing.expectEqual(@as(u64, 0), c.suspects_cleared);
    }
    try b.expectAlive("the seam arm");
    // 2. B remounted (its RAM caches gone; the seam OFF from here): the
    // loss is on the device.
    try b.umount();
    try b.expectCleanExit("B's unmount after the seam arm");
    b.env = &env_fixed;
    var b_mark = try b.logLen();
    try b.spawn(&.{});
    _ = try b.awaitLog("ready: ", b_mark, ready_wait_ns);
    _ = try b.awaitLog("enrolled at the coordinator as a fleet READ worker", b_mark, 30 * std.time.ns_per_s);
    {
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "sh", "-c", "grep -v '/f0$' \"$1/seam.sha\" | sha256sum --quiet -c - 2>&1 | head -3; echo \"rc=${PIPESTATUS[1]}\"", "smoke", rig.work }, .cwd = .{ .path = b.mp }, .environ_map = &rig.env });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        std.debug.print("smoke[merged-census]: the seam ON — B remounted reads its 7 remaining tenants off the device:\n{s}", .{r.stdout});
        try testing.expect(std.mem.indexOf(u8, r.stdout, "rc=0") == null);
    }
    // 3. The seam OFF: the same shape on the shipped law.
    {
        const script = try mergedPackScript(gpa, "fixed", merged_pack_files);
        defer gpa.free(script);
        const out = try b.onMount(script);
        defer gpa.free(out);
        try testing.expectEqualStrings("filled\n", out);
    }
    const punches1 = blk: {
        const sa = try rig.stats();
        defer gpa.free(sa);
        break :blk numberAfter(sa, "\"block_free_file_punches\":") orelse 0;
    };
    {
        const out = try b.onMount("rm fixed/f0 && sync -f . && echo removed");
        defer gpa.free(out);
        try testing.expectEqualStrings("removed\n", out);
    }
    {
        var got = try fsckJsonTarget(rig, rig.mp, &.{"--online"});
        defer got.report.deinit(gpa);
        const sa = try rig.stats();
        defer gpa.free(sa);
        std.debug.print("smoke[merged-census]: the seam OFF — the rm stayed local (A punches {d} → {d}); fsck --online on A while B is live: {d} finding(s), {d} member binding(s) merged\n", .{ punches1, numberAfter(sa, "\"block_free_file_punches\":") orelse 0, got.report.findings.len, got.report.counters.block_plane_member_bindings });
        for (got.report.findings[0..@min(got.report.findings.len, 6)]) |f| std.debug.print("  unexpected: {f}\n", .{f});
        try testing.expectEqual(punches1, numberAfter(sa, "\"block_free_file_punches\":") orelse 0);
        try testing.expectEqual(@as(usize, 0), got.report.findings.len);
        try testing.expectEqual(@as(u8, 0), got.code);
        try testing.expect(got.report.counters.block_plane_member_bindings >= merged_pack_files - 1);
    }
    // A reader on any member reads any member's committed file: A (the
    // manager) reads B's seven remaining packed tenants while B is live.
    try mergedPackReadOn(rig, rig.mp, "A while B is live", "fixed.sha");
    // B remounted once more: the fixed tenants intact on the device.
    try b.umount();
    try b.expectCleanExit("B's unmount after the fixed arm");
    b_mark = try b.logLen();
    try b.spawn(&.{});
    _ = try b.awaitLog("ready: ", b_mark, ready_wait_ns);
    {
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "sh", "-c", "grep -v '/f0$' \"$1/fixed.sha\" | sha256sum --quiet -c - && echo ok", "smoke", rig.work }, .cwd = .{ .path = b.mp }, .environ_map = &rig.env });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        if (!std.mem.eql(u8, r.stdout, "ok\n")) std.debug.print("--- B remounted reads its fixed tenants ---\n{s}{s}\n", .{ r.stdout, r.stderr });
        try testing.expectEqualStrings("ok\n", r.stdout);
    }
    try b.umount();
    try b.expectCleanExit("B's last unmount");
    try rig.expectAlive("the merged census row");
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

/// The re-use row's set: two data volumes of three grant floors of 1 MiB
/// blocks each (`block_grant.BLOCK_GRANT_FLOOR` = 64: a smaller volume
/// cannot hold one member's window beside another's) — the fill to
/// ENOSPC is ~384 MiB, seconds on the venue.
const reuse_row_block: u64 = 1 << 20;
const reuse_row_data_bytes: u64 = 3 * 64 * reuse_row_block;

/// The re-use row's read: ONE 1 MiB O_DIRECT request straddling the two
/// blocks of a two-block file — a multi-block request the queue thread's
/// warm ladder demotes (its direct device leg serves single blocks and
/// deposits nothing); on B, with the dest lease off
/// (`SQUEEZEFS_READ_DEST_LEASE=0` — an aligned sub-block window would
/// otherwise DMA straight into the reply and deposit nothing), each
/// 512 KiB half is the lane's whole-block fill, which deposits. `$2` =
/// the file.
const reuse_row_window_read = "dd if=\"$2\" iflag=direct,skip_bytes skip=524288 bs=1048576 count=1 status=none | sha256sum";

test "smoke: A MEMBER'S CACHED COPY OF ANOTHER MEMBER'S BLOCKS NEVER OUTLIVES THE BLOCKS' RE-USE (0003 §W.2) — two 192 MiB data volumes of 1 MiB blocks, no cache path (every write beyond inline striped) → mount A (the manager) + B JOINED (the dest lease off: its window reads are whole-block fills that deposit) → A writes `f` (two blocks, fsync'd) → B reads it until its tiers hold its blocks → A fills the set to ENOSPC → A `rm f`: the delete recalls B's token and B purges the blocks' keys before it acks (scoped purges ≥ 1 on B) → A writes `g` (two blocks, fsync'd) into the blocks the rm freed → B reads `g`: sha-equal to A's, never `f`'s bytes" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    var bs_buf: [16]u8 = undefined;
    try rig.formatSetSized(false, &.{ "--block-size", try std.fmt.bufPrint(&bs_buf, "{d}", .{reuse_row_block}) }, reuse_row_data_bytes);
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    var env_b = try envWith(gpa, &rig.env, &.{.{ "SQUEEZEFS_READ_DEST_LEASE", "0" }});
    defer env_b.deinit();
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    b.env = &env_b;
    try b.spawn(&.{});
    _ = try b.awaitReady();
    // 1. A's file `f`: two blocks; the sha of the straddling window.
    {
        const out = try rig.onMount("dd if=/dev/urandom of=f bs=1048576 count=2 iflag=fullblock conv=fsync status=none && set -- \"$1\" f && " ++ reuse_row_window_read ++ " > \"$1/f.sha\" && echo written");
        defer gpa.free(out);
        try testing.expectEqualStrings("written\n", out);
    }
    // 2. B reads the window (under A's token) until its tiers hold both
    //    blocks.
    {
        var deposited = false;
        var pass: u32 = 0;
        while (pass < 4 and !deposited) : (pass += 1) {
            const out = try b.onMount("set -- \"$1\" f && " ++ reuse_row_window_read ++ " > \"$1/f.b.sha\"; if cmp -s \"$1/f.b.sha\" \"$1/f.sha\"; then echo equal; else cat \"$1/f.b.sha\" \"$1/f.sha\"; fi");
            defer gpa.free(out);
            if (!std.mem.eql(u8, out, "equal\n")) std.debug.print("smoke[reuse]: B's read of `f` differs:\n{s}", .{out});
            try testing.expectEqualStrings("equal\n", out);
            const sb = try b.stats();
            defer gpa.free(sb);
            deposited = (numberAfter(sb, "\"hot_block_current_bytes\":") orelse 0) + (numberAfter(sb, "\"read_lane_hold_bytes\":") orelse 0) >= 2 * reuse_row_block;
        }
        std.debug.print("smoke[reuse]: B read A's `f` {d} time(s); its tiers hold both blocks: {}\n", .{ pass, deposited });
        try testing.expect(deposited);
    }
    // 3. A fills the set to ENOSPC one block per file (B's tiers keep
    //    `f`'s blocks): a refused one-block write allocated nothing, so no
    //    free lands later to give `g` a block other than `f`'s.
    {
        const out = try rig.onMountTimed("j=0; while dd if=/dev/zero of=fill$j bs=1048576 count=1 conv=fsync status=none 2>/dev/null; do j=$((j+1)); done; rm -f fill$j; echo $j", 900);
        defer gpa.free(out);
        std.debug.print("smoke[reuse]: A filled the set with {s} MiB to ENOSPC\n", .{std.mem.trimEnd(u8, out, "\n")});
    }
    const sb0 = try b.stats();
    const scoped0 = numberAfter(sb0, "\"dlm_token_recall_scoped_purges\":") orelse 0;
    const keys0 = numberAfter(sb0, "\"dlm_token_recall_purged_keys\":") orelse 0;
    gpa.free(sb0);
    // 4. A deletes `f` (B holds its token: the recall), then writes `g`:
    //    the only clear blocks are the ones `f` held. The delete's frees
    //    land asynchronously (seconds on the venue) and a write on a
    //    latched-full set answers ENOSPC until they do: the write is
    //    polled until it lands, bounded at 60 s.
    {
        const out = try rig.onMountTimed("rm f && n=0; until dd if=/dev/urandom of=g bs=1048576 count=2 iflag=fullblock conv=fsync status=none 2>/dev/null; do n=$((n+1)); [ $n -lt 120 ] || exit 1; rm -f g; sleep 0.5; done; set -- \"$1\" g && " ++ reuse_row_window_read ++ " > \"$1/g.sha\" && echo written", 600);
        defer gpa.free(out);
        try testing.expectEqualStrings("written\n", out);
    }
    // 5. B reads `g`'s window: A's bytes, never the cached `f` — then the
    //    recall's purge on B, the counters that name it.
    {
        const out = try b.onMount("set -- \"$1\" g && " ++ reuse_row_window_read ++ " > \"$1/g.b.sha\"; if cmp -s \"$1/g.b.sha\" \"$1/g.sha\"; then echo equal; elif cmp -s \"$1/g.b.sha\" \"$1/f.sha\"; then echo stale-f; else echo other; fi");
        defer gpa.free(out);
        std.debug.print("smoke[reuse]: B reads A's `g` (the re-used blocks): {s}", .{out});
        try testing.expectEqualStrings("equal\n", out);
    }
    {
        const sb = try b.stats();
        defer gpa.free(sb);
        const scoped = numberAfter(sb, "\"dlm_token_recall_scoped_purges\":") orelse 0;
        const keys = numberAfter(sb, "\"dlm_token_recall_purged_keys\":") orelse 0;
        std.debug.print("smoke[reuse]: B's recall of `f`: scoped purges {d} → {d}, purged keys {d} → {d}, census purges {d}\n", .{ scoped0, scoped, keys0, keys, numberAfter(sb, "\"dlm_token_recall_census_purges\":") orelse 0 });
        try testing.expect(scoped >= scoped0 + 1);
        try testing.expect(keys >= keys0 + 2);
    }
    try b.expectAlive("the re-use row");
    try rig.expectAlive("the re-use row");
    try b.umount();
    try b.expectCleanExit("B's unmount after the re-use row");
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
}

test "smoke: THE FLEET READ SHARD — mount A (the coordinator) + B JOINED with the fleet on: B enrolls at A as a READ worker over its own manager link (job_remote_workers 1 on A) → 1 GiB of fsync'd files, 512 MiB written on each side → `scrub --online <A>` partitions 2 ways: shard 0 on A online, shard 1 LEASED to B, run by B's engine over B's own plane (B's scrub_blocks_scanned grows, job_fleet_worker_shards 1), PROPOSED as read_shard_result and merged on A (dispatched 1 = completed 1, relocal 0; every block scanned, 0 findings) → `scrub --online --throttle 5` again: B's residue stretched by the duty cycle, kill -9 B inside it → A's departure expires the lease (fencing +1), the residue is LOST and the local view re-run (relocal 1), the job completes whole, 0 findings; the engagement counters per side" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var t_owner_text: [8]u8 = undefined;
    try rig.env.put("SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", try std.fmt.bufPrint(&t_owner_text, "{d}", .{smoke_t_owner_ms}));
    try rig.formatSet(true);
    try rig.createImage();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    const b = try Peer.init(rig, "b");
    defer b.deinit();
    try b.spawn(&.{});
    _ = try b.awaitReady();
    _ = try b.awaitLog("enrolled at the coordinator as a fleet READ worker", 0, 60 * std.time.ns_per_s);
    {
        const st = try rig.stats();
        defer gpa.free(st);
        try testing.expectEqual(@as(u64, 1), numberAfter(st, "\"job_remote_workers\":") orelse 0);
    }
    // 1. The fill: 512 MiB on each side.
    {
        const sa = try fleetFillScript(gpa, "a");
        defer gpa.free(sa);
        const oa = try rig.onMount(sa);
        defer gpa.free(oa);
        try testing.expect(std.mem.indexOf(u8, oa, "filled") != null);
        const sb = try fleetFillScript(gpa, "b");
        defer gpa.free(sb);
        const ob = try b.onMount(sb);
        defer gpa.free(ob);
        try testing.expect(std.mem.indexOf(u8, ob, "filled") != null);
    }
    // 2. The sharded scrub: shard 1 runs on B.
    const b_scanned0 = blk: {
        const st = try b.stats();
        defer gpa.free(st);
        break :blk numberAfter(st, "\"scrub_blocks_scanned\":") orelse 0;
    };
    const a_mark = try rig.logLen();
    {
        var got = try fsckJsonTarget(rig, rig.mp, &.{ "--online", "--scrub" });
        defer got.report.deinit(gpa);
        if (got.report.findings.len != 0) for (got.report.findings) |f| std.debug.print("  unexpected: {f}\n", .{f});
        const sa = try rig.stats();
        defer gpa.free(sa);
        const sb = try b.stats();
        defer gpa.free(sb);
        const b_scanned = numberAfter(sb, "\"scrub_blocks_scanned\":") orelse 0;
        std.debug.print("smoke[fleet]: scrub --online on A over {d} block(s): shard 0 on A, shard 1 on B — A: dispatched {d}, completed {d}, relocal {d}, remote shards {d}; B: worker shards {d}, its scrub scanned {d} block(s) ({d} B read off its own plane); merged report {d} block(s) scanned ({d} B), {d} failure(s), {d} finding(s), exit {d}\n", .{ fleet_blocks, numberAfter(sa, "\"job_fleet_shards_dispatched\":") orelse 0, numberAfter(sa, "\"job_fleet_shards_completed\":") orelse 0, numberAfter(sa, "\"job_fleet_shards_relocal\":") orelse 0, numberAfter(sa, "\"job_remote_shards\":") orelse 0, numberAfter(sb, "\"job_fleet_worker_shards\":") orelse 0, b_scanned - b_scanned0, numberAfter(sb, "\"fsck_scrub_read_bytes\":") orelse 0, got.report.counters.scrub_blocks_scanned, got.report.counters.scrub_bytes_scanned, got.report.counters.scrub_failures, got.report.findings.len, got.code });
        try testing.expectEqual(@as(usize, 0), got.report.findings.len);
        try testing.expectEqual(@as(u8, 0), got.code);
        try testing.expect(got.report.counters.scrub_blocks_scanned >= fleet_blocks);
        try testing.expectEqual(@as(u64, 0), got.report.counters.scrub_failures);
        _ = try rig.awaitLog("1 proposed by the fleet, 1 run here", a_mark, 10 * std.time.ns_per_s);
        try testing.expectEqual(@as(u64, 1), numberAfter(sa, "\"job_fleet_shards_dispatched\":") orelse 0);
        try testing.expectEqual(@as(u64, 1), numberAfter(sa, "\"job_fleet_shards_completed\":") orelse 0);
        try testing.expectEqual(@as(u64, 0), numberAfter(sa, "\"job_fleet_shards_relocal\":") orelse 0);
        try testing.expectEqual(@as(u64, 1), numberAfter(sb, "\"job_fleet_worker_shards\":") orelse 0);
        try testing.expect(b_scanned > b_scanned0);
    }
    // 3. kill -9 B mid-shard. The scrub throttled to 5 % duty stretches
    // B's residue to tens of seconds (the §8 duty-cycle law); B's shard
    // counter moves at the shard's START, so the kill lands inside it.
    const a_mark2 = try rig.logLen();
    const b_shards0 = blk: {
        const st = try b.stats();
        defer gpa.free(st);
        break :blk numberAfter(st, "\"job_fleet_worker_shards\":") orelse 0;
    };
    const exp0 = blk: {
        const st = try rig.stats();
        defer gpa.free(st);
        break :blk numberAfter(st, "\"job_remote_lease_expiries\":") orelse 0;
    };
    // The scrub in a child: it waits for the job's terminal state.
    var scrub = try std.process.spawn(io, .{ .argv = &.{ rig.bin, "scrub", "--online", "--json", "--throttle", "5", rig.mp }, .environ_map = &rig.env, .stdin = .ignore, .stdout = .{ .file = rig.log_file.? }, .stderr = .{ .file = rig.log_file.? } });
    _ = awaitPeerAtLeast(b, "\"job_fleet_worker_shards\":", b_shards0 + 1, 120 * std.time.ns_per_s) catch |e| {
        try printFleetLines(rig, b, a_mark2, 0);
        return e;
    };
    try b.kill();
    // Exit 0 is the CLI's word for a clean report (findings exit 1).
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try scrub.wait(io));
    _ = try rig.awaitLog("0 proposed by the fleet, 2 run here", a_mark2, 300 * std.time.ns_per_s);
    {
        const sa = try rig.stats();
        defer gpa.free(sa);
        std.debug.print("smoke[fleet]: kill -9 B mid-shard — A: lease expiries {d} (was {d}), relocal {d}, dispatched {d}, completed {d}, remote workers {d}; the job completed whole (exit 0: 0 findings)\n", .{ numberAfter(sa, "\"job_remote_lease_expiries\":") orelse 0, exp0, numberAfter(sa, "\"job_fleet_shards_relocal\":") orelse 0, numberAfter(sa, "\"job_fleet_shards_dispatched\":") orelse 0, numberAfter(sa, "\"job_fleet_shards_completed\":") orelse 0, numberAfter(sa, "\"job_remote_workers\":") orelse 99 });
        try testing.expect((numberAfter(sa, "\"job_remote_lease_expiries\":") orelse 0) >= exp0 + 1);
        try testing.expectEqual(@as(u64, 1), numberAfter(sa, "\"job_fleet_shards_relocal\":") orelse 0);
        try testing.expectEqual(@as(u64, 2), numberAfter(sa, "\"job_fleet_shards_dispatched\":") orelse 0);
        try testing.expectEqual(@as(u64, 1), numberAfter(sa, "\"job_fleet_shards_completed\":") orelse 0);
        try testing.expectEqual(@as(u64, 0), numberAfter(sa, "\"job_remote_workers\":") orelse 99);
        try testing.expectEqual(@as(u64, 0), numberAfter(sa, "\"invariant_tripwires\":") orelse 0);
    }
    try rig.expectAlive("the fleet row");
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    _ = try fsckClean(rig, "the fleet row");
}

/// The `key id {16 hex}` the rekey verb prints (the new key's id).
fn keyIdIn(text: []const u8) ?[]const u8 {
    const i = std.mem.indexOf(u8, text, "onto key id ") orelse return null;
    const start = i + "onto key id ".len;
    if (text.len < start + 16) return null;
    for (text[start .. start + 16]) |c| if (!std.ascii.isHex(c)) return null;
    return text[start .. start + 16];
}

/// A key file of 48 random bytes (base64), 0600, at `work/<name>`.
fn makeKeyFile(rig: *Rig, name: []const u8) ![]u8 {
    var script: std.Io.Writer.Allocating = .init(rig.gpa);
    defer script.deinit();
    try script.writer.print("umask 077; head -c 48 /dev/urandom | base64 > {s}; chmod 600 {s}", .{ name, name });
    const out = try rig.shOk(rig.work, script.written());
    rig.gpa.free(out);
    return std.fmt.allocPrint(rig.gpa, "{s}/{s}", .{ rig.work, name });
}

/// The key-dir files the rotation rows installed (process-wide state on
/// the venue): removed at the row's end.
fn removeKeyDirFile(key_id_hex: []const u8) void {
    var pb: [128:0]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "/etc/squeezefs/keys/{s}.key", .{key_id_hex}, 0) catch return;
    _ = linux.unlink(path.ptr);
}

test "smoke: THE REKEY ROW — format (1 MiB blocks, lz4 + aes-256-gcm, key A, a cache path) → fill (40 packed 64 KiB files, three fsync'd 20 MiB incompressible striped files, an 8 MiB compressible one) → umount/remount (the packs) → `defrag --rekey --encrypt-key B --throttle 50 <mp>` opens the window and sweeps every block onto B through the ONE mover while a write lands under B → the window closes (rekey_window_open 0, windows_closed 1, len drift 0), every file byte-exact → `config show` names B and no next key → a mount with key A refuses by id, with B serves every file → a second rotation to C at 10 % killed -9 mid-sweep → remount under B resolves C from the key dir, adopts the job, converges and flips to C → fsck --offline --scrub clean under C" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    var bs_buf: [16]u8 = undefined;
    try rig.createMetaVolume();
    for ([_][:0]const u8{ rig.d0, rig.d1 }) |dp| {
        const f = try Io.Dir.cwd().createFile(io, dp, .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, matrix_vol_len);
    }
    _ = linux.mkdir(rig.cache.ptr, 0o700);
    const key_a = try makeKeyFile(rig, "a.key");
    defer gpa.free(key_a);
    const key_b = try makeKeyFile(rig, "b.key");
    defer gpa.free(key_b);
    const key_c = try makeKeyFile(rig, "c.key");
    defer gpa.free(key_c);
    var key_b_id: [16]u8 = undefined;
    var key_c_id: [16]u8 = undefined;
    var ids_installed: u8 = 0;
    defer {
        if (ids_installed >= 1) removeKeyDirFile(&key_b_id);
        if (ids_installed >= 2) removeKeyDirFile(&key_c_id);
    }
    {
        const r = try rig.squeezefs(&.{ "format", "--block-size", try std.fmt.bufPrint(&bs_buf, "{d}", .{kvmap_block}), "--compress", "lz4", "--encrypt", "aes-256-gcm", "--encrypt-key", key_a, "--disk-cache-paths", rig.cache, rig.meta_uri, rig.data_uri });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }
    try rig.createImage();
    try rig.env.put("SQUEEZEFS_ENCRYPT_KEY_FILE", key_a);
    try rig.env.put("SQUEEZEFS_BLOCK_REFS_VERIFY", "1");

    // 1. The fill under A; umount (the staged files promote into packs);
    //    remount; the shas.
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(0);
    {
        const out = try rig.onMount("mkdir -p p && i=0; while [ $i -lt 40 ]; do dd if=\"$1/image\" of=p/small$i bs=64k skip=$i count=1 status=none; i=$((i + 1)); done; for n in 0 1 2; do dd if=\"$1/image\" of=big$n bs=1M skip=$((n * 20)) count=20 conv=fsync status=none; done; yes compressible-rotation-row | head -c 8388608 > zeroish && sync -f .; echo filled");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "filled") != null);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    var mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    {
        const out = try rig.onMount("sha256sum big0 big1 big2 zeroish p/small* | sort -k2 > \"$1/before.sha\"; echo ok");
        defer gpa.free(out);
    }
    {
        // Every packed tenant byte-exact against its SOURCE slice (the
        // before/after shas alone passed with every tenant refused EIO:
        // sha256sum skipped them identically on both sides).
        const out = try rig.onMount("i=0; bad=0; while [ $i -lt 40 ]; do dd if=\"$1/image\" bs=64k skip=$i count=1 status=none | cmp - p/small$i >/dev/null 2>&1 || bad=$((bad + 1)); i=$((i + 1)); done; echo packed-bad=$bad");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "packed-bad=0") != null);
    }
    const blocks_before = blk: {
        const st = try rig.stats();
        defer gpa.free(st);
        break :blk numberAfter(st, "\"rekey_blocks_rewritten\":") orelse 0;
    };

    // 2. The rotation A → B at 50 %: the verb installs B's key-dir file,
    //    opens the window, submits the sweep; a write during the sweep
    //    lands under B; the sweep converges and closes the window.
    var job_b_buf: [jobs_records.JOB_ID_TEXT_LEN]u8 = undefined;
    {
        const r = try rig.squeezefs(&.{ "defrag", "--rekey", "--encrypt-key", key_b, "--throttle", "50", rig.mp });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nrekey B: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "rotation window open") != null);
        @memcpy(&job_b_buf, jobIdIn(r.stdout) orelse return error.NoRekeyJob);
        @memcpy(&key_b_id, keyIdIn(r.stdout) orelse return error.NoKeyId);
        ids_installed = 1;
    }
    {
        const out = try rig.onMount("dd if=\"$1/image\" of=during bs=1M skip=100 count=4 conv=fsync status=none && sha256sum during > \"$1/during.sha\"; echo written");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "written") != null);
    }
    try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, &job_b_buf, 900 * std.time.ns_per_s));
    {
        const st = try rig.stats();
        defer gpa.free(st);
        const rewritten = (numberAfter(st, "\"rekey_blocks_rewritten\":") orelse 0) - blocks_before;
        std.debug.print("\nsmoke[rekey]: A → B: {d} block(s) rewritten ({d} B), {d} pass(es), {d} confirming deferral(s); windows closed {d}, window open {d}, previous-key unwraps {d}; tripwires len_drift {d} tenant_len_conflict {d}\n", .{ rewritten, numberAfter(st, "\"rekey_bytes_rewritten\":") orelse 0, numberAfter(st, "\"rekey_passes\":") orelse 0, numberAfter(st, "\"rekey_confirm_deferrals\":") orelse 0, numberAfter(st, "\"rekey_windows_closed\":") orelse 0, numberAfter(st, "\"rekey_window_open\":") orelse 99, numberAfter(st, "\"crypto_key_unwraps_previous\":") orelse 0, numberAfter(st, "\"rekey_image_len_drift\":") orelse 99, numberAfter(st, "\"rekey_tenant_len_conflict\":") orelse 99 });
        // 60 MiB of striped blocks + the 8 MiB file + the packs: well over 60.
        try testing.expect(rewritten >= 60);
        try testing.expectEqual(@as(u64, 1), numberAfter(st, "\"rekey_windows_closed\":") orelse 0);
        try testing.expectEqual(@as(u64, 0), numberAfter(st, "\"rekey_window_open\":") orelse 99);
        try testing.expectEqual(@as(u64, 0), numberAfter(st, "\"rekey_image_len_drift\":") orelse 99);
        try testing.expectEqual(@as(u64, 0), numberAfter(st, "\"rekey_tenant_len_conflict\":") orelse 99);
        try testing.expectEqual(@as(u64, 0), numberAfter(st, "\"packed_mapping_refusals\":") orelse 99);
    }
    {
        const out = try rig.onMount("sha256sum big0 big1 big2 zeroish p/small* | sort -k2 | cmp - \"$1/before.sha\" && sha256sum during | cmp - \"$1/during.sha\" && echo all-exact");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "all-exact") != null);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    // The durable record (`config show` reads the newest ledger record —
    // the dismount's checkpoint landed the flip): B primary, no next key.
    {
        const r = try rig.squeezefs(&.{ "config", "show", "--json", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        var needle: [64]u8 = undefined;
        try testing.expect(std.mem.indexOf(u8, r.stdout, try std.fmt.bufPrint(&needle, "\"key_id\":\"{s}\"", .{&key_b_id})) != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "key_id_next") == null);
    }
    // 3. Key A is retired: a mount under it refuses by id; under B it serves.
    {
        const r = try rig.squeezefs(&.{ "mount", rig.meta_uri, rig.mp });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "key id differs") != null);
    }
    try rig.env.put("SQUEEZEFS_ENCRYPT_KEY_FILE", key_b);
    mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    {
        const out = try rig.onMount("sha256sum big0 big1 big2 zeroish p/small* | sort -k2 | cmp - \"$1/before.sha\" && sha256sum during | cmp - \"$1/during.sha\" && echo all-exact-under-b");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "all-exact-under-b") != null);
    }
    // 4. B → C at 10 %, kill -9 mid-sweep → remount under B: C resolves
    //    from the key dir, the window is open, the job adopted; it
    //    converges and flips the primary to C.
    var job_c_buf: [jobs_records.JOB_ID_TEXT_LEN]u8 = undefined;
    {
        const r = try rig.squeezefs(&.{ "defrag", "--rekey", "--encrypt-key", key_c, "--throttle", "10", rig.mp });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nrekey C: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        @memcpy(&job_c_buf, jobIdIn(r.stdout) orelse return error.NoRekeyJob);
        @memcpy(&key_c_id, keyIdIn(r.stdout) orelse return error.NoKeyId);
        ids_installed = 2;
    }
    {
        // Mid-sweep: at least one block rewritten, the job still running.
        const deadline = clock.monotonicNs() + 120 * std.time.ns_per_s;
        var mid = false;
        while (clock.monotonicNs() < deadline) {
            const r = try rig.squeezefs(&.{ "job", "status", rig.mp, &job_c_buf });
            defer rig.freeRun(r);
            if (std.mem.indexOf(u8, r.stdout, " running, ")) |_| {
                if (std.mem.indexOf(u8, r.stdout, ", 0/")) |_| {} else {
                    mid = true;
                    break;
                }
            }
            sleepMs(50);
        }
        try testing.expect(mid);
    }
    rig.killed = true;
    _ = linux.kill(rig.child_pid, .KILL);
    try testing.expectEqual(std.process.Child.Term{ .signal = .KILL }, try rig.waitChild());
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(rig.mp.ptr, 0)));
    mark = try rig.logLen();
    try rig.spawnMount(&.{});
    _ = try rig.awaitReady(mark);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[mark..], "key-rotation window OPEN") != null);
        try testing.expect(std.mem.indexOf(u8, text[mark..], "job(s) adopted") != null);
    }
    try testing.expectEqualStrings("completed", try awaitJobTerminal(rig, &job_c_buf, 900 * std.time.ns_per_s));
    {
        const st = try rig.stats();
        defer gpa.free(st);
        std.debug.print("smoke[rekey]: B → C killed -9 mid-sweep → remount adopted it: {d} block(s) rewritten after the remount, windows closed {d}, window open {d}, previous-key unwraps {d} (the remount's unwrap cache is cold: every block the sweep rewrote decoded under B)\n", .{ numberAfter(st, "\"rekey_blocks_rewritten\":") orelse 0, numberAfter(st, "\"rekey_windows_closed\":") orelse 0, numberAfter(st, "\"rekey_window_open\":") orelse 99, numberAfter(st, "\"crypto_key_unwraps_previous\":") orelse 0 });
        try testing.expectEqual(@as(u64, 1), numberAfter(st, "\"rekey_windows_closed\":") orelse 0);
        try testing.expectEqual(@as(u64, 0), numberAfter(st, "\"rekey_window_open\":") orelse 99);
        // The A → B line showed 0 here: the pre-rotation shas had filled the
        // blob-keyed unwrap cache. After the remount the sweep's decodes
        // are the first unwraps of their blobs and fall to the previous key.
        try testing.expect((numberAfter(st, "\"crypto_key_unwraps_previous\":") orelse 0) >= 1);
    }
    {
        const out = try rig.onMount("sha256sum big0 big1 big2 zeroish p/small* | sort -k2 | cmp - \"$1/before.sha\" && sha256sum during | cmp - \"$1/during.sha\" && echo all-exact-under-c");
        defer gpa.free(out);
        try testing.expect(std.mem.indexOf(u8, out, "all-exact-under-c") != null);
    }
    try rig.expectAlive("the rekey row");
    {
        const st = try rig.stats();
        defer gpa.free(st);
        try testing.expectEqual(@as(u64, 0), numberAfter(st, "\"packed_mapping_refusals\":") orelse 99);
        try testing.expectEqual(@as(u64, 0), numberAfter(st, "\"invariant_tripwires\":") orelse 99);
    }
    try rig.umount();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, try rig.waitChild());
    {
        const r = try rig.squeezefs(&.{ "config", "show", "--json", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        var needle: [64]u8 = undefined;
        try testing.expect(std.mem.indexOf(u8, r.stdout, try std.fmt.bufPrint(&needle, "\"key_id\":\"{s}\"", .{&key_c_id})) != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "key_id_next") == null);
    }
    try rig.env.put("SQUEEZEFS_ENCRYPT_KEY_FILE", key_c);
    _ = try fsckClean(rig, "the rekey row");
}

/// `fsckJson` over any target (the online arm's mountpoint).
/// A JSON fsck verb's parsed report and exit code.
const FsckOutcome = struct { report: json_scan_report.Report, code: u8 };

fn fsckJsonTarget(rig: *Rig, target: []const u8, extra: []const []const u8) !FsckOutcome {
    return verbJsonTarget(rig, "fsck", target, extra);
}

/// `fsck` or `scrub` (C7 alone) with `--json`; the report parsed.
fn verbJsonTarget(rig: *Rig, verb: []const u8, target: []const u8, extra: []const []const u8) !FsckOutcome {
    var argv: [8][]const u8 = undefined;
    var n: usize = 0;
    argv[n] = verb;
    n += 1;
    argv[n] = "--json";
    n += 1;
    for (extra) |a| {
        argv[n] = a;
        n += 1;
    }
    argv[n] = target;
    n += 1;
    const r = try rig.squeezefs(argv[0..n]);
    defer rig.freeRun(r);
    const code: u8 = switch (r.term) {
        .exited => |c| c,
        else => 255,
    };
    const report = json_scan_report.Report.parse(rig.gpa, r.stdout) catch {
        std.debug.print("\n--- fsck ({f}) printed no report ---\n{s}\n--- stderr ---\n{s}\n", .{ r.term, r.stdout, r.stderr });
        return error.FsckNoReport;
    };
    return .{ .report = report, .code = code };
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
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
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
