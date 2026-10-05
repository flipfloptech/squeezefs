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
//! * Parks are ticked (`tick.zig`, `channel_ticked_reregisters`).
//!
//! The waiter node is the `Notified` value itself and lives on the caller's
//! frame: it must not move between `enable` and `wait`/`cancel`. One
//! `Notified` serves one wait.

const std = @import("std");
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
            while (true) {
                {
                    s.interior.lockUncancelable(io);
                    defer s.interior.unlock(io);
                    if (n.registered and s.epoch > n.registered_epoch) {
                        if (n.linked) s.unlinkLocked(n);
                        return .notified;
                    }
                    if (s.permit) {
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
                _ = tick.parkUntilUncancelable(io, &n.state, deadline_ns, &tick.channel_ticked_reregisters);
            }
        }

        fn waitInner(n: *Notified, io: Io, comptime cancelable: bool) Io.Cancelable!void {
            const s = n.notify;
            while (true) {
                {
                    s.interior.lockUncancelable(io);
                    defer s.interior.unlock(io);
                    // A notifyWaiters since registration completes this waiter.
                    if (n.registered and s.epoch > n.registered_epoch) {
                        if (n.linked) s.unlinkLocked(n);
                        return;
                    }
                    // A stored permit serves the front-most claimant — or a
                    // fresh arrival when nobody is registered (send-then-wait).
                    if (s.permit) {
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
                    _ = tick.park(io, &n.state, &tick.channel_ticked_reregisters) catch |err| switch (err) {
                        error.Canceled => {
                            n.cancel(io);
                            return err;
                        },
                    };
                } else {
                    _ = tick.parkUncancelable(io, &n.state, &tick.channel_ticked_reregisters);
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

test "permit_satisfies_late_waiter: a notifyOne before the waiter registers serves it" {
    const io = testing.io;
    var n: Notify = .init;
    n.notifyOne(io);
    var w = n.notified();
    try testing.expect(w.enable(io));
    try w.wait(io);
    try testing.expect(!n.permit);
    try testing.expectEqual(@as(u32, 0), n.waiters(io));
}

test "notify_one_wakes_a_parked_waiter" {
    const io = testing.io;
    var n: Notify = .init;
    const Parked = struct {
        fn run(nt: *Notify) void {
            var w = nt.notified();
            w.waitUncancelable(testing.io);
        }
    };
    const t = try std.Thread.spawn(.{}, Parked.run, .{&n});
    while (n.waiters(io) == 0) try std.Thread.yield();
    n.notifyOne(io);
    t.join();
    try testing.expect(!n.permit);
}

test "enabled_waiter_never_loses_a_pre_await_notify: a notifyWaiters or notifyOne between enable and wait completes the waiter" {
    const io = testing.io;
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

test "notify_waiters_releases_registered_only: no permit is stored for a later waiter" {
    const io = testing.io;
    var n: Notify = .init;
    const Parked = struct {
        fn run(nt: *Notify) void {
            var w = nt.notified();
            w.waitUncancelable(testing.io);
        }
    };
    const t = try std.Thread.spawn(.{}, Parked.run, .{&n});
    while (n.waiters(io) == 0) try std.Thread.yield();
    n.notifyWaiters(io);
    t.join();
    var late = n.notified();
    try testing.expect(!late.enable(io));
    try testing.expectEqual(@as(u32, 1), n.waiters(io));
    late.cancel(io);
    try testing.expectEqual(@as(u32, 0), n.waiters(io));
}

test "two notifyOne calls reach two waiters in FIFO order; a permit never accumulates past one" {
    const io = testing.io;
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

test "a cancelled parked wait unregisters; the enable-then-recheck idiom over a shared flag under a storm loses nothing" {
    const io = testing.io;
    var n: Notify = .init;
    const Parked = struct {
        fn run(nt: *Notify) Io.Cancelable!void {
            var w = nt.notified();
            try w.wait(testing.io);
        }
    };
    var fut = io.concurrent(Parked.run, .{&n}) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    while (n.waiters(io) == 0) try std.Thread.yield();
    try testing.expectEqual(error.Canceled, fut.cancel(io));
    try testing.expectEqual(@as(u32, 0), n.waiters(io));

    const Shared = struct {
        n: Notify = .init,
        ready: std.atomic.Value(u32) = .init(0),
        const rounds = 2000;
        fn producer(s: *@This()) void {
            var i: u32 = 1;
            while (i <= rounds) : (i += 1) {
                while (s.ready.load(.acquire) != 0) std.atomic.spinLoopHint();
                s.ready.store(i, .release);
                s.n.notifyOne(testing.io);
            }
        }
        fn consumer(s: *@This()) void {
            var i: u32 = 1;
            while (i <= rounds) : (i += 1) {
                while (true) {
                    var w = s.n.notified();
                    _ = w.enable(testing.io);
                    if (s.ready.load(.acquire) == i) {
                        w.cancel(testing.io);
                        break;
                    }
                    w.waitUncancelable(testing.io);
                }
                s.ready.store(0, .release);
            }
        }
    };
    var s: Shared = .{};
    const p = try std.Thread.spawn(.{}, Shared.producer, .{&s});
    const c = try std.Thread.spawn(.{}, Shared.consumer, .{&s});
    p.join();
    c.join();
    try testing.expectEqual(@as(u32, 0), s.n.waiters(io));
}

test "waitUntil: a passed deadline unregisters and keeps a later permit; a stored permit or epoch wins over the deadline; a cross-thread notify ends a far-deadline wait" {
    const io = testing.io;
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
        fn run(nt: *Notify, out: *Notify.Notified.WaitOutcome) void {
            var pw = nt.notified();
            out.* = pw.waitUntilUncancelable(testing.io, std.math.maxInt(u64));
        }
    };
    var out: Notify.Notified.WaitOutcome = .deadline;
    const t = try std.Thread.spawn(.{}, Parked.run, .{ &n, &out });
    while (n.waiters(io) == 0) try std.Thread.yield();
    n.notifyOne(io);
    t.join();
    try testing.expectEqual(Notify.Notified.WaitOutcome.notified, out);
    try testing.expect(!n.permit);
}
