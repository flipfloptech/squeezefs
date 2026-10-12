//! The unprivileged mount and unmount through `fusermount3` (charter §12:
//! "the daemon links FUSE 3 only for `fusermount3`-class helpers, never for
//! the request path"; `docs/porting/fuse_abi_session.md` §10.1 "fusermount3
//! path"). Linux only.
//!
//! The `_FUSE_COMMFD` protocol (libfuse `fuse_mount_fusermount` /
//! `receive_fd`): the daemon creates a `socketpair(AF_UNIX, SOCK_STREAM)`,
//! execs `fusermount3 -o <opts> -- <mountpoint>` with ONE end's number in
//! the environment variable `_FUSE_COMMFD`; fusermount3 (setuid root)
//! opens `/dev/fuse`, performs the `mount(2)` with the kernel data it
//! builds (`fd=,rootmode=,user_id=,group_id=` plus the options we passed),
//! and sends the `/dev/fuse` fd back over the socket as ONE `SCM_RIGHTS`
//! control message carrying one data byte, then exits 0. The daemon
//! `recvmsg`s the fd, reaps the helper and owns the connection from there
//! exactly as if it had mounted itself. `auto_unmount` is never passed:
//! the daemon unmounts explicitly through `fusermount3 -u`.
//!
//! Unmount: `fusermount3 -u -- <mountpoint>`; EBUSY (a bystander's
//! transient fd) retries ten times at 100 ms — one second, the attr TTL,
//! past which the straggler is a real open and the lazy `-u -z` detaches
//! what the kernel will drop at its last close (session.rs:521–523).

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const log = @import("../core/log.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/fusermount is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const helper = "fusermount3";
pub const commfd_env = "_FUSE_COMMFD";
/// The unmount retry: ten attempts 100 ms apart (module doc).
pub const UMOUNT_RETRIES: u32 = 10;
pub const UMOUNT_RETRY_NS: u64 = 100 * std.time.ns_per_ms;

pub const MountError = error{
    SocketpairFailed,
    SpawnFailed,
    HelperFailed,
    NoFdReceived,
    OutOfMemory,
};

/// Mount `mountpoint` through fusermount3 with the `-o` string `opts`;
/// returns the `/dev/fuse` fd (CLOEXEC).
pub fn mount(gpa: std.mem.Allocator, io: Io, env: *const std.process.Environ.Map, mountpoint: []const u8, opts: []const u8) MountError!linux.fd_t {
    var pair: [2]linux.fd_t = undefined;
    if (linux.errno(linux.socketpair(linux.AF.UNIX, linux.SOCK.STREAM | linux.SOCK.CLOEXEC, 0, &pair)) != .SUCCESS) {
        log.err("fusermount3: socketpair failed", .{});
        return error.SocketpairFailed;
    }
    const ours = pair[1];
    const theirs = pair[0];
    defer _ = linux.close(ours);
    // The helper's end is closed in this process right after the spawn so
    // the recvmsg below sees EOF (not a hang) if the helper never sends.
    var theirs_open = true;
    defer if (theirs_open) {
        _ = linux.close(theirs);
    };

    var child_env = env.clone(gpa) catch return error.OutOfMemory;
    defer child_env.deinit();
    var num: [16]u8 = undefined;
    child_env.put(commfd_env, std.fmt.bufPrint(&num, "{d}", .{theirs}) catch unreachable) catch return error.OutOfMemory;

    var child = std.process.spawn(io, .{
        .argv = &.{ helper, "-o", opts, "--", mountpoint },
        .environ_map = &child_env,
        .stdin = .ignore,
        // The helper's complaints (a missing `user_allow_other`, an unowned
        // mountpoint) belong on our stderr / log; it has nothing to say on
        // stdout, and ours may be a protocol channel (the test runner's).
        .stdout = .ignore,
        .stderr = .inherit,
        .inherit_files = &.{.{ .handle = theirs, .flags = .{ .nonblocking = false } }},
    }) catch |e| {
        log.err("fusermount3: spawn failed ({s}) — is fusermount3 installed and setuid?", .{@errorName(e)});
        return error.SpawnFailed;
    };
    _ = linux.close(theirs);
    theirs_open = false;

    const received = receiveFd(ours);
    const term = child.wait(io) catch |e| {
        log.err("fusermount3: wait failed ({s})", .{@errorName(e)});
        if (received) |fd| _ = linux.close(fd);
        return error.HelperFailed;
    };
    if (!term.success()) {
        log.err("fusermount3 {f} mounting {s} with -o {s}", .{ term, mountpoint, opts });
        if (received) |fd| _ = linux.close(fd);
        return error.HelperFailed;
    }
    return received orelse {
        log.err("fusermount3 exited 0 but sent no /dev/fuse fd for {s}", .{mountpoint});
        return error.NoFdReceived;
    };
}

/// `receive_fd`: one byte of data plus the SCM_RIGHTS control message.
fn receiveFd(sock: linux.fd_t) ?linux.fd_t {
    var data: [1]u8 = undefined;
    var iov = [_]std.posix.iovec{.{ .base = &data, .len = 1 }};
    // CMSG_SPACE(sizeof(int)): header + one fd, aligned to the kernel's
    // cmsg alignment.
    const space = std.mem.alignForward(usize, @sizeOf(linux.cmsghdr), linux.cmsg_align) + std.mem.alignForward(usize, @sizeOf(i32), linux.cmsg_align);
    var control: [64]u8 align(@alignOf(linux.cmsghdr)) = undefined;
    std.debug.assert(space <= control.len);
    var msg: linux.msghdr = .{ .name = null, .namelen = 0, .iov = &iov, .iovlen = 1, .control = &control, .controllen = space, .flags = 0 };
    while (true) {
        const rc = linux.recvmsg(sock, &msg, 0);
        switch (linux.errno(rc)) {
            .SUCCESS => {
                if (rc == 0) return null;
                break;
            },
            .INTR => continue,
            else => |e| {
                log.err("fusermount3: recvmsg failed ({t})", .{e});
                return null;
            },
        }
    }
    if (msg.controllen < @sizeOf(linux.cmsghdr)) return null;
    const hdr: *const linux.cmsghdr = @ptrCast(@alignCast(&control));
    if (hdr.level != linux.SOL.SOCKET or hdr.type != linux.SCM.RIGHTS) return null;
    const data_off = std.mem.alignForward(usize, @sizeOf(linux.cmsghdr), linux.cmsg_align);
    if (hdr.len < data_off + @sizeOf(i32)) return null;
    const fd = std.mem.readInt(i32, control[data_off..][0..4], .little);
    // The received fd is not CLOEXEC: make it so (the daemon execs nothing
    // after this, but a helper spawned later must never inherit the
    // connection).
    _ = linux.fcntl(fd, linux.F.SETFD, linux.FD_CLOEXEC);
    return fd;
}

/// `fusermount3 -u` with the bounded EBUSY retry, then `-u -z`; true once
/// detached.
pub fn unmount(io: Io, mountpoint: []const u8) bool {
    var attempt: u32 = 0;
    while (attempt < UMOUNT_RETRIES) : (attempt += 1) {
        if (runHelper(io, &.{ helper, "-u", "--", mountpoint })) return true;
        const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(UMOUNT_RETRY_NS) };
        _ = linux.nanosleep(&ts, null);
    }
    log.warn("fusermount3 -u {s} stayed busy for {d} × {d} ms; detaching lazily", .{ mountpoint, UMOUNT_RETRIES, UMOUNT_RETRY_NS / std.time.ns_per_ms });
    return unmountLazy(io, mountpoint);
}

