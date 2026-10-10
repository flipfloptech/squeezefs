//! `squeezefs umount <mountpoint>` (charter §8 "`umount` (teardown
//! promotes staged files first)"; §4 "Teardown": only an exit with no
//! teardown or an EXTERNAL `umount(8)` — which sends no DESTROY on a
//! type-`fuse` mount — leaves the kernel's drain as the recourse): the
//! verb SIGNALS the daemon and lets its orderly `close()` run the
//! teardown in the charter's order — the drains release every retained
//! zero-copy slot while the connection is live, the staged files promote,
//! the final checkpoint lands, and the daemon's own `umount2` detaches —
//! then waits for the daemon's exit and confirms the detach. Linux only.
//!
//! The daemon is found through the mount's own `.stats` inode (the
//! `mount` family's `pid`): the one channel that names exactly the process
//! serving THIS mountpoint. When no daemon answers there (a mount whose
//! daemon already died, a foreign FUSE mount, a path that is no mount) the
//! verb falls back LOUD to the external detach — root's `umount2(path, 0)`
//! with EBUSY retried for one lock-plane tick (`sync/tick.zig`, 2 s: a
//! caller still returning from its last syscall), anyone else's
//! `fusermount3 -u` — never a lazy detach from this verb: `MNT_DETACH`
//! hides an owed reply behind a disappearing mountpoint, and the operator
//! asked for a real unmount.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const fusermount = @import("fusermount.zig");
const errors = @import("../core/errors.zig");
const clock = @import("../core/clock.zig");
const json_scan = @import("../core/json_scan.zig");
const tick_law = @import("../sync/tick.zig");
const transport = @import("../fuse/transport.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/umount is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const BUSY_RETRY_NS: u64 = tick_law.tick_ns;

/// How long the signalled daemon may take to exit: the session close's
/// residency backstop (the longest wait its teardown can owe a retained
/// zero-copy slot) plus the lock plane's tick for the final checkpoint,
/// the dismount pass and the lane's trailing barrier (each sub-second on
/// a quiet mount; the tick is their shared backstop quantum).
pub const DAEMON_EXIT_WAIT_NS: u64 = transport.worker.RESIDENCY_NS + tick_law.tick_ns;

/// The poll quantum of the exit wait (the smallest the daemon's own
/// teardown can be observed at without spinning).
const EXIT_POLL_NS: u64 = 20 * std.time.ns_per_ms;

/// Root's path: `umount2(path, 0)` with the bounded EBUSY retry; the
/// errno of the last attempt on failure.
pub fn umountAsRoot(path: [:0]const u8) linux.E {
    const deadline = clock.monotonicNs() + BUSY_RETRY_NS;
    while (true) {
        const e = linux.errno(linux.umount2(path.ptr, 0));
        if (e != .BUSY or clock.monotonicNs() >= deadline) return e;
        std.Thread.yield() catch {};
    }
}

/// The daemon's pid out of the `.stats` document's `mount` family (the
/// first `"pid"` key is the mount's — the family precedes every per-op
/// family). Null when absent or 0 (a document from a renderer that
/// knows no daemon).
pub fn daemonPidOf(stats_text: []const u8) ?linux.pid_t {
    const n = json_scan.number(stats_text, "pid") orelse return null;
    if (n == 0 or n > std.math.maxInt(i32)) return null;
    return @intCast(n);
}

/// Why the verb fell back to the external detach (each reported by name).
pub const Fallback = enum {
    /// `<mountpoint>/.stats` could not be read (no mount, a foreign FUSE
    /// filesystem, a daemon that died: the connection is aborted).
    stats_unreadable,
    /// The document names no daemon pid.
    no_pid,
    /// The pid no longer exists (the daemon died between the read and
    /// the signal).
    daemon_gone,

    pub fn text(self: Fallback) []const u8 {
        return switch (self) {
            .stats_unreadable => "its .stats inode is unreadable (no squeezefs daemon serves it, or the daemon died)",
            .no_pid => "its .stats names no daemon pid",
            .daemon_gone => "the daemon named by .stats no longer exists",
        };
    }
};

/// The signalled path's verdict.
pub const Signalled = union(enum) {
    /// The daemon exited; whether the mountpoint is still a FUSE mount
    /// (the daemon's own `umount2` failed) is the caller's check.
    exited: linux.pid_t,
    /// The daemon is still alive at the deadline (never detached from
    /// under it: its drains may still owe the kernel replies).
    still_alive: linux.pid_t,
    fallback: Fallback,
};

/// Is `pid` alive? A zombie counts as gone (it has run its teardown; the
/// daemon is not this process's child, so nothing here reaps it) — the
/// state letter of `/proc/<pid>/stat` after the comm's closing paren.
fn alive(io: Io, pid: linux.pid_t) bool {
    if (linux.errno(linux.kill(pid, @fromBackingInt(@intCast(0)))) == .SRCH) return false;
    var pb: [64]u8 = undefined;
    var buf: [512]u8 = undefined;
    const path = std.fmt.bufPrint(&pb, "/proc/{d}/stat", .{pid}) catch return true;
    const text = Io.Dir.cwd().readFile(io, path, &buf) catch return true;
    const close = std.mem.lastIndexOfScalar(u8, text, ')') orelse return true;
    const rest = std.mem.trimStart(u8, text[close + 1 ..], " ");
    return rest.len == 0 or rest[0] != 'Z';
}

/// Signal the daemon serving `mountpoint` and wait for its exit.
pub fn signalDaemon(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8) Signalled {
    const path = std.fmt.allocPrint(gpa, "{s}/.stats", .{mountpoint}) catch return .{ .fallback = .stats_unreadable };
    defer gpa.free(path);
    const text = Io.Dir.cwd().readFileAlloc(io, path, gpa, .limited(16 << 20)) catch return .{ .fallback = .stats_unreadable };
    defer gpa.free(text);
    const pid = daemonPidOf(text) orelse return .{ .fallback = .no_pid };
    if (linux.errno(linux.kill(pid, .TERM)) != .SUCCESS) return .{ .fallback = .daemon_gone };
    const deadline = clock.monotonicNs() + DAEMON_EXIT_WAIT_NS;
    while (alive(io, pid)) {
        if (clock.monotonicNs() >= deadline) return .{ .still_alive = pid };
        io.sleep(.fromNanoseconds(EXIT_POLL_NS), .awake) catch {};
    }
    return .{ .exited = pid };
}

/// Is `path` (still) a `fuse`-type mount? `/proc/self/mountinfo`'s exact
/// mountpoint field (never a stat of the path: that is a request to the
/// very daemon whose exit is being judged).
pub fn isFuseMount(io: Io, gpa: std.mem.Allocator, path: []const u8) bool {
    // procfs reports size 0: a fixed buffer, never a size-driven read.
    const buf = gpa.alloc(u8, 1 << 20) catch return true;
    defer gpa.free(buf);
    const text = Io.Dir.cwd().readFile(io, "/proc/self/mountinfo", buf) catch return false;
    return mountinfoIsFuse(text, path);
}

/// `mountinfo` fields: `id parent major:minor root MOUNTPOINT options …
/// - FSTYPE source super_options`; the LAST line naming `mountpoint`
/// wins (a later mount over the same path shadows).
pub fn mountinfoIsFuse(text: []const u8, mountpoint: []const u8) bool {
    var found = false;
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        var it = std.mem.tokenizeScalar(u8, line, ' ');
        var field: usize = 0;
        var mp: ?[]const u8 = null;
        var fstype: ?[]const u8 = null;
        var after_dash = false;
        while (it.next()) |tok| : (field += 1) {
            if (field == 4) mp = tok;
            if (after_dash) {
                fstype = tok;
                break;
            }
            if (std.mem.eql(u8, tok, "-")) after_dash = true;
        }
        const m = mp orelse continue;
        const t = fstype orelse continue;
        if (!std.mem.eql(u8, m, mountpoint)) continue;
        found = std.mem.eql(u8, t, "fuse") or std.mem.startsWith(u8, t, "fuse.");
    }
    return found;
}

