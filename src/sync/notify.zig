//! Notify with permit + epoch and synchronous enable-then-recheck (charter
//! §9; the reference's `sqz_notify.rs`).
//!
//! * `notifyOne` stores a PERMIT if no waiter is registered, else wakes
//!   exactly one waiter (FIFO) and stores the permit it will consume. A
//!   permit satisfies the next waiter immediately, so the send-then-wait
//!   race is safe; permits never accumulate past one (tokio parity).
//! * `notifyWaiters` bumps the EPOCH and wakes every currently registered
//!   waiter; it never stores a permit (a later waiter parks).
//! * A waiter REGISTERS FIRST (`enable`), then re-checks its own condition,
//!   then parks (`wait`) — the todo-24 law: the registration and the epoch
//!   snapshot happen under the same lock every notifier mutates under, so a
//!   notify landing between the re-check and the park is never lost. The
//!   retired shape registered at first poll: a `notify_waiters` in the
//!   window bumped the epoch BEFORE the registration and the waiter parked
//!   forever — unhealable by the tick, which re-contends only the inner
//!   epoch-gated wait, never the caller's outer condition (the
//!   meta-slot-migration wedge, ~50 % of a preflight suite parked).
//!   `notify_model.zig` proves both the late epoch read and the
//!   recheck-before-enable idiom fail.
//! * Parks are ticked (`tick.zig`); a tick whose re-check finds the waiter
//!   satisfied with no token delivered counts `channel_ticked_reregisters`
//!   (a rescued lost wake); an idle waiter's ticks count nothing.
//!
//! The waiter node is the `Notified` value itself and lives on the caller's
//! frame: it must not move between `enable` and `wait`/`cancel`. One
//! `Notified` serves one wait.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const Io = std.Io;
const assert = std.debug.assert;
const tick = @import("tick.zig");
const clock = @import("../core/clock.zig");

