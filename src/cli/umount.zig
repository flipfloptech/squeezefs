//! `squeezefs umount [--lazy] <mountpoint>` (charter §8 "`umount`
//! (teardown promotes staged files first)"; §4 "Teardown": only an exit
//! with no teardown or an EXTERNAL `umount(8)` — which sends no DESTROY on
//! a type-`fuse` mount — leaves the kernel's drain as the recourse;
//! decision 0003 §S, the busy-mount contract): the verb SIGNALS the daemon
//! and lets its orderly `close()` run the teardown in the charter's order
//! — the drains release every retained zero-copy slot while the connection
//! is live, the staged files promote, the final checkpoint lands, and the
//! daemon's own `umount2` detaches — then waits for the daemon's exit and
//! confirms the detach. Linux only.
//!
//! The daemon is found through the mount's own `.stats` inode (the
//! `mount` family's `pid`): the one channel that names exactly the process
//! serving THIS mountpoint. When no daemon answers there (a mount whose
//! daemon already died, a foreign FUSE mount, a path that is no mount) the
//! verb falls back LOUD to the external detach — root's `umount2(path, 0)`
//! with EBUSY retried for one lock-plane tick (`sync/tick.zig`, 2 s: a
//! caller still returning from its last syscall), anyone else's
//! `fusermount3 -u` — never a lazy detach by itself: `MNT_DETACH` hides an
//! owed reply behind a disappearing mountpoint, and the operator asked for
//! a real unmount.
//!
//! BUSY (decision 0003 §S). Before the signal the verb runs the holder
//! census (`holders.zig`: every process's cwd/root/exe/fds/mappings on the
//! mount, by the kernel's mount id; this process `chdir`s to `/` first so
//! it is never its own holder). A held mount is REFUSED: `EBUSY`, the
//! holders named on stderr, exit 1, nothing signalled — the mount keeps
//! serving, as `umount(8)` of a busy ext4 would. The daemon runs the same
//! census when the signal lands (a raw `kill -TERM` meets it too) and
//! refuses the same way, counting `transport_umount_busy_refusals`; the
//! verb reads that counter back through `.stats` so a holder that arrived
//! between the two censuses is still answered `EBUSY` with its name, not
//! a 30 s wait. `--lazy` is the explicit opt-in to the other shape: the
//! daemon detaches the mountpoint now (`MNT_DETACH`) and serves every
//! holder until the kernel ends the connection, then tears down; the verb
//! names the holders it leaves behind and waits for the exit only when
//! there are none.
//!
//! THE VERDICT IS ORDER-BASED, on kernel facts alone (never 0 after a
//! kill): the mount's entry gone from the table while the daemon lived (or
//! the daemon gone with the mount detached and no holder left) is the
//! daemon's own unmount — exit 0 once it exited; the daemon gone with the
//! mount still attached, or with holders still on a lazily detached mount,
//! is a death (the supervisor's ladder, a crash) — exit 1, the mountpoint
//! freed externally so the dead connection does not sit there as
//! `ENOTCONN`; a daemon alive past the exit bound is exit 1 and never
//! detached from under.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const fusermount = @import("fusermount.zig");
const holders = @import("holders.zig");
const errors = @import("../core/errors.zig");
const clock = @import("../core/clock.zig");
const json_scan = @import("../core/json_scan.zig");
const mountinfo = @import("../core/mountinfo.zig");
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

/// Polls between two reads of the daemon's `.stats` while it still serves
/// (its refusal counter): a render is a cold document, read at the
/// drain's lease budget (100 ms), not at the liveness quantum.
const STATS_POLL_EVERY: u64 = transport.worker.DRAIN_LEASE_BUDGET_NS / EXIT_POLL_NS;

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

