//! Explorer models of the permit/epoch pair (`notify.zig`): one notifier
//! racing one waiter that runs the enable-then-recheck idiom over a shared
//! `ready` flag the notifier sets before notifying.
//!
//! The interior mutex is a flag with explicit acquire/release steps; the
//! park is one atomic compare-and-sleep step whose sleeper, once every
//! possible waker has finished, is the flagged LOST WAKEUP.
//!
//! Weakening ledger:
//!   notify_waiters — `.epoch_read_after_park`: the registration epoch is
//!     snapshotted at the first wait instead of at `enable` (the todo-24
//!     wedge: a bump between the two is invisible); `.recheck_before_enable`:
//!     the caller re-checks `ready` BEFORE registering (the classic lost
//!     wakeup — the registration misses the bump).
//!   notify_one     — `.recheck_before_enable` alone HOLDS: the stored
//!     permit is exactly what makes the send-then-wait race safe; with
//!     `.no_permit_store` (a notify with nobody registered stores nothing)
//!     beside it the wake is lost.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const Kind = enum { one, waiters };

/// Weakenings compose (the permit's proof needs two at once).
const Weakening = struct {
    epoch_read_after_park: bool = false,
    recheck_before_enable: bool = false,
    no_permit_store: bool = false,
};

fn NotifyModel(comptime kind: Kind, comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const notifier = 0;
        const waiter = 1;

        /// The interior mutex.
        locked: bool = false,
        permit: bool = false,
        epoch: u8 = 0,
        /// The waiter's registration (a one-entry queue).
        linked: bool = false,
        token: u8 = 0,
        /// The caller's condition, set by the notifier before it notifies.
        ready: bool = false,

        npc: u8 = 0,
        wpc: u8 = 0,
        registered: bool = false,
        registered_epoch: u8 = 0,
        /// `.epoch_read_after_park`: the late snapshot has been taken.
        snapshot_taken: bool = false,
        satisfied: bool = false,
        lost: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                notifier => self.npc == 4,
                waiter => self.wpc == 9,
                else => unreachable,
            };
        }
        fn link(self: *Self) void {
            self.linked = true;
            self.registered = true;
            if (!weakening.epoch_read_after_park) {
                self.registered_epoch = self.epoch;
                self.snapshot_taken = true;
            }
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                notifier => switch (self.npc) {
                    0 => {
                        self.ready = true;
                        self.npc = 1;
                    },
                    1 => if (!self.locked) {
                        self.locked = true;
                        self.npc = 2;
                    },
                    2 => {
                        switch (kind) {
                            .one => {
                                if (self.linked or !weakening.no_permit_store) self.permit = true;
                                if (self.linked) {
                                    self.linked = false;
                                    self.token = 1;
                                }
                            },
                            .waiters => {
                                self.epoch += 1;
                                if (self.linked) {
                                    self.linked = false;
                                    self.token = 1;
                                }
                            },
                        }
                        self.npc = 3;
                    },
                    3 => {
                        self.locked = false;
                        self.npc = 4;
                    },
                    else => unreachable,
                },
                waiter => switch (self.wpc) {
                    // The weakened idiom re-checks first.
                    0 => {
                        if (weakening.recheck_before_enable and self.ready) {
                            self.satisfied = true;
                            self.wpc = 9;
                        } else self.wpc = 1;
                    },
                    // enable: register under the lock.
                    1 => if (!self.locked) {
                        self.locked = true;
                        self.wpc = 2;
                    },
                    2 => {
                        if (!self.permit) self.link();
                        self.wpc = 3;
                    },
                    3 => {
                        self.locked = false;
                        self.wpc = if (weakening.recheck_before_enable) 5 else 4;
                    },
                    // The shipped idiom's re-check: satisfied ⇒ cancel the registration.
                    4 => {
                        if (self.ready) {
                            self.satisfied = true;
                            self.wpc = 10;
                        } else self.wpc = 5;
                    },
                    10 => if (!self.locked) {
                        self.locked = true;
                        self.linked = false;
                        self.wpc = 11;
                    },
                    11 => {
                        self.locked = false;
                        self.wpc = 9;
                    },
                    // wait: check under the lock, else park.
                    5 => if (!self.locked) {
                        self.locked = true;
                        self.wpc = 6;
                    },
                    6 => {
                        if (self.registered and !self.snapshot_taken) {
                            // The weakened snapshot: taken at the first wait,
                            // after any bump that landed since `enable`.
                            self.registered_epoch = self.epoch;
                            self.snapshot_taken = true;
                        }
                        if (self.registered and self.epoch > self.registered_epoch) {
                            self.linked = false;
                            self.satisfied = true;
                            self.wpc = 7;
                        } else if (self.permit) {
                            self.permit = false;
                            self.linked = false;
                            self.satisfied = true;
                            self.wpc = 7;
                        } else {
                            if (!self.linked) self.link();
                            self.token = 0;
                            self.wpc = 7;
                        }
                    },
                    7 => {
                        self.locked = false;
                        self.wpc = if (self.satisfied) 9 else 8;
                    },
                    // park: one atomic compare-and-sleep.
                    8 => if (self.token != 0) {
                        self.wpc = 5;
                    } else if (self.npc == 4) {
                        self.lost = true;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.lost) return error.LostWakeup;
        }
        pub fn final(self: *const Self) !void {
            if (!self.satisfied) return error.WaiterNeverSatisfied;
        }
    };
}

const testing = std.testing;

test "notify_waiters enable-then-recheck: holds; the late epoch read and recheck-before-enable both lose the wake" {
    try explorer.expectHolds(NotifyModel(.waiters, .{}), .{});
    try explorer.expectFails(NotifyModel(.waiters, .{ .epoch_read_after_park = true }), .{}, error.InvariantViolated);
    try explorer.expectFails(NotifyModel(.waiters, .{ .recheck_before_enable = true }), .{}, error.InvariantViolated);
}

test "notify_one permit: holds; the permit alone covers a recheck-before-enable waiter, and without it that waiter loses the wake" {
    try explorer.expectHolds(NotifyModel(.one, .{}), .{});
    try explorer.expectHolds(NotifyModel(.one, .{ .recheck_before_enable = true }), .{});
    try explorer.expectFails(NotifyModel(.one, .{ .recheck_before_enable = true, .no_permit_store = true }), .{}, error.InvariantViolated);
}

test "notify models: exhaustive within the explorer's default limits" {
    inline for (.{ NotifyModel(.waiters, .{}), NotifyModel(.one, .{}) }) |Model| {
        var report: explorer.Report = undefined;
        try explorer.explore(Model, testing.allocator, .{}, &report);
        try testing.expect(report.states > 1);
        try testing.expect(report.states < 5000);
    }
}