pub const Notify = struct {
    interior: Io.Mutex = .init,
    /// One stored `notifyOne` permit.
    permit: bool = false,
    /// Bumped by `notifyWaiters`: a waiter registered under an older epoch
    /// completes on its next check.
    epoch: u64 = 0,
    head: ?*Notified = null,
    tail: ?*Notified = null,
    len: u32 = 0,

    pub const init: Notify = .{};

    pub const Notified = struct {
        notify: *Notify,
        next: ?*Notified = null,
        prev: ?*Notified = null,
        linked: bool = false,
        /// Enabled at least once (the epoch snapshot is valid).
        registered: bool = false,
        registered_epoch: u64 = 0,
        state: std.atomic.Value(u32) = .init(0),

        /// Register NOW without parking. Returns `true` when a permit or a
        /// passed epoch already satisfies the waiter (the caller may skip the
        /// wait; `wait` then returns at once and consumes). Idempotent.
        pub fn enable(n: *Notified, io: Io) bool {
            const s = n.notify;
            s.interior.lockUncancelable(io);
            defer s.interior.unlock(io);
            if (n.registered) return s.permit or s.epoch > n.registered_epoch;
            // A stored permit satisfies immediately; consumption happens at
            // completion (`wait`), so enabling stays side-effect free.
            if (s.permit) return true;
            s.linkLocked(n);
            return false;
        }

        /// Park until a permit or an epoch bump satisfies this waiter.
        pub fn wait(n: *Notified, io: Io) Io.Cancelable!void {
            return n.waitInner(io, true);
        }

        pub fn waitUncancelable(n: *Notified, io: Io) void {
            n.waitInner(io, false) catch |err| switch (err) {
                error.Canceled => unreachable,
            };
        }

        /// Unregister without consuming anything (the dropped-future shape).
        /// A permit that `notifyOne` stored for this waiter stays for the
        /// next arrival.
        pub fn cancel(n: *Notified, io: Io) void {
            const s = n.notify;
            s.interior.lockUncancelable(io);
            defer s.interior.unlock(io);
            if (n.linked) s.unlinkLocked(n);
        }

        pub const WaitOutcome = enum {
            /// A permit or an epoch bump satisfied the waiter (consumed, as `wait`).
            notified,
            /// The monotonic deadline passed first: the waiter is unregistered
            /// and a permit stored for it stays for the next arrival (the
            /// dropped-future shape of `cancel`).
            deadline,
        };

        /// `waitUncancelable` bounded by a monotonic deadline — the cadence
        /// task's `timeout_at(next_tick, wake.notified())`. The deadline is
        /// judged under the interior lock, so a notify landing before it is
        /// never reported as a timeout.
        pub fn waitUntilUncancelable(n: *Notified, io: Io, deadline_ns: u64) WaitOutcome {
            const s = n.notify;
            var ticked = false;
            while (true) {
                {
                    s.interior.lockUncancelable(io);
                    defer s.interior.unlock(io);
                    if (n.registered and s.epoch > n.registered_epoch) {
                        tick.noteChannelRescue(ticked, &n.state);
                        if (n.linked) s.unlinkLocked(n);
                        return .notified;
                    }
                    if (s.permit) {
                        tick.noteChannelRescue(ticked, &n.state);
                        s.permit = false;
                        if (n.linked) s.unlinkLocked(n);
                        return .notified;
                    }
                    if (clock.monotonicNs() >= deadline_ns) {
                        if (n.linked) s.unlinkLocked(n);
                        return .deadline;
                    }
                    if (!n.linked) s.linkLocked(n);
                    n.state.store(0, .monotonic);
                }
                ticked = tick.parkUntilUncancelable(io, &n.state, deadline_ns, null) == .ticked;
            }
        }

        fn waitInner(n: *Notified, io: Io, comptime cancelable: bool) Io.Cancelable!void {
            const s = n.notify;
            var ticked = false;
            while (true) {
                {
                    s.interior.lockUncancelable(io);
                    defer s.interior.unlock(io);
                    // A notifyWaiters since registration completes this waiter.
                    if (n.registered and s.epoch > n.registered_epoch) {
                        tick.noteChannelRescue(ticked, &n.state);
                        if (n.linked) s.unlinkLocked(n);
                        return;
                    }
                    // A stored permit serves the front-most claimant — or a
                    // fresh arrival when nobody is registered (send-then-wait).
                    if (s.permit) {
                        tick.noteChannelRescue(ticked, &n.state);
                        s.permit = false;
                        if (n.linked) s.unlinkLocked(n);
                        return;
                    }
                    // Woken by notifyOne (unlinked there) but the permit went
                    // to a faster claimant: re-register at the BACK — the
                    // bounded-barging law, re-contention rather than a wedge.
                    if (!n.linked) s.linkLocked(n);
                    n.state.store(0, .monotonic);
                }
                if (cancelable) {
                    ticked = (tick.park(io, &n.state, null) catch |err| switch (err) {
                        error.Canceled => {
                            n.cancel(io);
                            return err;
                        },
                    }) == .ticked;
                } else {
                    ticked = tick.parkUncancelable(io, &n.state, null) == .ticked;
                }
            }
        }
    };

    /// An inert waiter; `enable` (or `wait`) registers it.
    pub fn notified(self: *Notify) Notified {
        return .{ .notify = self };
    }

    pub fn notifyOne(self: *Notify, io: Io) void {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        self.permit = true;
        // FIFO wake; the waiter consumes the permit on its next check. It
        // leaves the queue here so a second notifyOne reaches the NEXT
        // waiter instead of re-waking this one.
        if (self.head) |w| {
            self.unlinkLocked(w);
            tick.wake(io, &w.state);
        }
    }

    pub fn notifyWaiters(self: *Notify, io: Io) void {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        self.epoch += 1;
        while (self.head) |w| {
            self.unlinkLocked(w);
            tick.wake(io, &w.state);
        }
    }

    /// Test seam: a `notifyWaiters` whose wake tokens are DROPPED — the
    /// lost-wake shape the tick absorbs and `channel_ticked_reregisters`
    /// counts (the waiters stay parked until their tick's re-check).
    pub fn notifyWaitersDroppingWakeForTest(self: *Notify, io: Io) void {
        assert(@import("builtin").is_test);
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        self.epoch += 1;
        while (self.head) |w| self.unlinkLocked(w);
    }

    /// Registered waiters (the deterministic park observable).
    pub fn waiters(self: *Notify, io: Io) u32 {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        return self.len;
    }

    fn linkLocked(self: *Notify, n: *Notified) void {
        assert(!n.linked);
        n.next = null;
        n.prev = self.tail;
        if (self.tail) |t| t.next = n else self.head = n;
        self.tail = n;
        n.linked = true;
        n.registered = true;
        n.registered_epoch = self.epoch;
        self.len += 1;
    }

    fn unlinkLocked(self: *Notify, n: *Notified) void {
        assert(n.linked);
        if (n.prev) |p| p.next = n.next else self.head = n.next;
        if (n.next) |x| x.prev = n.prev else self.tail = n.prev;
        n.next = null;
        n.prev = null;
        n.linked = false;
        self.len -= 1;
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;
const sync_testing = @import("testing.zig");

fn permitSatisfiesLateWaiter(io: Io) !void {
    var n: Notify = .init;
    n.notifyOne(io);
    var w = n.notified();
    try testing.expect(w.enable(io));
    try w.wait(io);
    try testing.expect(!n.permit);
    try testing.expectEqual(@as(u32, 0), n.waiters(io));
}

test "permit_satisfies_late_waiter: a notifyOne before the waiter registers serves it" {
    try sync_testing.underBoth(permitSatisfiesLateWaiter);
}

fn notifyOneWakesParked(io: Io) !void {
    var n: Notify = .init;
    const Parked = struct {
        fn run(io_: Io, nt: *Notify) void {
            var w = nt.notified();
            w.waitUncancelable(io_);
        }
    };
    const t = try std.Thread.spawn(.{}, Parked.run, .{ io, &n });
    while (n.waiters(io) == 0) try std.Thread.yield();
    n.notifyOne(io);
    t.join();
    try testing.expect(!n.permit);
}

test "notify_one_wakes_a_parked_waiter" {
    try sync_testing.underBoth(notifyOneWakesParked);
}

fn enabledWaiterNeverLoses(io: Io) !void {
    var n: Notify = .init;
    var w = n.notified();
    try testing.expect(!w.enable(io));
    n.notifyWaiters(io);
    try testing.expect(w.enable(io));
    try w.wait(io);

    var n2: Notify = .init;
    var w2 = n2.notified();
    try testing.expect(!w2.enable(io));
    n2.notifyOne(io);
    try w2.wait(io);
    try testing.expect(!n2.permit);
}

test "enabled_waiter_never_loses_a_pre_await_notify: a notifyWaiters or notifyOne between enable and wait completes the waiter" {
    try sync_testing.underBoth(enabledWaiterNeverLoses);
}

fn notifyWaitersRegisteredOnly(io: Io) !void {
    var n: Notify = .init;
    const Parked = struct {
        fn run(io_: Io, nt: *Notify) void {
            var w = nt.notified();
            w.waitUncancelable(io_);
        }
    };
    const t = try std.Thread.spawn(.{}, Parked.run, .{ io, &n });
    while (n.waiters(io) == 0) try std.Thread.yield();
    n.notifyWaiters(io);
    t.join();
    var late = n.notified();
    try testing.expect(!late.enable(io));
    try testing.expectEqual(@as(u32, 1), n.waiters(io));
    late.cancel(io);
    try testing.expectEqual(@as(u32, 0), n.waiters(io));
}

test "notify_waiters_releases_registered_only: no permit is stored for a later waiter" {
    try sync_testing.underBoth(notifyWaitersRegisteredOnly);
}

fn fifoPermits(io: Io) !void {
    var n: Notify = .init;
    var a = n.notified();
    var b = n.notified();
    _ = a.enable(io);
    _ = b.enable(io);
    n.notifyOne(io);
    try testing.expectEqual(@as(u32, 1), a.state.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), b.state.load(.monotonic));
    try testing.expectEqual(@as(u32, 1), n.waiters(io));
    n.notifyOne(io);
    try testing.expectEqual(@as(u32, 1), b.state.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), n.waiters(io));
    // One permit for two woken waiters: the first consumes it, the second
    // re-registers at the back (bounded barging) and parks.
    try a.wait(io);
    try testing.expect(!n.permit);
    try testing.expect(!b.enable(io));
    b.cancel(io);
    try testing.expectEqual(@as(u32, 0), n.waiters(io));
}

