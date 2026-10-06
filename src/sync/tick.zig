//! The 2 s ticked park — the ONE law every unbounded wait in `src/sync/`
//! rides (charter §9; `docs/decisions/0001` item 3; the reference's
//! `sqz_sync::TICK` / `sqz_channel::ticked`).
//!
//! A waiter parks on its own futex word for at most one TICK. A wake flips
//! the word and `futexWake`s it; a wait that outlives the tick returns
//! `.ticked`, counts a re-register, and the caller re-contends with its
//! queue position intact. Timer wakes ride a different delivery path from
//! the waker handoff, so a waiter whose own wake was lost self-heals one
//! tick later instead of wedging — the OQ-5 lost-wakeup class made
//! unrepresentable. The counters are 0 on a healthy daemon; growth is a
//! lost wake absorbed, loudly.
//!
//! The tick is unreachable by healthy waits (metadata holds are never
//! device-I/O-length) and cheap enough that a lost wake costs one beat.

const std = @import("std");
const Io = std.Io;
const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const futex = @import("futex.zig");

/// The liveness tick: the backstop's re-contend period.
pub const tick_ns: u64 = 2 * std.time.ns_per_s;

/// `lock_ticked_reregisters`: mutex/rwlock acquires that re-contended past
/// a tick (the reference's `TICK_RECOVERIES`).
pub var lock_ticked_reregisters: counters.ShardedCounter = .{};

/// `channel_ticked_reregisters`: every other primitive's ticked park
/// (notify, semaphore, single-flight, once-cell, channels — the reference's
/// `TICKED_WAIT_RECOVERIES`).
pub var channel_ticked_reregisters: counters.ShardedCounter = .{};

pub const Outcome = enum {
    /// The word left 0: a wake token was delivered.
    woken,
    /// One full tick elapsed with the word still 0: re-register and re-contend.
    ticked,
    /// The caller's own deadline (`parkUntil`) passed first with the word still 0.
    deadline,
};

/// Park while `word` reads 0, for at most one tick. Spurious returns
/// (EINTR, a futex refresh) re-park for the remainder; only a wait that
/// reaches the deadline counts against `counter`. The futex compare is what
/// closes the window between the caller's registration and the sleep: a
/// token stored before the sleep makes the wait return at once.
pub fn park(io: Io, word: *std.atomic.Value(u32), counter: *counters.ShardedCounter) Io.Cancelable!Outcome {
    return parkUntil(io, word, std.math.maxInt(u64), counter);
}

/// `park` bounded by the caller's own monotonic deadline as well: returns
/// `.deadline` when `deadline_ns` passes before a token or the tick (the
/// cadence task's `timeout_at(next_tick, notified())`). The tick stays the
/// liveness backstop underneath and still counts.
pub fn parkUntil(io: Io, word: *std.atomic.Value(u32), deadline_ns: u64, counter: *counters.ShardedCounter) Io.Cancelable!Outcome {
    const tick_deadline = clock.monotonicNs() + tick_ns;
    const until = @min(tick_deadline, deadline_ns);
    while (true) {
        // acquire pairs with the waker's release swap: a woken caller sees
        // every write the waker made before handing over the token.
        if (word.load(.acquire) != 0) return .woken;
        const now = clock.monotonicNs();
        if (now >= until) {
            if (deadline_ns <= tick_deadline) return .deadline;
            counter.inc();
            return .ticked;
        }
        try futex.ioWaitTimeout(io, word, 0, until - now);
    }
}

/// `parkUntil` without a cancellation point (see `parkUncancelable`).
pub fn parkUntilUncancelable(io: Io, word: *std.atomic.Value(u32), deadline_ns: u64, counter: *counters.ShardedCounter) Outcome {
    const old = io.swapCancelProtection(.blocked);
    defer _ = io.swapCancelProtection(old);
    return parkUntil(io, word, deadline_ns, counter) catch |err| switch (err) {
        error.Canceled => unreachable,
    };
}

/// `park` without a cancellation point (committers are never cancelled —
/// charter §9). The vtable has no timed uncancelable wait, so the timed wait
/// runs under blocked cancel protection, which is the documented idiom.
pub fn parkUncancelable(io: Io, word: *std.atomic.Value(u32), counter: *counters.ShardedCounter) Outcome {
    const old = io.swapCancelProtection(.blocked);
    defer _ = io.swapCancelProtection(old);
    return park(io, word, counter) catch |err| switch (err) {
        error.Canceled => unreachable,
    };
}

