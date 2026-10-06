//! fd readiness for lane-resident tasks (charter §9, amended 2026-10-06:
//! "`POLL_ADD` one-shot per await" on the lane's own ring; decision 0003 §D
//! Q-B; the reference's `sqz_fdwatch` contract L14–24 with its epoll thread
//! and cross-thread wake deleted — 0002 ledger).
//!
//! `readable(fd)` on a lane fiber arms ONE `POLL_ADD(POLLIN)` SQE with the
//! fiber as `user_data`, parks, and returns `.ready` at the CQE: level-
//! triggered per await, so "drain to EAGAIN, then re-await" sites port 1:1.
//! One awaiter per fd per lane: a second registration displaces the first
//! (its SQE is cancelled by the lane; it completes `.replaced`) and fires the
//! `exec_fdwatch_double_await` tripwire — a caller bug made loud, not a hang.
//! Off a lane: `poll(2)` on the calling thread (a blocking context has no
//! ring).
//!
//! Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const errors = @import("../core/errors.zig");
const fiber_mod = @import("fiber.zig");
const lane_mod = @import("lane.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("exec/fdwatch is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Fiber = fiber_mod.Fiber;

pub const Readiness = enum {
    /// Readable (or hung up / in error: the caller's read tells which).
    ready,
    /// A later awaiter on the same fd displaced this one.
    replaced,
};

pub const Error = error{ Canceled, SubmissionQueueOverflow, RingFailed, Unexpected };

/// Wait until `fd` is readable.
pub fn readable(fd: linux.fd_t) Error!Readiness {
    const f = lane_mod.currentFiber() orelse return readableBlocking(fd);
    const lane = f.lane;
    // One awaiter per fd per lane: displace a standing one. It is parked on
    // its POLL_ADD (only one fiber runs on a lane), so the lane's
    // ASYNC_CANCEL completes it `-ECANCELED` and it reports `.replaced` —
    // no cancel request on the fiber itself.
    for (lane.fibers) |*other| {
        if (other == f or other.watched_fd != fd) continue;
        stats.fdwatch_double_await.fire("a second readable() awaiter on one fd on one lane");
        other.watch_replaced = true;
        other.watched_fd = -1;
        lane.postCancelOp(other);
    }
    try f.checkCancel();
    if (!lane.armRingOp(f, linux.io_uring_sqe.prep_poll_add, .{ fd, @as(u32, linux.POLL.IN) })) return if (lane.ring.isDead()) error.RingFailed else error.SubmissionQueueOverflow;
    f.watched_fd = fd;
    f.watch_replaced = false;
    _ = f.changeAwaiting(.nothing, .ring_op);
    while (f.op_pending) f.park();
    f.watched_fd = -1;
    const requested = f.changeAwaiting(.ring_op, .nothing);
    if (f.watch_replaced) {
        f.watch_replaced = false;
        return .replaced;
    }
    if (f.cqe_res < 0) {
        const e = errors.fromCqeRes(f.cqe_res);
        if (e == error.Canceled and requested and f.protection.check() == .unblocked) {
            f.protection.acknowledged = true;
            return error.Canceled;
        }
        return switch (e) {
            error.RingFailed => error.RingFailed,
            // A cancelled SQE whose request was already acknowledged, or a
            // kernel error: the caller's next read reports the real state.
            else => .ready,
        };
    }
    return .ready;
}

fn readableBlocking(fd: linux.fd_t) Error!Readiness {
    var pfd = [_]linux.pollfd{.{ .fd = fd, .events = linux.POLL.IN, .revents = 0 }};
    while (true) {
        const rc = linux.poll(&pfd, 1, -1);
        switch (linux.errno(rc)) {
            .SUCCESS => return .ready,
            .INTR => continue,
            else => return error.Unexpected,
        }
    }
}

const testing = std.testing;

test "off a lane, readable() is poll(2): a written eventfd is ready" {
    const efd_rc = linux.eventfd(0, linux.EFD.CLOEXEC | linux.EFD.NONBLOCK);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(efd_rc));
    const efd: linux.fd_t = @intCast(efd_rc);
    defer _ = linux.close(efd);
    const one: u64 = 1;
    _ = linux.write(efd, @ptrCast(&one), 8);
    try testing.expectEqual(Readiness.ready, try readable(efd));
}
