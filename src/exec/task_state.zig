//! The task delivery word (charter §9 "waker-based executor"; the
//! reference's `exec_core::TaskState`, `docs/porting/exec.md` §1.3): ONE
//! atomic byte per fiber that decides, for every wake from every thread, who
//! enqueues the fiber — so a fiber is never queued twice and a wake is never
//! dropped while the fiber runs. It replaces the reference's per-parker
//! sticky flag and std's `Io.Uring` ready-queue CAS dance with one word
//! whose two laws are stated and model-checked (`task_state_model.zig`).
//!
//!     IDLE ──wake──▶ SCHEDULED ──begin_poll──▶ RUNNING ──end_poll_pending──▶ IDLE
//!                                                 │  ▲                      (or SCHEDULED when NOTIFIED)
//!                                            wake │  │
//!                                                 ▼  │
//!                                              NOTIFIED ──end_poll_complete──▶ COMPLETE
//!
//! Law 1: a wake is never dropped while the task runs — a wake that lands
//! mid-poll parks as NOTIFIED and the RUNNER re-enqueues on its way out.
//! Law 2: a task is never queued twice — only `IDLE→SCHEDULED` (the wake that
//! won the CAS) and post-poll `NOTIFIED→SCHEDULED` (the runner, the one party
//! that may leave NOTIFIED) enqueue, and both are single-winner.
//!
//! Portable: a pure state machine; the lane (`lane.zig`) is its only product
//! client.

const std = @import("std");

pub const State = enum(u8) {
    /// Parked (futex, CQE, timer, awaiting): the only state a wake may
    /// enqueue from.
    idle = 0,
    /// In a lane's inbox or ready queue, or being pushed by the wake that won.
    scheduled = 1,
    /// On the CPU.
    running = 2,
    /// Polled AND woken since the poll began; the runner requeues.
    notified = 3,
    /// Terminal; wakes no-op forever.
    complete = 4,
};

pub const TaskState = struct {
    word: std.atomic.Value(u8),

    /// Spawn IS the first wake: a fresh task starts in the queue.
    pub fn newScheduled() TaskState {
        return .{ .word = .init(@backingInt(State.scheduled)) };
    }

    pub fn load(self: *const TaskState) State {
        return @fromBackingInt(self.word.load(.acquire));
    }

    /// A wake from any thread. True ⇔ the caller must enqueue the task:
    /// `IDLE→SCHEDULED`. `RUNNING→NOTIFIED` leaves the enqueue to the runner
    /// (law 1); SCHEDULED/NOTIFIED/COMPLETE are no-ops (law 2). acq_rel on
    /// success pairs with `beginPoll`'s acquire: an enqueue-winning wake
    /// publishes the waker's preceding writes (the CQE result, the futex
    /// token) to the runner's next poll.
    pub fn wake(self: *TaskState) bool {
        var cur: State = @fromBackingInt(self.word.load(.acquire));
        while (true) {
            const next: State = switch (cur) {
                .idle => .scheduled,
                .running => .notified,
                .scheduled, .notified, .complete => return false,
            };
            if (self.word.cmpxchgWeak(@backingInt(cur), @backingInt(next), .acq_rel, .acquire)) |actual| {
                cur = @fromBackingInt(actual);
                continue;
            }
            return next == .scheduled;
        }
    }

    /// The runner takes a queue entry: `SCHEDULED→RUNNING`. False ⇒ a stale
    /// entry (the task completed or was re-queued through another path) —
    /// skip it. acquire pairs with `wake`'s acq_rel.
    pub fn beginPoll(self: *TaskState) bool {
        return self.word.cmpxchgStrong(@backingInt(State.scheduled), @backingInt(State.running), .acq_rel, .acquire) == null;
    }

    /// The poll returned Pending (the fiber parked): `RUNNING→IDLE`. On
    /// failure the state is NOTIFIED (a wake landed mid-poll) and ONLY the
    /// runner may leave it: a plain release store to SCHEDULED, and the
    /// caller re-enqueues (true). release pairs with the next `beginPoll`'s
    /// acquire so the fiber's parked frame is visible to its next runner.
    pub fn endPollPending(self: *TaskState) bool {
        if (self.word.cmpxchgStrong(@backingInt(State.running), @backingInt(State.idle), .release, .acquire)) |actual| {
            std.debug.assert(@as(State, @fromBackingInt(actual)) == .notified);
            self.word.store(@backingInt(State.scheduled), .release);
            return true;
        }
        return false;
    }

    /// The task finished: terminal. The previous state is RUNNING or
    /// NOTIFIED (a wake that raced completion no-ops from here on). acq_rel:
    /// the result bytes written before this are published to the awaiter.
    pub fn endPollComplete(self: *TaskState) void {
        const prev: State = @fromBackingInt(self.word.swap(@backingInt(State.complete), .acq_rel));
        std.debug.assert(prev == .running or prev == .notified);
    }
};