/// Deliver a wake token: flip the word and wake its futex. The swap is a
/// release so the waiter's acquire load in `park` sees the waker's prior
/// writes; a word already holding a token needs no second syscall (the
/// waiter has not consumed the first one yet). The caller must hold the
/// node alive across this call (every caller does it under the primitive's
/// interior mutex, which the waiter needs before it can unlink).
pub fn wake(io: Io, word: *std.atomic.Value(u32)) void {
    if (word.swap(1, .release) == 0) futex.ioWake(io, word, 1);
}

const testing = std.testing;
const sync_testing = @import("testing.zig");

fn tokenBeforePark(io: Io) !void {
    var word: std.atomic.Value(u32) = .init(0);
    var c: counters.ShardedCounter = .{};
    wake(io, &word);
    try testing.expectEqual(Outcome.woken, try park(io, &word, &c));
    try testing.expectEqual(@as(u64, 0), c.load());
}

test "a token stored before the park returns at once as .woken" {
    try sync_testing.underBoth(tokenBeforePark);
}

fn foreignWake(io: Io) !void {
    var word: std.atomic.Value(u32) = .init(0);
    var c: counters.ShardedCounter = .{};
    const Waker = struct {
        fn run(io_: Io, w: *std.atomic.Value(u32)) void {
            wake(io_, w);
        }
    };
    const t = try std.Thread.spawn(.{}, Waker.run, .{ io, &word });
    try testing.expectEqual(Outcome.woken, try park(io, &word, &c));
    t.join();
    try testing.expectEqual(@as(u64, 0), c.load());
}

test "a wake from another thread ends the park as .woken without a tick" {
    try sync_testing.underBoth(foreignWake);
}

fn uncancelableToken(io: Io) !void {
    var word: std.atomic.Value(u32) = .init(1);
    var c: counters.ShardedCounter = .{};
    try testing.expectEqual(Outcome.woken, parkUncancelable(io, &word, &c));
}

test "the uncancelable park under blocked protection returns .woken on a token" {
    try sync_testing.underBoth(uncancelableToken);
}

fn parkUntilBody(io: Io) !void {
    var word: std.atomic.Value(u32) = .init(0);
    var c: counters.ShardedCounter = .{};
    try testing.expectEqual(Outcome.deadline, parkUntilUncancelable(io, &word, clock.monotonicNs() -| 1, &c));
    try testing.expectEqual(Outcome.deadline, try parkUntil(io, &word, 0, &c));
    try testing.expectEqual(@as(u64, 0), c.load());
    wake(io, &word);
    try testing.expectEqual(Outcome.woken, try parkUntil(io, &word, 0, &c));
    // A deadline past the tick: the wait is the tick's (the futex timeout is
    // bounded by it), so a token delivered from another thread ends it as
    // .woken exactly like `park`.
    word.store(0, .monotonic);
    const Waker = struct {
        fn run(io_: Io, w: *std.atomic.Value(u32)) void {
            wake(io_, w);
        }
    };
    const t = try std.Thread.spawn(.{}, Waker.run, .{ io, &word });
    try testing.expectEqual(Outcome.woken, try parkUntil(io, &word, std.math.maxInt(u64), &c));
    t.join();
    try testing.expectEqual(@as(u64, 0), c.load());
}

test "parkUntil: a passed deadline returns .deadline without counting a tick; a token still wins; a far deadline leaves the tick as the backstop" {
    try sync_testing.underBoth(parkUntilBody);
}

/// The park on a FIBER: the token is delivered by a raw thread while the
/// waiter is parked on its lane (the futex table's fiber path, the lane's
/// timer as the tick bound).
fn fiberPark(io: Io) !void {
    const Parked = struct {
        fn run(io_: Io, w: *std.atomic.Value(u32), c: *counters.ShardedCounter) Io.Cancelable!Outcome {
            return park(io_, w, c);
        }
    };
    var word: std.atomic.Value(u32) = .init(0);
    var c: counters.ShardedCounter = .{};
    var fut = io.concurrent(Parked.run, .{ io, &word, &c }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    // The waiter is parked (or about to be): the token ends it either way.
    std.Thread.yield() catch {};
    wake(io, &word);
    try testing.expectEqual(Outcome.woken, try fut.await(io));
    try testing.expectEqual(@as(u64, 0), c.load());
}

test "a parked concurrent waiter is woken by a raw thread's token without a tick" {
    try sync_testing.underBoth(fiberPark);
}
