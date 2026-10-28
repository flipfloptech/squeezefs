//! Explorer model of the sampled CPU census (`stats.SampledCpu`): ONE
//! writer — the budget sampler's thread — publishes a two-word sample
//! (`total`, `classes`) under a sequence word: odd while the write is in
//! flight, even and new once it landed; TWO readers (two `.stats` renders
//! on two lanes) each copy the two words between two reads of the sequence
//! and keep the copy iff the first read was even and the second equal.
//!
//! The invariant a kept copy carries is the sample's own: `classes ≤
//! total` — true of every published pair (the writer samples the classes
//! before the total), false of a TORN copy (the new total beside the old
//! classes, or the old total beside the new classes).
//!
//! Weakening ledger:
//!   `.no_recheck` — a reader keeps its copy on the first read's parity
//!     alone: a write that lands between its two word loads leaves it
//!     with a torn pair, kept.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const Weakening = struct {
    no_recheck: bool = false,
};

fn SampledModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        /// Threads 0 and 1 read; 2 writes.
        pub const thread_count: usize = 3;
        const writer = 2;

        seq: u8 = 0,
        /// The published pair: (total, classes) = (10, 4) then (20, 12).
        total: u8 = 10,
        classes: u8 = 4,
        /// The writer publishes twice.
        writes: u8 = 0,
        wpc: u8 = 0,
        rpc: [2]u8 = @splat(0),
        /// Per reader: the first sequence read, the copy in hand, and
        /// whether a copy was kept (the pair judged by `invariant`).
        seen: [2]u8 = @splat(0),
        copy_total: [2]u8 = @splat(0),
        copy_classes: [2]u8 = @splat(0),
        kept: [2]bool = @splat(false),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t == writer) self.wpc == 9 else self.rpc[t] == 9;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == writer) return self.writerStep();
            switch (self.rpc[t]) {
                // the first sequence read: odd ⇒ a write is in flight, retry
                0 => {
                    self.seen[t] = self.seq;
                    self.rpc[t] = if (self.seq & 1 != 0) 0 else 1;
                },
                // the copy's first word
                1 => {
                    self.copy_total[t] = self.total;
                    self.rpc[t] = 2;
                },
                // the copy's second word
                2 => {
                    self.copy_classes[t] = self.classes;
                    self.rpc[t] = if (weakening.no_recheck) 4 else 3;
                },
                // the re-read: equal ⇒ keep, else start over
                3 => {
                    if (self.seq == self.seen[t]) {
                        self.kept[t] = true;
                        self.rpc[t] = 9;
                    } else {
                        self.rpc[t] = 0;
                    }
                },
                // WEAKENED: kept on the first read's parity alone
                4 => {
                    self.kept[t] = true;
                    self.rpc[t] = 9;
                },
                else => unreachable,
            }
        }
        fn writerStep(self: *Self) void {
            switch (self.wpc) {
                // open: the sequence goes odd
                0 => {
                    self.seq += 1;
                    self.wpc = 1;
                },
                // the first word
                1 => {
                    self.total = 20;
                    self.wpc = 2;
                },
                // the second word
                2 => {
                    self.classes = 12;
                    self.wpc = 3;
                },
                // close: the sequence goes even
                3 => {
                    self.seq += 1;
                    self.writes += 1;
                    self.wpc = 9;
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            for (self.kept, 0..) |k, t| {
                if (!k) continue;
                if (self.copy_classes[t] > self.copy_total[t]) return error.TornCopyKept;
                // A whole copy is one of the two published pairs.
                const old = self.copy_total[t] == 10 and self.copy_classes[t] == 4;
                const new = self.copy_total[t] == 20 and self.copy_classes[t] == 12;
                if (!old and !new) return error.TornCopyKept;
            }
        }
        pub fn final(self: *const Self) !void {
            if (self.writes != 1) return error.WriteNotPublished;
            for (self.kept) |k| if (!k) return error.ReaderNeverKept;
        }
    };
}

const testing = std.testing;

test "the sampled census holds: every kept copy is a whole published pair; a reader that skips the re-read keeps a torn one" {
    try explorer.expectHolds(SampledModel(.{}), .{});
    try explorer.expectFails(SampledModel(.{ .no_recheck = true }), .{}, error.InvariantViolated);
}

test "sampled census model: exhaustive within the explorer's default limits" {
    var report: explorer.Report = undefined;
    try explorer.explore(SampledModel(.{}), testing.allocator, .{}, &report);
    try testing.expect(report.states > 1);
    try testing.expect(report.states < 2000);
}
