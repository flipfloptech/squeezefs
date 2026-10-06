//! `squeezefs umount <mountpoint>` (charter §8 "`umount` (teardown
//! promotes staged files first)" — no staging exists at this rung, so the
//! verb is the detach alone): root calls `umount2(mountpoint, 0)`, anyone
//! else `fusermount3 -u`. The kernel sends the daemon DESTROY and the
//! daemon's own teardown does the rest (the final checkpoint, the backend
//! close, exit 0). Linux only.
//!
//! EBUSY is retried for one lock-plane tick (`sync/tick.zig`, 2 s: a
//! caller still returning from its last syscall), then reported — never a
//! lazy detach from this verb: `MNT_DETACH` hides an owed reply behind a
//! disappearing mountpoint, and the operator asked for a real unmount.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const cli = @import("cli.zig");
const args = @import("args.zig");
const fusermount = @import("fusermount.zig");
const errors = @import("../core/errors.zig");
const clock = @import("../core/clock.zig");
const tick_law = @import("../sync/tick.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/umount is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const BUSY_RETRY_NS: u64 = tick_law.tick_ns;

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

pub fn run(p: cli.Process, u: args.Umount) u8 {
    var zbuf: [linux.PATH_MAX:0]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{u.mountpoint}, 0) catch {
        p.stderr.writeAll("squeezefs umount: the mountpoint path is too long\n") catch {};
        return cli.exit_usage;
    };
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

const testing = std.testing;

test "umount of a directory that is not a mountpoint reports the kernel's EINVAL through the ONE table" {
    if (linux.geteuid() != 0) return error.SkipZigTest;
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64:0]u8 = undefined;
    const dir = try std.fmt.bufPrintSentinel(&pb, "/build/tmp/not-a-mount-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir(dir.ptr, 0o755);
    defer _ = linux.rmdir(dir.ptr);
    const e = umountAsRoot(dir);
    try testing.expectEqual(linux.E.INVAL, e);
    try testing.expectEqual(error.InvalidArgument, errors.fromErrno(e));
}
