//! The guard's smoke stage (Linux venue; run by `zig build smoke` with the
//! built binary as `./squeezefs` in the cwd — in the unit run it finds no
//! binary and skips): a real daemon mounts → `clients` names the holder
//! live → `claim clear` on the live mount refuses (the flock) → `kill -9`
//! the daemon → `clients` reads the holder provably DEAD → a second mount
//! within the 45 s TTL mounts at once by the same-host dead-pid proof →
//! `umount` → `claim clear` is a no-op → another `kill -9` → `claim clear`
//! clears the dead holder's claim (operator attestation) → a fresh mount
//! finds no claim. The two verbs run in-process through the library faces
//! (`gate.clients`, `gate.claimClear`) the CLI binds.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;

const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const derive = @import("../../core/derive.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const claim = @import("claim.zig");
const gate = @import("gate.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("guard/smoke is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const testing = std.testing;

const volume_len: u64 = 256 << 20;
const ready_wait_ns: u64 = 60 * std.time.ns_per_s;

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
    pool: *uring_fs.Pool,

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
            .work = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/guard-smoke-{d}", .{pid}, 0),
            .img = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/guard-smoke-{d}/vol.img", .{pid}, 0),
            .meta_uri = try std.fmt.allocPrint(gpa, "sqmeta:///build/tmp/guard-smoke-{d}/vol.img", .{pid}),
            .mp = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/guard-mnt-{d}", .{pid}, 0),
            .log_path = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/guard-smoke-{d}/daemon.log", .{pid}, 0),
            .env = std.process.Environ.Map.init(gpa),
            .pool = undefined,
        };
        try self.env.put("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        self.pool = try uring_fs.Pool.init(gpa, uring_fs.Config.resolve(&self.env, derive.Facts.probe()));
        _ = linux.mkdir(self.work.ptr, 0o755);
        _ = linux.mkdir(self.mp.ptr, 0o755);
        const f = try Io.Dir.cwd().createFile(io, self.img, .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, volume_len);
        return self;
    }

    fn deinit(self: *Rig) void {
        if (self.child) |*c| {
            _ = linux.kill(self.child_pid, .KILL);
            _ = c.wait(self.io) catch {};
        }
        _ = linux.umount2(self.mp.ptr, linux.MNT.DETACH);
        if (self.log_file) |f| f.close(self.io);
        _ = linux.rmdir(self.mp.ptr);
        Io.Dir.cwd().deleteTree(self.io, self.work) catch {};
        self.pool.deinit();
        self.env.deinit();
        self.gpa.free(self.bin);
        self.gpa.free(self.work);
        self.gpa.free(self.img);
        self.gpa.free(self.meta_uri);
        self.gpa.free(self.mp);
        self.gpa.free(self.log_path);
        self.gpa.destroy(self);
    }

    fn squeezefs(self: *Rig, args: []const []const u8) !std.process.RunResult {
        var argv: [8][]const u8 = undefined;
        argv[0] = self.bin;
        for (args, 0..) |a, i| argv[i + 1] = a;
        return std.process.run(self.gpa, self.io, .{ .argv = argv[0 .. args.len + 1], .environ_map = &self.env });
    }

    fn spawnMount(self: *Rig) !void {
        std.debug.assert(self.child == null);
        if (self.log_file == null) self.log_file = try Io.Dir.cwd().createFile(self.io, self.log_path, .{ .truncate = false });
        self.child = try std.process.spawn(self.io, .{ .argv = &.{ self.bin, "mount", self.meta_uri, self.mp }, .environ_map = &self.env, .stdin = .ignore, .stdout = .{ .file = self.log_file.? }, .stderr = .{ .file = self.log_file.? } });
        self.child_pid = self.child.?.id.?;
    }

    fn readLog(self: *Rig) ![]u8 {
        return Io.Dir.cwd().readFileAlloc(self.io, self.log_path, self.gpa, .limited(16 << 20));
    }

    fn logLen(self: *Rig) !usize {
        const text = try self.readLog();
        defer self.gpa.free(text);
        return text.len;
    }

    /// Poll the log for `needle` after `from`; the end of the match.
    fn awaitLog(self: *Rig, needle: []const u8, from: usize) !usize {
        const deadline = clock.monotonicNs() + ready_wait_ns;
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

    fn killDaemon(self: *Rig) !void {
        var c = self.child.?;
        self.child = null;
        _ = linux.kill(self.child_pid, .KILL);
        const term = try c.wait(self.io);
        try testing.expectEqual(std.process.Child.Term{ .signal = .KILL }, term);
        // The dead connection: unmount the stale FUSE mount.
        _ = linux.umount2(self.mp.ptr, 0);
    }

    fn umount(self: *Rig) !void {
        const r = try self.squeezefs(&.{ "umount", self.mp });
        defer {
            self.gpa.free(r.stdout);
            self.gpa.free(r.stderr);
        }
        if (!r.term.success()) std.debug.print("\numount: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        var c = self.child.?;
        self.child = null;
        const deadline = clock.monotonicNs() + ready_wait_ns;
        while (alive(self.child_pid) and clock.monotonicNs() < deadline) sleepMs(20);
        const term = try c.wait(self.io);
        try testing.expect(term.success());
    }

    fn clientRows(self: *Rig) ![]claim.Registration {
        return gate.clients(self.gpa, self.io, &self.env, self.pool, self.img, null);
    }

    fn clear(self: *Rig) !gate.Report {
        return gate.claimClear(self.gpa, self.io, &self.env, self.meta_uri, .{ .pool = self.pool });
    }
};

fn alive(pid: linux.pid_t) bool {
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

/// Root, `/dev/fuse`, the binary beside us (the smoke's venue law).
fn venueAvailable() bool {
    if (linux.getuid() != 0) return false;
    const rc = linux.open("/dev/fuse", .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return false;
    _ = linux.close(@intCast(rc));
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, "squeezefs", 0, .{ .TYPE = true }, &st)) != .SUCCESS) {
        log.warn("guard smoke: no ./squeezefs binary in the cwd — run through `zig build smoke`. Skipping.", .{});
        return false;
    }
    return true;
}

test "smoke: the D0 guard — mount → clients names the live holder → claim clear refuses the live mount → kill -9 → clients reads the holder DEAD → a second mount within the TTL mounts by the dead-pid proof → umount → claim clear is a no-op → kill -9 → claim clear clears → a fresh mount" {
    const io = testing.io;
    if (!venueAvailable()) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    {
        const r = try rig.squeezefs(&.{ "format", rig.meta_uri });
        defer {
            gpa.free(r.stdout);
            gpa.free(r.stderr);
        }
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }
    const t0 = clock.monotonicNs();

    // 1. Mount; the holder is live and is the daemon.
    try rig.spawnMount();
    _ = try rig.awaitLog("ready: ", 0);
    {
        const rows = try rig.clientRows();
        defer gpa.free(rows);
        try testing.expectEqual(@as(usize, 1), rows.len);
        try testing.expectEqual(claim.RegistrationState.live, rows[0].state);
        try testing.expectEqual(@as(u32, @intCast(rig.child_pid)), rows[0].pid);
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        try claim.writeClientsText(&w, rows, rig.img, 1);
        std.debug.print("\n{s}", .{w.buffered()});
    }
    // 2. claim clear on the live mount: refused by the flock (the backend's
    // one loud refusal line names the holder's claim).
    {
        log.testing.expectErrorLogs(1);
        var r = try rig.clear();
        defer r.deinit();
        try log.testing.expectAllFired();
        try testing.expect(r.refused());
        try testing.expect(r.volumes[0].outcome == .live_mounted);
    }
    // 3. kill -9: the claim stays FRESH on the volume, the pid is dead.
    try rig.killDaemon();
    {
        const rows = try rig.clientRows();
        defer gpa.free(rows);
        try testing.expectEqual(@as(usize, 1), rows.len);
        try testing.expectEqual(claim.RegistrationState.dead, rows[0].state);
        try testing.expect(rows[0].age_s <= claim.STALE_TTL_S);
    }
    // 4. A second mount within the TTL: the dead-pid proof reclaims at once.
    try testing.expect(clock.monotonicNs() - t0 < claim.STALE_TTL_S * std.time.ns_per_s);
    const mark = try rig.logLen();
    try rig.spawnMount();
    const ready2 = try rig.awaitLog("ready: ", mark);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[mark..ready2], "provably dead same-host holder") != null);
        try testing.expect(std.mem.indexOf(u8, text[mark..ready2], "writer claim committed and barriered") != null);
    }
    // 5. A clean umount deletes the claim; claim clear finds nothing.
    try rig.umount();
    {
        const rows = try rig.clientRows();
        defer gpa.free(rows);
        try testing.expectEqual(@as(usize, 0), rows.len);
        var r = try rig.clear();
        defer r.deinit();
        try testing.expect(!r.refused());
        try testing.expectEqual(gate.ClearOutcome.no_claim, r.volumes[0].outcome.cleared);
    }
    // 6. kill -9 again → claim clear clears the dead holder's claim.
    const mark3 = try rig.logLen();
    try rig.spawnMount();
    _ = try rig.awaitLog("ready: ", mark3);
    try rig.killDaemon();
    {
        var r = try rig.clear();
        defer r.deinit();
        try testing.expect(!r.refused());
        try testing.expect(r.volumes[0].outcome.cleared == .cleared);
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        try r.write(&w);
        std.debug.print("{s}", .{w.buffered()});
        const rows = try rig.clientRows();
        defer gpa.free(rows);
        try testing.expectEqual(@as(usize, 0), rows.len);
    }
    // 7. A fresh mount claims an absent record (no reclaim line).
    const mark4 = try rig.logLen();
    try rig.spawnMount();
    const ready4 = try rig.awaitLog("ready: ", mark4);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text[mark4..ready4], "provably dead") == null);
        try testing.expect(std.mem.indexOf(u8, text[mark4..ready4], "guard flock+claim") != null);
    }
    try rig.umount();
    std.debug.print("guard smoke: {d} ms end to end\n", .{(clock.monotonicNs() - t0) / std.time.ns_per_ms});
}