/// The external detach (the fallback): root's `umount2`, else fusermount3.
fn detachExternally(p: cli.Process, path: [:0]const u8) u8 {
    if (linux.geteuid() == 0) {
        const e = umountAsRoot(path);
        if (e == .SUCCESS) return cli.exit_ok;
        p.stderr.print("squeezefs umount: umount2({s}) failed: {t} ({s})\n", .{ path, e, @errorName(errors.fromErrno(e)) }) catch {};
        return cli.exit_failed;
    }
    if (fusermount.unmount(p.io, path)) return cli.exit_ok;
    p.stderr.print("squeezefs umount: fusermount3 -u {s} failed\n", .{path}) catch {};
    return cli.exit_failed;
}

pub fn run(p: cli.Process, u: args.Umount) u8 {
    var zbuf: [linux.PATH_MAX:0]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{u.mountpoint}, 0) catch {
        p.stderr.writeAll("squeezefs umount: the mountpoint path is too long\n") catch {};
        return cli.exit_usage;
    };
    switch (signalDaemon(p.gpa, p.io, path)) {
        .exited => |pid| {
            if (!isFuseMount(p.io, p.gpa, path)) return cli.exit_ok;
            // The daemon's own detach did not land (its fusermount3 leg
            // failed): the external detach is the remedy, said so.
            p.stderr.print("squeezefs umount: daemon {d} exited but {s} is still mounted — detaching externally\n", .{ pid, path }) catch {};
            return detachExternally(p, path);
        },
        .still_alive => |pid| {
            p.stderr.print("squeezefs umount: daemon {d} has not exited {d} s after SIGTERM — its teardown is still running (or wedged: see its log and supervisor); not detaching from under it\n", .{ pid, DAEMON_EXIT_WAIT_NS / std.time.ns_per_s }) catch {};
            return cli.exit_failed;
        },
        .fallback => |why| {
            p.stderr.print("squeezefs umount: {s}: {s} — detaching externally (an external unmount of a type-fuse mount sends the daemon no DESTROY; a still-running daemon tears down on the connection's abort)\n", .{ path, why.text() }) catch {};
            return detachExternally(p, path);
        },
    }
}