test "two notifyOne calls reach two waiters in FIFO order; a permit never accumulates past one" {
    try sync_testing.underBoth(fifoPermits);
}

fn cancelledWaitAndStorm(io: Io) !void {
    var n: Notify = .init;
    const Parked = struct {
        fn run(io_: Io, nt: *Notify) Io.Cancelable!void {
            var w = nt.notified();
            try w.wait(io_);
        }
    };
    var fut = io.concurrent(Parked.run, .{ io, &n }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency"),
    };
    while (n.waiters(io) == 0) try std.Thread.yield();
    try testing.expectEqual(error.Canceled, fut.cancel(io));
    try testing.expectEqual(@as(u32, 0), n.waiters(io));

    const Shared = struct {
        n: Notify = .init,
        ready: std.atomic.Value(u32) = .init(0),
        const rounds = 2000;
        fn producer(io_: Io, s: *@This()) void {
            var i: u32 = 1;
            while (i <= rounds) : (i += 1) {
                while (s.ready.load(.acquire) != 0) std.atomic.spinLoopHint();
                s.ready.store(i, .release);
                s.n.notifyOne(io_);
            }
        }
        fn consumer(io_: Io, s: *@This()) void {
            var i: u32 = 1;
            while (i <= rounds) : (i += 1) {
                while (true) {
                    var w = s.n.notified();
                    _ = w.enable(io_);
                    if (s.ready.load(.acquire) == i) {
                        w.cancel(io_);
                        break;
                    }
                    w.waitUncancelable(io_);
                }
                s.ready.store(0, .release);
            }
        }
    };
    var s: Shared = .{};
    const p = try std.Thread.spawn(.{}, Shared.producer, .{ io, &s });
    const c = try std.Thread.spawn(.{}, Shared.consumer, .{ io, &s });
    p.join();
    c.join();
    try testing.expectEqual(@as(u32, 0), s.n.waiters(io));
}

