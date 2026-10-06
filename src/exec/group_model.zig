//! Explorer model of the group join (`group.zig`): two members finishing
//! against one awaiter registering, over one token word.
//!
//! The real protocol is one RMW per side: a member `fetchSub`s and acts
//! when it saw `num_running == 1 ∧ have_awaiter`; the awaiter `fetchOr`s
//! `have_awaiter` and parks only when it saw `num_running != 0`. The
//! invariant: the awaiter is woken at most once (never doubled), and when
//! every member is gone the awaiter is never left parked (never lost).
//!
//! Weakening ledger:
//!   `.split_awaiter_rmw` — the awaiter reads `num_running` and then sets
//!     `have_awaiter` as two steps (a `.monotonic` load + store instead of
//!     the RMW): the last member's decrement lands between them, sees no
//!     awaiter, wakes nobody; the awaiter then parks on a finished group —
//!     the lost wake.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const Weakening = struct {
    split_awaiter_rmw: bool = false,
};

fn GroupModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        /// Threads 0 and 1 are members; 2 is the awaiter.
        pub const thread_count: usize = 3;
        const awaiter = 2;

        num_running: u8 = 2,
        have_awaiter: bool = false,
        /// The awaiter's cell is published (`g.state`).
        cell_published: bool = false,
        /// The cell word (1 = woken).
        word: u8 = 0,
        wakes: u8 = 0,
        mpc: [2]u8 = @splat(0),
        apc: u8 = 0,
        /// The weakened awaiter's loaded count.
        aload: u8 = 0,
        parked: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t == awaiter) self.apc == 9 else self.mpc[t] == 9;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == awaiter) return self.awaiterStep();
            switch (self.mpc[t]) {
                // fetchSub(one_running): act on what it read.
                0 => {
                    const prev_running = self.num_running;
                    const prev_awaiter = self.have_awaiter;
                    self.num_running -= 1;
                    if (prev_running == 1 and prev_awaiter) {
                        // The cell was published before the awaiter's RMW.
                        std.debug.assert(self.cell_published);
                        self.word = 1;
                        self.wakes += 1;
                        self.parked = false;
                    }
                    self.mpc[t] = 9;
                },
                else => unreachable,
            }
        }
        fn awaiterStep(self: *Self) void {
            switch (self.apc) {
                // Publish the cell.
                0 => {
                    self.cell_published = true;
                    self.apc = 1;
                },
                1 => {
                    if (weakening.split_awaiter_rmw) {
                        // WEAKENED: read now, set the flag in a later step.
                        self.aload = self.num_running;
                        self.apc = 2;
                        return;
                    }
                    // fetchOr(have_awaiter): one step.
                    const prev_running = self.num_running;
                    self.have_awaiter = true;
                    self.apc = if (prev_running == 0) 9 else 3;
                },
                2 => {
                    self.have_awaiter = true;
                    self.apc = if (self.aload == 0) 9 else 3;
                },
                // park until the word flips.
                3 => {
                    if (self.word != 0) {
                        self.apc = 9;
                    } else {
                        self.parked = true;
                    }
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.wakes > 1) return error.AwaiterWokenTwice;
            // Every member finished, the awaiter parked, nobody will wake it.
            if (self.num_running == 0 and self.parked and self.word == 0 and self.mpc[0] == 9 and self.mpc[1] == 9) return error.LostWake;
        }
        pub fn final(self: *const Self) !void {
            if (self.apc != 9) return error.AwaiterNeverReturned;
            if (self.num_running != 0) return error.MembersOutstanding;
        }
    };
}

const testing = std.testing;

test "the group join holds: the awaiter is woken exactly once or never needs to be; the split awaiter RMW loses the wake" {
    try explorer.expectHolds(GroupModel(.{}), .{});
    try explorer.expectFails(GroupModel(.{ .split_awaiter_rmw = true }), .{}, error.InvariantViolated);
}

test "group model: exhaustive within the explorer's default limits" {
    var report: explorer.Report = undefined;
    try explorer.explore(GroupModel(.{}), testing.allocator, .{}, &report);
    try testing.expect(report.states > 1);
    try testing.expect(report.states < 200);
}
