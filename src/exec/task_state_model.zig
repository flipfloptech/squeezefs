//! Explorer models of the delivery word (`task_state.zig`): the reference's
//! three loom models (`exec.md` §1.3) over every schedule, plus the two
//! weakenings the reference never recorded.
//!
//! Each RMW of the real code is ONE step; the one real two-step sequence
//! (`end_poll_pending`'s failed CAS followed by the plain `store(SCHEDULED)`)
//! is two steps, so a wake may land between them exactly as on hardware.
//!
//! Weakening ledger:
//!   wake_racing_poll — `.pending_as_store`: `end_poll_pending` as a plain
//!     `store(IDLE)` instead of the CAS: the NOTIFIED a mid-poll wake left
//!     is overwritten and nobody enqueues (law 1 broken: a lost wake).
//!   concurrent_wakes — `.wake_as_load_store`: `wake` as a load followed by
//!     a store instead of the CAS: both wakers read IDLE and both enqueue
//!     (law 2 broken: queued twice).

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const Scenario = enum { wake_racing_poll, concurrent_wakes, wake_racing_completion };

const Weakening = struct {
    pending_as_store: bool = false,
    wake_as_load_store: bool = false,
};

const idle: u8 = 0;
const scheduled: u8 = 1;
const running: u8 = 2;
const notified: u8 = 3;
const complete: u8 = 4;

fn TaskModel(comptime scenario: Scenario, comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        /// Thread 0 is the runner (idle from the start in the two-wakes
        /// scenario); the rest are wakers.
        pub const thread_count: usize = switch (scenario) {
            .concurrent_wakes => 3,
            else => 2,
        };
        const runner = 0;
        const wakers: usize = thread_count - 1;

        state: u8 = switch (scenario) {
            .concurrent_wakes => idle,
            else => scheduled,
        },
        /// Queue entries for this task (law 2: never above 1); a SCHEDULED
        /// task starts with its one entry.
        queue: u8 = switch (scenario) {
            .concurrent_wakes => 0,
            else => 1,
        },
        rpc: u8 = switch (scenario) {
            // The two-wakes scenario has no runner: it is done from the start.
            .concurrent_wakes => 9,
            else => 0,
        },
        runner_requeued: bool = false,
        wpc: [wakers]u8 = @splat(0),
        /// The weakened wake's loaded value between its load and its store.
        wload: [wakers]u8 = @splat(0),
        waker_enqueues: u8 = 0,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            if (t == runner) return self.rpc == 9;
            return self.wpc[t - 1] == 9;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == runner) return self.runnerStep();
            self.wakerStep(t - 1);
        }
        fn runnerStep(self: *Self) void {
            switch (self.rpc) {
                // begin_poll: CAS SCHEDULED→RUNNING (the queue entry is taken).
                0 => {
                    if (self.state == scheduled) {
                        self.state = running;
                        self.queue -= 1;
                    }
                    self.rpc = 1;
                },
                1 => switch (scenario) {
                    .wake_racing_poll => {
                        if (weakening.pending_as_store) {
                            // WEAKENED: a plain store loses a NOTIFIED.
                            self.state = idle;
                            self.rpc = 9;
                        } else if (self.state == running) {
                            // CAS RUNNING→IDLE succeeds: the fiber is parked.
                            self.state = idle;
                            self.rpc = 9;
                        } else {
                            // CAS failed: NOTIFIED. The plain store is the next step.
                            self.rpc = 2;
                        }
                    },
                    .wake_racing_completion => {
                        // end_poll_complete: swap(COMPLETE).
                        self.state = complete;
                        self.rpc = 9;
                    },
                    .concurrent_wakes => unreachable,
                },
                // store(SCHEDULED) and re-enqueue: only the runner leaves NOTIFIED.
                2 => {
                    self.state = scheduled;
                    self.queue += 1;
                    self.runner_requeued = true;
                    self.rpc = 9;
                },
                else => unreachable,
            }
        }
        fn wakerStep(self: *Self, w: usize) void {
            // A racing wake starts once the poll began (the loom models'
            // "concurrently with end_poll_pending"); a step before that is a
            // no-op the explorer prunes as an already-seen state.
            if (scenario != .concurrent_wakes and self.rpc == 0) return;
            switch (self.wpc[w]) {
                0 => {
                    if (weakening.wake_as_load_store) {
                        // WEAKENED: load now, store later.
                        self.wload[w] = self.state;
                        self.wpc[w] = 1;
                        return;
                    }
                    // wake: one CAS from the current state.
                    self.applyWake(self.state);
                    self.wpc[w] = 9;
                },
                1 => {
                    self.applyWake(self.wload[w]);
                    self.wpc[w] = 9;
                },
                else => unreachable,
            }
        }
        fn applyWake(self: *Self, seen: u8) void {
            switch (seen) {
                idle => {
                    self.state = scheduled;
                    self.queue += 1;
                    self.waker_enqueues += 1;
                },
                running => self.state = notified,
                else => {},
            }
        }
        pub fn invariant(self: *const Self) !void {
            // Law 2: never queued twice.
            if (self.queue > 1) return error.QueuedTwice;
        }
        pub fn final(self: *const Self) !void {
            switch (scenario) {
                .wake_racing_poll => {
                    // Exactly one party enqueued and the task is pollable.
                    if (self.runner_requeued == (self.waker_enqueues == 1)) return error.EnqueueNotExclusive;
                    if (self.state != scheduled or self.queue != 1) return error.NotRescheduled;
                },
                .concurrent_wakes => {
                    if (self.waker_enqueues != 1 or self.queue != 1 or self.state != scheduled) return error.NotExactlyOnce;
                },
                .wake_racing_completion => {
                    if (self.waker_enqueues != 0 or self.queue != 0 or self.state != complete) return error.WakeAfterCompletion;
                },
            }
        }
    };
}

const testing = std.testing;

test "wake racing poll is never lost and never double-queued; the plain-store end_poll_pending loses the mid-poll wake" {
    try explorer.expectHolds(TaskModel(.wake_racing_poll, .{}), .{});
    try explorer.expectFails(TaskModel(.wake_racing_poll, .{ .pending_as_store = true }), .{}, error.FinalViolated);
}

test "concurrent wakes enqueue exactly once; a load+store wake enqueues twice" {
    try explorer.expectHolds(TaskModel(.concurrent_wakes, .{}), .{});
    try explorer.expectFails(TaskModel(.concurrent_wakes, .{ .wake_as_load_store = true }), .{}, error.InvariantViolated);
}

test "a wake racing completion never enqueues" {
    try explorer.expectHolds(TaskModel(.wake_racing_completion, .{}), .{});
}

test "task_state models: exhaustive within the explorer's default limits" {
    inline for (.{ TaskModel(.wake_racing_poll, .{}), TaskModel(.concurrent_wakes, .{}), TaskModel(.wake_racing_completion, .{}) }) |Model| {
        var report: explorer.Report = undefined;
        try explorer.explore(Model, testing.allocator, .{}, &report);
        try testing.expect(report.states > 1);
        try testing.expect(report.states < 200);
    }
}
