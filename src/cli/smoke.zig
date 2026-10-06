//! THE SMOKE (charter §11 "pjdfstest is the release gate" — this is its
//! rung-3 stand-in; `zig build smoke`, part of `zig build gate`): the
//! built `squeezefs` binary as a child process, a 256 MiB file volume on
//! the venue's ext4, a real POSIX mount at `/build/tmp/mnt-<pid>`, the
//! shell's own tools against it, `kill -9` mid-`tar -x`, remounts, the
//! `status` summary, and the must-stay-0 gauges read through the SIGUSR1
//! stats dump (the `.stats` inode's stand-in until rung 5).
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
/// The inline bound: a file of `inline_max + 1` bytes must be refused.
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
    mp: [:0]u8,
    log_path: [:0]u8,
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
            .mp = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/mnt-{d}", .{pid}, 0),
            .log_path = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/smoke-{d}/daemon.log", .{pid}, 0),
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
        self.gpa.free(self.mp);
        self.gpa.free(self.log_path);
        self.gpa.destroy(self);
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
        argv[n] = self.meta_uri;
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
        const r = try rig.squeezefs(&.{ "format", "--single-writer", rig.meta_uri, "sqdata:///dev/null-not-used" });
        defer rig.freeRun(r);
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "formatted sqmeta://") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "data volumes accepted, NOT formatted") != null);
    }
    {
        const r = try rig.squeezefs(&.{ "format", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "already formatted") != null);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "--force") != null);
    }
    // status before any mount: not mounted, next_ino 2.
    {
        const r = try rig.squeezefs(&.{ "status", rig.meta_uri });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        try testing.expect(std.mem.indexOf(u8, r.stdout, "format v4 (one-member forest)") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "next_ino 2,") != null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "not mounted") != null);
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
        try testing.expect((numberAfter(tail, "\"fuse_write_beyond_inline_refused\":") orelse 0) >= 1);
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