test "a cancelled parked wait unregisters; the enable-then-recheck idiom over a shared flag under a storm loses nothing" {
    try sync_testing.underBoth(cancelledWaitAndStorm);
}

fn waitUntilBody(io: Io) !void {
    var n: Notify = .init;
    var w = n.notified();
    try testing.expectEqual(Notify.Notified.WaitOutcome.deadline, w.waitUntilUncancelable(io, 0));
    try testing.expectEqual(@as(u32, 0), n.waiters(io));
    // A permit stored after the timeout serves the next wait at once.
    n.notifyOne(io);
    var w2 = n.notified();
    try testing.expectEqual(Notify.Notified.WaitOutcome.notified, w2.waitUntilUncancelable(io, 0));
    try testing.expect(!n.permit);
    // An epoch bump between enable and the wait completes it despite a passed deadline.
    var w3 = n.notified();
    try testing.expect(!w3.enable(io));
    n.notifyWaiters(io);
    try testing.expectEqual(Notify.Notified.WaitOutcome.notified, w3.waitUntilUncancelable(io, 0));
    // A far deadline parks; a notify from another thread ends it.
    const Parked = struct {
        fn run(io_: Io, nt: *Notify, out: *Notify.Notified.WaitOutcome) void {
            var pw = nt.notified();
            out.* = pw.waitUntilUncancelable(io_, std.math.maxInt(u64));
        }
    };
    var out: Notify.Notified.WaitOutcome = .deadline;
    const t = try std.Thread.spawn(.{}, Parked.run, .{ io, &n, &out });
    while (n.waiters(io) == 0) try std.Thread.yield();
    n.notifyOne(io);
    t.join();
    try testing.expectEqual(Notify.Notified.WaitOutcome.notified, out);
    try testing.expect(!n.permit);
}

test "waitUntil: a passed deadline unregisters and keeps a later permit; a stored permit or epoch wins over the deadline; a cross-thread notify ends a far-deadline wait" {
    try sync_testing.underBoth(waitUntilBody);
}

test "the tick accounting: an idle waiter parked past a tick and then notified counts NOTHING (the reclaimer's idle shape — the phantom 'lost wakes'); a notify whose wake was lost is rescued by the tick and counts exactly one channel_ticked_reregisters" {
    const io = testing.io;
    var n: Notify = .init;
    const Parked = struct {
        fn run(io_: Io, nt: *Notify) void {
            var w = nt.notified();
            w.waitUncancelable(io_);
        }
    };
    const before = tick.channel_ticked_reregisters.load();
    // 1. Idle past one tick, then a real notify: no rescue.
    const t1 = try std.Thread.spawn(.{}, Parked.run, .{ io, &n });
    while (n.waiters(io) == 0) try std.Thread.yield();
    io.sleep(.fromNanoseconds(@intCast(tick.tick_ns + tick.tick_ns / 4)), .awake) catch unreachable;
    n.notifyWaiters(io);
    t1.join();
    try testing.expectEqual(before, tick.channel_ticked_reregisters.load());
    // 2. The epoch bumps but the token never lands: the waiter's tick
    //    finds it satisfied with no token — one rescue, counted.
    const t2 = try std.Thread.spawn(.{}, Parked.run, .{ io, &n });
    while (n.waiters(io) == 0) try std.Thread.yield();
    const t0 = clock.monotonicNs();
    n.notifyWaitersDroppingWakeForTest(io);
    t2.join();
    try testing.expect(clock.monotonicNs() - t0 >= tick.tick_ns - tick.tick_ns / 10);
    try testing.expectEqual(before + 1, tick.channel_ticked_reregisters.load());
    try testing.expectEqual(@as(u32, 0), n.waiters(io));
}