const testing = std.testing;

test "the daemon pid is the mount family's `pid`; 0 or absent names no daemon" {
    try testing.expectEqual(@as(?linux.pid_t, 4242), daemonPidOf("{\"build\":{},\"mount\":{\"uid\":0,\"gid\":0,\"pid\":4242,\"mountpoint\":\"/m\"}}"));
    try testing.expectEqual(@as(?linux.pid_t, null), daemonPidOf("{\"mount\":{\"uid\":0,\"pid\":0}}"));
    try testing.expectEqual(@as(?linux.pid_t, null), daemonPidOf("{\"mount\":{\"uid\":0}}"));
    try testing.expectEqual(@as(?linux.pid_t, null), daemonPidOf("{\"mount\":{\"pid\":4294967296}}"));
}

test "the exit wait covers the workers' residency backstop plus one tick" {
    try testing.expectEqual(transport.worker.RESIDENCY_NS + tick_law.tick_ns, DAEMON_EXIT_WAIT_NS);
    try testing.expect(DAEMON_EXIT_WAIT_NS > transport.worker.RESIDENCY_NS);
}

test "mountinfo: a fuse-type line at the exact mountpoint, the last line wins, a prefix match or another fstype is not a FUSE mount" {
    const text =
        \\22 1 0:21 / / rw,relatime - ext4 /dev/root rw
        \\90 22 0:45 / /build/tmp/mnt-7 rw,nosuid,nodev,relatime - fuse squeezefs rw,user_id=0,group_id=0
        \\91 22 0:46 / /build/tmp/mnt-77 rw,relatime - fuse.sshfs x rw
        \\92 22 0:47 / /build/tmp/mnt-8 rw,relatime - tmpfs tmpfs rw
        \\
    ;
    try testing.expect(mountinfoIsFuse(text, "/build/tmp/mnt-7"));
    try testing.expect(mountinfoIsFuse(text, "/build/tmp/mnt-77"));
    try testing.expect(!mountinfoIsFuse(text, "/build/tmp/mnt-8"));
    try testing.expect(!mountinfoIsFuse(text, "/build/tmp/mnt"));
    try testing.expect(!mountinfoIsFuse(text, "/"));
    // A later line over the same path shadows the earlier mount.
    const shadowed = text ++ "93 22 0:48 / /build/tmp/mnt-7 rw,relatime - tmpfs tmpfs rw\n";
    try testing.expect(!mountinfoIsFuse(shadowed, "/build/tmp/mnt-7"));
}

test "a directory that is no mount: no daemon to signal (.stats unreadable), not a FUSE mount, and the external detach reports the kernel's EINVAL through the ONE table" {
    if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "umount2(2) of a non-mount needs root");
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64:0]u8 = undefined;
    const dir = try std.fmt.bufPrintSentinel(&pb, "/build/tmp/not-a-mount-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir(dir.ptr, 0o755);
    defer _ = linux.rmdir(dir.ptr);
    try testing.expectEqual(Signalled{ .fallback = .stats_unreadable }, signalDaemon(testing.allocator, testing.io, dir));
    try testing.expect(!isFuseMount(testing.io, testing.allocator, dir));
    const e = umountAsRoot(dir);
    try testing.expectEqual(linux.E.INVAL, e);
    try testing.expectEqual(error.InvalidArgument, errors.fromErrno(e));
}

test "a .stats document naming a dead pid falls back as `daemon_gone`" {
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64]u8 = undefined;
    const dir = try std.fmt.bufPrint(&pb, "/build/tmp/fake-mount-{d}", .{linux.getpid()});
    var zb: [64:0]u8 = undefined;
    const dirz = try std.fmt.bufPrintSentinel(&zb, "{s}", .{dir}, 0);
    _ = linux.mkdir(dirz.ptr, 0o755);
    defer Io.Dir.cwd().deleteTree(testing.io, dir) catch {};
    // A pid no process holds: the kernel's pid space ends at
    // /proc/sys/kernel/pid_max (≤ 4194304); a child reaped by its parent
    // long ago is not guaranteed free, so a freshly forked-and-reaped pid
    // is the one certainly-dead number.
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) linux.exit(0);
    const dead: linux.pid_t = @intCast(rc);
    var status: i32 = 0;
    _ = linux.waitpid(dead, &status, 0);
    var sb: [96]u8 = undefined;
    const path = try std.fmt.bufPrint(&sb, "{s}/.stats", .{dir});
    var doc: [128]u8 = undefined;
    const text = try std.fmt.bufPrint(&doc, "{{\"mount\":{{\"uid\":0,\"pid\":{d}}}}}", .{dead});
    try Io.Dir.cwd().writeFile(testing.io, .{ .sub_path = path, .data = text });
    try testing.expectEqual(Signalled{ .fallback = .daemon_gone }, signalDaemon(testing.allocator, testing.io, dir));
}
