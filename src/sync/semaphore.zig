//! The async semaphore under the bounded-barging law (charter §9; the
//! reference's `sqz_semaphore.rs`).
//!
//! A release never assigns permits to a sleeping waiter — it returns them
//! to the pool and wakes the FIFO front if the pool could now serve it;
//! woken waiters re-contend, so a dead waiter owns nothing and wedges
//! nothing (its corpse costs contenders one tick). A FRESH `acquire` does
//! not barge past queued waiters (FIFO courtesy); a QUEUED waiter's
//! re-contend does (the dead-front tick rail). `tryAcquire`'s contract IS
//! the barge (tokio parity: it takes free permits regardless of the queue).
//! Parks are ticked (`tick.zig`; a tick that finds the permits there with no
//! token delivered counts `channel_ticked_reregisters`); waiter nodes live
//! on the acquirer's frame.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const Io = std.Io;
const assert = std.debug.assert;
const tick = @import("tick.zig");

pub const Semaphore = struct {
    interior: Io.Mutex = .init,
    permits: usize,
    head: ?*Waiter = null,
    tail: ?*Waiter = null,
    len: u32 = 0,

    const Waiter = struct {
        want: usize,
        next: ?*Waiter = null,
        prev: ?*Waiter = null,
        linked: bool = false,
        state: std.atomic.Value(u32) = .init(0),
    };

    pub fn init(permits: usize) Semaphore {
        return .{ .permits = permits };
    }

    /// A held allotment, movable by value: `release` returns it, `forget`
    /// consumes it without returning (the sync-coalescer gate pattern).
    pub const Permit = struct {
        sem: *Semaphore,
        n: usize,

        pub fn release(p: Permit, io: Io) void {
            p.sem.release(io, p.n);
        }

        pub fn forget(p: *Permit) void {
            p.n = 0;
        }
    };

    pub fn available(self: *Semaphore, io: Io) usize {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        return self.permits;
    }

    /// Parked acquirers (the deterministic park observable).
    pub fn waiters(self: *Semaphore, io: Io) u32 {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        return self.len;
    }

    /// Return `n` permits (also `add_permits`): the pool grows and the FIFO
    /// front is woken if it could now be served — a token, never ownership.
    pub fn release(self: *Semaphore, io: Io, n: usize) void {
        if (n == 0) return;
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        self.permits += n;
        self.wakeFrontIfServableLocked(io);
    }

    /// Immediate grab — barges past queued waiters by contract.
    pub fn tryAcquire(self: *Semaphore, io: Io, n: usize) ?Permit {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        if (self.permits < n) return null;
        self.permits -= n;
        return .{ .sem = self, .n = n };
    }

    pub fn acquire(self: *Semaphore, io: Io, n: usize) Io.Cancelable!Permit {
        try self.acquireUnits(io, n, true);
        return .{ .sem = self, .n = n };
    }

    pub fn acquireUncancelable(self: *Semaphore, io: Io, n: usize) Permit {
        self.acquireUnits(io, n, false) catch |err| switch (err) {
            error.Canceled => unreachable,
        };
        return .{ .sem = self, .n = n };
    }

    /// Grant-or-enqueue under ONE interior hold, then the ticked re-contend
    /// loop (the `acquire_ticked` shape).
    fn acquireUnits(self: *Semaphore, io: Io, n: usize, comptime cancelable: bool) Io.Cancelable!void {
        var w: Waiter = .{ .want = n };
        {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            // Fresh: free permits AND nobody queued (FIFO courtesy).
            if (self.len == 0 and self.permits >= n) {
                self.permits -= n;
                return;
            }
            self.pushLocked(&w);
        }
        while (true) {
            const outcome = if (cancelable) (tick.park(io, &w.state, null) catch |err| switch (err) {
                error.Canceled => {
                    self.unregister(io, &w);
                    return err;
                },
            }) else tick.parkUncancelable(io, &w.state, null);
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            // Queued: re-contend regardless of position (the dead-front rail).
            if (self.permits >= n) tick.noteChannelRescue(outcome == .ticked, &w.state);
            w.state.store(0, .monotonic);
            if (self.permits >= n) {
                self.permits -= n;
                self.unlinkLocked(&w);
                // Leftover permits may serve the next front too.
                self.wakeFrontIfServableLocked(io);
                return;
            }
        }
    }

    /// A cancelled waiter leaves; its departure may unblock the new front.
    fn unregister(self: *Semaphore, io: Io, w: *Waiter) void {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        self.unlinkLocked(w);
        self.wakeFrontIfServableLocked(io);
    }

    fn wakeFrontIfServableLocked(self: *Semaphore, io: Io) void {
        const front = self.head orelse return;
        if (front.want <= self.permits) tick.wake(io, &front.state);
    }

    fn pushLocked(self: *Semaphore, w: *Waiter) void {
        assert(!w.linked);
        w.next = null;
        w.prev = self.tail;
        if (self.tail) |t| t.next = w else self.head = w;
        self.tail = w;
        w.linked = true;
        self.len += 1;
    }

    fn unlinkLocked(self: *Semaphore, w: *Waiter) void {
        assert(w.linked);
        if (w.prev) |p| p.next = w.next else self.head = w.next;
        if (w.next) |x| x.prev = w.prev else self.tail = w.prev;
        w.next = null;
        w.prev = null;
        w.linked = false;
        self.len -= 1;
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;
const sync_testing = @import("testing.zig");

fn permitsBound(io: Io) !void {
    var s = Semaphore.init(2);
    const p1 = s.tryAcquire(io, 1).?;
    const p2 = s.tryAcquire(io, 1).?;
    try testing.expect(s.tryAcquire(io, 1) == null);
    p1.release(io);
    try testing.expectEqual(@as(usize, 1), s.available(io));
    const p3 = s.tryAcquire(io, 1).?;
    p2.release(io);
    p3.release(io);
    try testing.expectEqual(@as(usize, 2), s.available(io));
}

test "permits_bound_and_release_restores" {
    try sync_testing.underBoth(permitsBound);
}

fn forgetLeaks(io: Io) !void {
    var s = Semaphore.init(1);
    var p = s.tryAcquire(io, 1).?;
    p.forget();
    p.release(io);
    try testing.expectEqual(@as(usize, 0), s.available(io));
    s.release(io, 1);
    try testing.expectEqual(@as(usize, 1), s.available(io));
}

test "forget_leaks_the_permit_by_contract" {
    try sync_testing.underBoth(forgetLeaks);
}

fn parkedAcquireWakes(io: Io) !void {
    var s = Semaphore.init(1);
    const held = s.tryAcquire(io, 1).?;
    const Parked = struct {
        fn run(io_: Io, sem: *Semaphore) void {
            const p = sem.acquireUncancelable(io_, 1);
            p.release(io_);
        }
    };
    const t = try std.Thread.spawn(.{}, Parked.run, .{ io, &s });
    while (s.waiters(io) == 0) try std.Thread.yield();
    held.release(io);
    t.join();
    try testing.expectEqual(@as(usize, 1), s.available(io));
    try testing.expectEqual(@as(u32, 0), s.waiters(io));
}

test "parked_acquire_wakes_on_release_fifo" {
    try sync_testing.underBoth(parkedAcquireWakes);
}

fn freshQueuesTryBarges(io: Io) !void {
    var s = Semaphore.init(1);
    const held = s.tryAcquire(io, 1).?;
    const Parked = struct {
        fn run(io_: Io, sem: *Semaphore) void {
            const p = sem.acquireUncancelable(io_, 1);
            p.release(io_);
        }
    };
    const t = try std.Thread.spawn(.{}, Parked.run, .{ io, &s });
    while (s.waiters(io) == 0) try std.Thread.yield();
    // The pool is empty: even the barging try refuses.
    try testing.expect(s.tryAcquire(io, 1) == null);
    held.release(io);
    t.join();
    // A free pool with a queued waiter: a fresh acquire queues, a try takes.
    const Dead = struct {
        fn run(io_: Io, sem: *Semaphore) Io.Cancelable!void {
            const p = try sem.acquire(io_, 1);
            p.release(io_);
        }
    };
    const big = s.tryAcquire(io, 1).?;
    var fut = io.concurrent(Dead.run, .{ io, &s }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency"),
    };
    while (s.waiters(io) == 0) try std.Thread.yield();
    big.release(io);
    // The token is in flight to the parked waiter; a try barges past it.
    if (s.tryAcquire(io, 1)) |p| p.release(io);
    try fut.await(io);
    try testing.expectEqual(@as(usize, 1), s.available(io));
}

test "fresh_acquire_queues_behind_waiter_but_try_barges" {
    try sync_testing.underBoth(freshQueuesTryBarges);
}

fn cancelledAcquireUnlinks(io: Io) !void {
    var s = Semaphore.init(1);
    const held = s.tryAcquire(io, 1).?;
    const Parked = struct {
        fn run(io_: Io, sem: *Semaphore) Io.Cancelable!void {
            const p = try sem.acquire(io_, 1);
            p.release(io_);
        }
    };
    var fut = io.concurrent(Parked.run, .{ io, &s }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency"),
    };
    while (s.waiters(io) == 0) try std.Thread.yield();
    try testing.expectEqual(error.Canceled, fut.cancel(io));
    try testing.expectEqual(@as(u32, 0), s.waiters(io));
    held.release(io);
    try testing.expectEqual(@as(usize, 1), s.available(io));
}

test "a cancelled parked acquire unlinks and hands the pool to the next waiter" {
    try sync_testing.underBoth(cancelledAcquireUnlinks);
}

fn concurrencyNeverExceeds(io: Io) !void {
    const Shared = struct {
        s: Semaphore = Semaphore.init(2),
        inside: std.atomic.Value(u32) = .init(0),
        peak: std.atomic.Value(u32) = .init(0),
        fn run(io_: Io, sh: *@This()) void {
            var i: usize = 0;
            while (i < 300) : (i += 1) {
                const p = sh.s.acquireUncancelable(io_, 1);
                const now = sh.inside.fetchAdd(1, .acq_rel) + 1;
                _ = sh.peak.fetchMax(now, .acq_rel);
                std.Thread.yield() catch {};
                _ = sh.inside.fetchSub(1, .acq_rel);
                p.release(io_);
            }
        }
    };
    var sh: Shared = .{};
    var ts: [6]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, Shared.run, .{ io, &sh });
    for (ts) |t| t.join();
    try testing.expect(sh.peak.load(.acquire) <= 2);
    try testing.expect(sh.peak.load(.acquire) >= 1);
    try testing.expectEqual(@as(usize, 2), sh.s.available(io));
}

test "concurrency never exceeds the permit count under a storm" {
    try sync_testing.underBoth(concurrencyNeverExceeds);
}