/// The daemon's busy-refusal count out of `.stats` (0 when the family or
/// the key is absent: an older daemon).
pub fn refusalsOf(stats_text: []const u8) u64 {
    return json_scan.number(stats_text, "transport_umount_busy_refusals") orelse 0;
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

/// The daemon's build commit out of `.stats` (`build.commit`, the first
/// `"commit"` key: the build family precedes every other).
pub fn buildCommitOf(stats_text: []const u8) ?[]const u8 {
    return json_scan.string(stats_text, "commit");
}

/// The signalled path's verdict.
pub const Signalled = union(enum) {
    /// The daemon unmounted (its mount left the table while it lived) and
    /// exited.
    exited: linux.pid_t,
    /// The daemon died with the mount still attached (or, lazily
    /// detached, with holders still on it): a kill or a crash, never its
    /// own unmount.
    died: linux.pid_t,
    /// The daemon refused the stop: the mount is busy (its census ran
    /// after ours and found a holder); it keeps serving.
    busy: linux.pid_t,
    /// `--lazy`: the mount is detached and the daemon serves the holders
    /// left on it; it exits when the last leaves.
    detached_held: linux.pid_t,
    /// The daemon is still alive at the deadline (never detached from
    /// under it: its drains may still owe the kernel replies).
    still_alive: linux.pid_t,
    /// `--lazy` on a daemon of another build: SIGUSR2's default action
    /// KILLS a process without our handler, so the lazy stop is sent only
    /// to a daemon that provably has one — the same build as this verb.
    foreign_build: linux.pid_t,
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

/// `<mountpoint>/.stats` (owned); null when unreadable. The 16 MiB bound
/// is the renderer's own document cap.
fn readStats(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8) ?[]u8 {
    const path = std.fmt.allocPrint(gpa, "{s}/.stats", .{mountpoint}) catch return null;
    defer gpa.free(path);
    return Io.Dir.cwd().readFileAlloc(io, path, gpa, .limited(16 << 20)) catch null;
}

/// The mount under judgement: its table entry at the start (null when
/// nothing is mounted at the path), the census over it, the mount id the
/// verdict watches.
const Watched = struct {
    gpa: std.mem.Allocator,
    io: Io,
    mountpoint: []const u8,
    target: ?holders.Target = null,
    /// Two table buffers: the start's (the target and its stacked mounts
    /// slice into it) and a scratch for every later look.
    table: []u8,
    scratch: []u8,
    text: []const u8 = "",

    fn init(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8) Watched {
        var self: Watched = .{ .gpa = gpa, .io = io, .mountpoint = mountpoint, .table = &.{}, .scratch = &.{} };
        self.table = gpa.alloc(u8, mountinfo.READ_BYTES) catch return self;
        self.scratch = gpa.alloc(u8, mountinfo.READ_BYTES) catch return self;
        self.text = mountinfo.read(io, self.table);
        if (mountinfo.entryAt(self.text, mountpoint)) |e| self.target = holders.Target.of(e);
        return self;
    }

    fn deinit(self: *Watched) void {
        if (self.scratch.len != 0) self.gpa.free(self.scratch);
        if (self.table.len != 0) self.gpa.free(self.table);
    }

    /// Is the mount we started with still attached at the path?
    fn attached(self: *const Watched) bool {
        const t = self.target orelse return false;
        if (self.scratch.len == 0) return false;
        const e = mountinfo.entryAt(mountinfo.read(self.io, self.scratch), self.mountpoint) orelse return false;
        return e.id == t.mount_id;
    }

    /// The census over the mount (attached or lazily detached: its holders
    /// still reference its id), everyone but this verb and the daemon.
    fn census(self: *const Watched, daemon_pid: linux.pid_t) holders.Census {
        const t = self.target orelse return .{};
        return holders.census(self.gpa, self.io, t, self.text, &.{ linux.getpid(), daemon_pid });
    }
};

/// Signal the daemon serving `mountpoint` (TERM, or USR2 under `lazy`)
/// and judge the outcome by the order of the kernel's facts (module doc).
pub fn signalDaemon(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8, lazy: bool, our_commit: []const u8, watched: *const Watched) Signalled {
    const text = readStats(gpa, io, mountpoint) orelse return .{ .fallback = .stats_unreadable };
    defer gpa.free(text);
    const pid = daemonPidOf(text) orelse return .{ .fallback = .no_pid };
    if (lazy and !std.mem.eql(u8, buildCommitOf(text) orelse "", our_commit)) return .{ .foreign_build = pid };
    const refusals_before = refusalsOf(text);
    if (linux.errno(linux.kill(pid, if (lazy) .USR2 else .TERM)) != .SUCCESS) return .{ .fallback = .daemon_gone };
    const deadline = clock.monotonicNs() + DAEMON_EXIT_WAIT_NS;
    var detached = false;
    var polls: u64 = 0;
    while (true) : (polls += 1) {
        const is_alive = alive(io, pid);
        if (!detached and !watched.attached()) detached = true;
        if (!is_alive) {
            if (!detached) return .{ .died = pid };
            // Detached and gone: the daemon's own unmount, unless it left
            // holders behind on a lazily detached mount (then it died
            // before the kernel ended the connection).
            if (lazy and watched.census(pid).busy()) return .{ .died = pid };
            return .{ .exited = pid };
        }
        if (detached) {
            // The census is a /proc walk: at the document cadence, not the
            // liveness quantum.
            if (lazy and polls % STATS_POLL_EVERY == 0 and watched.census(pid).busy()) return .{ .detached_held = pid };
        } else if (!lazy and polls % STATS_POLL_EVERY == 0) {
            // Still attached and serving: did the daemon's census refuse?
            if (readStats(gpa, io, mountpoint)) |now_text| {
                defer gpa.free(now_text);
                if (refusalsOf(now_text) > refusals_before) return .{ .busy = pid };
            }
        }
        if (clock.monotonicNs() >= deadline) return .{ .still_alive = pid };
        io.sleep(.fromNanoseconds(EXIT_POLL_NS), .awake) catch {};
    }
}

/// Is `path` (still) a `fuse`-type mount? `/proc/self/mountinfo`'s exact
/// mountpoint field (never a stat of the path: that is a request to the
/// very daemon whose exit is being judged).
pub fn isFuseMount(io: Io, gpa: std.mem.Allocator, path: []const u8) bool {
    const buf = gpa.alloc(u8, mountinfo.READ_BYTES) catch return true;
    defer gpa.free(buf);
    return mountinfoIsFuse(mountinfo.read(io, buf), path);
}

/// The fstype of the LAST line naming `mountpoint` is `fuse`/`fuse.*`.
pub fn mountinfoIsFuse(text: []const u8, mountpoint: []const u8) bool {
    const e = mountinfo.entryAt(text, mountpoint) orelse return false;
    return e.isFuse();
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

/// The refusal line: the typed class (`error.Busy`, EBUSY through the ONE
/// table) and the holders.
fn refuseBusy(p: cli.Process, path: []const u8, c: holders.Census) u8 {
    p.stderr.print("squeezefs umount: {s} is busy ({f}) — {t} ({s}): nothing unmounted, the mount keeps serving; retry when they are gone, or `squeezefs umount --lazy {s}` to detach now and let the daemon serve them to the end\n", .{ path, c, errors.toErrno(error.Busy), @errorName(error.Busy), path }) catch {};
    return cli.exit_failed;
}

pub fn run(p: cli.Process, u: args.Umount) u8 {
    var zbuf: [linux.PATH_MAX:0]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{u.mountpoint}, 0) catch {
        p.stderr.writeAll("squeezefs umount: the mountpoint path is too long\n") catch {};
        return cli.exit_usage;
    };
    // Never our own holder: the verb is often typed from inside the mount.
    _ = linux.chdir("/");
    var watched = Watched.init(p.gpa, p.io, path);
    defer watched.deinit();
    // The pre-signal census: a daemon pid is unknown yet (its `.stats` is
    // read by `signalDaemon`), and the daemon holds nothing of its mount.
    if (!u.lazy and watched.target != null) {
        const c = watched.census(0);
        if (c.busy()) return refuseBusy(p, path, c);
    }
    switch (signalDaemon(p.gpa, p.io, path, u.lazy, p.build.commit, &watched)) {
        .exited => |pid| {
            if (!isFuseMount(p.io, p.gpa, path)) return cli.exit_ok;
            // A successor mounted over the path after ours left: not ours
            // to touch; the daemon's own cycle completed.
            p.stderr.print("squeezefs umount: daemon {d} exited and its mount is gone; another FUSE mount now sits at {s} (left alone)\n", .{ pid, path }) catch {};
            return cli.exit_ok;
        },
        .busy => |pid| return refuseBusy(p, path, watched.census(pid)),
        .detached_held => |pid| {
            p.stdout.print("squeezefs umount: {s} detached; daemon {d} serves the {f} until the last leaves, then tears down (its supervisor logs the exit)\n", .{ path, pid, watched.census(pid) }) catch {};
            return cli.exit_ok;
        },
        .died => |pid| {
            p.stderr.print("squeezefs umount: daemon {d} died without unmounting {s} (see its log and supervisor) — the dead connection is detached externally; the unmount was NOT orderly\n", .{ pid, path }) catch {};
            if (isFuseMount(p.io, p.gpa, path)) _ = detachExternally(p, path);
            return cli.exit_failed;
        },
        .still_alive => |pid| {
            p.stderr.print("squeezefs umount: daemon {d} has not exited {d} s after the signal — its teardown is still running (or wedged: see its log and supervisor); not detaching from under it\n", .{ pid, DAEMON_EXIT_WAIT_NS / std.time.ns_per_s }) catch {};
            return cli.exit_failed;
        },
        .foreign_build => |pid| {
            p.stderr.print("squeezefs umount: daemon {d} at {s} is another build than this verb — the lazy stop (SIGUSR2) is sent only to a daemon known to handle it; `squeezefs umount {s}` (the orderly stop) works across builds\n", .{ pid, path, path }) catch {};
            return cli.exit_failed;
        },
        .fallback => |why| {
            p.stderr.print("squeezefs umount: {s}: {s} — detaching externally (an external unmount of a type-fuse mount sends the daemon no DESTROY; a still-running daemon tears down on the connection's abort)\n", .{ path, why.text() }) catch {};
            const rc = detachExternally(p, path);
            if (rc != cli.exit_ok and watched.target != null) {
                const c = watched.census(0);
                if (c.busy()) p.stderr.print("squeezefs umount: {s} is held by {f}\n", .{ path, c }) catch {};
            }
            return rc;
        },
    }
}

const testing = std.testing;

test "the daemon pid is the mount family's `pid`; 0 or absent names no daemon; the refusal count defaults to 0; the build commit is the build family's" {
    try testing.expectEqual(@as(?linux.pid_t, 4242), daemonPidOf("{\"build\":{},\"mount\":{\"uid\":0,\"gid\":0,\"pid\":4242,\"mountpoint\":\"/m\"}}"));
    try testing.expectEqual(@as(?linux.pid_t, null), daemonPidOf("{\"mount\":{\"uid\":0,\"pid\":0}}"));
    try testing.expectEqual(@as(?linux.pid_t, null), daemonPidOf("{\"mount\":{\"uid\":0}}"));
    try testing.expectEqual(@as(?linux.pid_t, null), daemonPidOf("{\"mount\":{\"pid\":4294967296}}"));
    try testing.expectEqual(@as(u64, 0), refusalsOf("{\"mount\":{\"pid\":1}}"));
    try testing.expectEqualStrings("abc123", buildCommitOf("{\"build\":{\"train\":\"0.1.0\",\"commit\":\"abc123\",\"profile\":\"gate\"},\"mount\":{\"pid\":1}}").?);
    try testing.expect(buildCommitOf("{\"mount\":{\"pid\":1}}") == null);
    try testing.expectEqual(@as(u64, 3), refusalsOf("{\"transport\":{\"transport_umount_busy_retries\":9,\"transport_umount_busy_refusals\":3}}"));
}

test "the exit wait covers the workers' residency backstop plus one tick; the .stats poll rides the drain's lease budget" {
    try testing.expectEqual(transport.worker.RESIDENCY_NS + tick_law.tick_ns, DAEMON_EXIT_WAIT_NS);
    try testing.expect(DAEMON_EXIT_WAIT_NS > transport.worker.RESIDENCY_NS);
    try testing.expectEqual(@as(u64, 5), STATS_POLL_EVERY);
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

test "a directory that is no mount: no daemon to signal (.stats unreadable), not watched, not a FUSE mount, and the external detach reports the kernel's EINVAL through the ONE table" {
    if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "umount2(2) of a non-mount needs root");
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64:0]u8 = undefined;
    const dir = try std.fmt.bufPrintSentinel(&pb, "/build/tmp/not-a-mount-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir(dir.ptr, 0o755);
    defer _ = linux.rmdir(dir.ptr);
    var watched = Watched.init(testing.allocator, testing.io, dir);
    defer watched.deinit();
    try testing.expect(watched.target == null);
    try testing.expect(!watched.attached());
    try testing.expectEqual(Signalled{ .fallback = .stats_unreadable }, signalDaemon(testing.allocator, testing.io, dir, false, "", &watched));
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
    var watched = Watched.init(testing.allocator, testing.io, dir);
    defer watched.deinit();
    try testing.expectEqual(Signalled{ .fallback = .daemon_gone }, signalDaemon(testing.allocator, testing.io, dir, false, "", &watched));
}

test "a tmpfs held by a child's cwd: the pre-signal census refuses it by name (the verb's law), the plain umount2 is EBUSY, and after the child leaves nobody holds it" {
    if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "mount(2) of a tmpfs needs root");
    const io = testing.io;
    const gpa = testing.allocator;
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64:0]u8 = undefined;
    const mp = try std.fmt.bufPrintSentinel(&pb, "/build/tmp/umount-busy-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir(mp.ptr, 0o755);
    defer _ = linux.rmdir(mp.ptr);
    if (linux.errno(linux.mount("tmpfs", mp.ptr, "tmpfs", 0, 0)) != .SUCCESS) return testkit.skip(@src(), .no_cap, "mount(2) of a tmpfs refused (CAP_SYS_ADMIN)");
    defer _ = linux.umount2(mp.ptr, linux.MNT.DETACH);
    var ready: [2]linux.fd_t = undefined;
    var done: [2]linux.fd_t = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&ready, .{ .CLOEXEC = true })));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&done, .{ .CLOEXEC = true })));
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) {
        _ = linux.close(ready[0]);
        _ = linux.close(done[1]);
        _ = linux.chdir(mp.ptr);
        var one: u8 = 1;
        _ = linux.write(ready[1], @ptrCast(&one), 1);
        _ = linux.close(ready[1]);
        _ = linux.read(done[0], @ptrCast(&one), 1);
        linux.exit(0);
    }
    const child: linux.pid_t = @intCast(rc);
    _ = linux.close(ready[1]);
    var byte: u8 = 0;
    _ = linux.read(ready[0], @ptrCast(&byte), 1);
    _ = linux.close(ready[0]);
    {
        var watched = Watched.init(gpa, io, mp);
        defer watched.deinit();
        try testing.expect(watched.target != null);
        try testing.expect(watched.attached());
        const c = watched.census(0);
        try testing.expectEqual(@as(usize, 1), c.total);
        try testing.expectEqual(child, c.named[0].pid);
        try testing.expectEqual(linux.E.BUSY, linux.errno(linux.umount2(mp.ptr, 0)));
        // The refusal line names the class and the holder.
        var out: [512]u8 = undefined;
        var w: Io.Writer = .fixed(&out);
        try w.print("{s} is busy ({f}) — {t} ({s})", .{ mp, c, errors.toErrno(error.Busy), @errorName(error.Busy) });
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "1 holder(s): pid ") != null);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), ") cwd) — BUSY (Busy)") != null);
    }
    _ = linux.close(done[1]);
    _ = linux.close(done[0]);
    var status: i32 = 0;
    _ = linux.waitpid(child, &status, 0);
    {
        var watched = Watched.init(gpa, io, mp);
        defer watched.deinit();
        try testing.expect(!watched.census(0).busy());
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(mp.ptr, 0)));
        try testing.expect(!watched.attached());
    }
}