/// `fusermount3 -u -z`: the lazy detach alone (`squeezefs umount --lazy`
/// on an unprivileged mount); true once detached.
pub fn unmountLazy(io: Io, mountpoint: []const u8) bool {
    return runHelper(io, &.{ helper, "-u", "-z", "--", mountpoint });
}

fn runHelper(io: Io, argv: []const []const u8) bool {
    var child = std.process.spawn(io, .{ .argv = argv, .stdin = .ignore, .stdout = .ignore, .stderr = .inherit }) catch return false;
    const term = child.wait(io) catch return false;
    return term.success();
}

// ---------------------------------------------------------------------------
// Tests (the Linux venue: fusermount3 installed; root is admissible —
// fusermount3 serves root like any user).
// ---------------------------------------------------------------------------

const testing = std.testing;

fn haveHelper(io: Io) bool {
    const r = std.process.run(testing.allocator, io, .{ .argv = &.{ helper, "--version" } }) catch return false;
    defer testing.allocator.free(r.stdout);
    defer testing.allocator.free(r.stderr);
    return r.term.success();
}

test "fusermount3: the _FUSE_COMMFD handshake yields a /dev/fuse fd whose first request is INIT; -u detaches the mount" {
    const io = testing.io;
    if (!haveHelper(io)) return testkit.skip(@src(), .no_device, "no fusermount3 helper on this host");
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try env.put("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64:0]u8 = undefined;
    const mp = try std.fmt.bufPrintSentinel(&pb, "/build/tmp/mnt-fm-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir(mp.ptr, 0o755);
    defer _ = linux.rmdir(mp.ptr);
    defer _ = linux.umount2(mp.ptr, linux.MNT.DETACH);
    const fd = try mount(testing.allocator, io, &env, mp, "fsname=squeezefs,subtype=squeezefs,default_permissions");
    defer _ = linux.close(fd);
    // The fd is a FUSE connection: a classical read yields the INIT request
    // (opcode 26) first.
    var buf: [8192]u8 = undefined;
    const n = linux.read(fd, &buf, buf.len);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(n));
    try testing.expect(n >= 40);
    try testing.expectEqual(@as(u32, 26), std.mem.readInt(u32, buf[4..8], .little));
    // CLOEXEC was set on the received fd.
    try testing.expectEqual(@as(usize, linux.FD_CLOEXEC), linux.fcntl(fd, linux.F.GETFD, 0));
    // The mount table names it.
    var mounts: [1 << 16]u8 = undefined;
    const text = try Io.Dir.cwd().readFile(io, "/proc/self/mounts", &mounts);
    try testing.expect(std.mem.indexOf(u8, text, mp) != null);
    try testing.expect(unmount(io, mp));
    const after = try Io.Dir.cwd().readFile(io, "/proc/self/mounts", &mounts);
    try testing.expect(std.mem.indexOf(u8, after, mp) == null);
}

test "fusermount3: a refused mount (a missing mountpoint) is a typed HelperFailed, no fd leaks" {
    const io = testing.io;
    if (!haveHelper(io)) return testkit.skip(@src(), .no_device, "no fusermount3 helper on this host");
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try env.put("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.HelperFailed, mount(testing.allocator, io, &env, "/build/tmp/does-not-exist-fm", "fsname=squeezefs"));
    try log.testing.expectAllFired();
}