// ---------------------------------------------------------------------------
// Tests: the three loom models' assertions as unit tests on two threads
// (`task_state_model.zig` explores every schedule).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the state walk: spawn is scheduled; begin_poll runs; a parked task is idle; a wake enqueues exactly once; completion is terminal" {
    var s = TaskState.newScheduled();
    try testing.expectEqual(State.scheduled, s.load());
    try testing.expect(!s.wake());
    try testing.expect(s.beginPoll());
    try testing.expect(!s.beginPoll());
    try testing.expect(!s.endPollPending());
    try testing.expectEqual(State.idle, s.load());
    try testing.expect(s.wake());
    try testing.expect(!s.wake());
    try testing.expect(s.beginPoll());
    s.endPollComplete();
    try testing.expectEqual(State.complete, s.load());
    try testing.expect(!s.wake());
    try testing.expect(!s.beginPoll());
}

test "wake_racing_poll_is_never_lost_and_never_double_queued: a foreign wake during the poll is absorbed by the runner's requeue, exactly one enqueue" {
    const Shared = struct {
        s: TaskState = .newScheduled(),
        waker_enqueues: std.atomic.Value(u32) = .init(0),
        fn waker(sh: *@This()) void {
            if (sh.s.wake()) _ = sh.waker_enqueues.fetchAdd(1, .monotonic);
        }
    };
    var rounds: u32 = 0;
    while (rounds < 2000) : (rounds += 1) {
        var sh: Shared = .{};
        try testing.expect(sh.s.beginPoll());
        const t = try std.Thread.spawn(.{}, Shared.waker, .{&sh});
        const runner_requeues = sh.s.endPollPending();
        t.join();
        // Exactly one party enqueues, and the task is pollable again.
        try testing.expect(runner_requeues != (sh.waker_enqueues.load(.monotonic) == 1));
        try testing.expectEqual(State.scheduled, sh.s.load());
        try testing.expect(sh.s.beginPoll());
    }
}

test "concurrent_wakes_enqueue_exactly_once: two wakes on an idle task, one enqueue" {
    const Shared = struct {
        s: TaskState = .newScheduled(),
        enqueues: std.atomic.Value(u32) = .init(0),
        fn waker(sh: *@This()) void {
            if (sh.s.wake()) _ = sh.enqueues.fetchAdd(1, .monotonic);
        }
    };
    var rounds: u32 = 0;
    while (rounds < 2000) : (rounds += 1) {
        var sh: Shared = .{};
        try testing.expect(sh.s.beginPoll());
        try testing.expect(!sh.s.endPollPending());
        const a = try std.Thread.spawn(.{}, Shared.waker, .{&sh});
        const b = try std.Thread.spawn(.{}, Shared.waker, .{&sh});
        a.join();
        b.join();
        try testing.expectEqual(@as(u32, 1), sh.enqueues.load(.monotonic));
        try testing.expectEqual(State.scheduled, sh.s.load());
    }
}

test "wake_racing_completion_noops: a wake racing end_poll_complete never enqueues; the task ends complete" {
    const Shared = struct {
        s: TaskState = .newScheduled(),
        enqueues: std.atomic.Value(u32) = .init(0),
        fn waker(sh: *@This()) void {
            if (sh.s.wake()) _ = sh.enqueues.fetchAdd(1, .monotonic);
        }
    };
    var rounds: u32 = 0;
    while (rounds < 2000) : (rounds += 1) {
        var sh: Shared = .{};
        try testing.expect(sh.s.beginPoll());
        const t = try std.Thread.spawn(.{}, Shared.waker, .{&sh});
        sh.s.endPollComplete();
        t.join();
        try testing.expectEqual(@as(u32, 0), sh.enqueues.load(.monotonic));
        try testing.expectEqual(State.complete, sh.s.load());
    }
}
