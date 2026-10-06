//! Explorer models of the revalidation pair (`epoch_core.zig`; survey §3.2;
//! the loom models `epoch_core_*` in the reference's
//! `loom-models/src/lib.rs`).
//!
//! Plain value types: the epoch and tail are `u8` fields (the values are
//! tiny), and every `step` is ONE atomic action — a load, a store, a fetchMax, or one CAS attempt (a failed
//! CAS re-reads and the thread stays on the same program counter). The
//! explorer runs sequentially consistent, so an ordering weakening is
//! expressed as the REORDERING it permits: the publisher's two stores
//! swapped, the loader's two loads swapped, a CAS replaced by a store.
//!
//! Weakening ledger:
//!   1 loader_never_sees_an_epoch_newer_than_its_tail — publish epoch-then-tail; load tail-then-epoch
//!   2 racing_pollers_step_the_epoch_exactly_once     — the epoch CAS as a plain store
//!   3 a_node_published_across_an_advance_is_adopted_or_rejected — stamp from a post-read re-probe

const std = @import("std");
const explorer = @import("../model/explorer.zig");
const core = @import("epoch_core.zig");

const done_pc: u8 = 0xff;

/// One `publish(t, e)` as a sub-machine: load the epoch, give up if not
/// newer, fetchMax the tail, CAS the epoch (retrying on a smaller observed
/// value, giving up on a larger one). `epoch_first` is the reversal
/// weakening: the CAS before the tail store.
const Publish = struct {
    pc: u8 = 0,
    cur: u8 = 0,
    moved: bool = false,

    fn step(self: *Publish, epoch: *u8, tail: *u8, t: u8, e: u8, comptime epoch_first: bool) void {
        switch (self.pc) {
            0 => {
                self.cur = epoch.*;
                self.pc = if (self.cur == core.unarmed_epoch or e <= self.cur) done_pc else 1;
            },
            1 => if (epoch_first) self.cas(epoch, e, 2) else {
                tail.* = @max(tail.*, t);
                self.pc = 2;
            },
            2 => if (epoch_first) {
                tail.* = @max(tail.*, t);
                self.pc = done_pc;
            } else self.cas(epoch, e, done_pc),
            else => unreachable,
        }
    }

    /// One CAS attempt `cur → e`.
    fn cas(self: *Publish, epoch: *u8, e: u8, next_pc: u8) void {
        if (epoch.* == self.cur) {
            epoch.* = e;
            self.moved = true;
            self.pc = next_pc;
        } else if (epoch.* >= e) {
            self.pc = done_pc;
        } else {
            self.cur = epoch.*;
        }
    }

    /// The epoch step as a plain store: the split-RMW weakening.
    fn storeInsteadOfCas(self: *Publish, epoch: *u8, e: u8) void {
        epoch.* = e;
        self.moved = true;
        self.pc = done_pc;
    }
};

// ---------------------------------------------------------------------------
// 1. the two-word law
// ---------------------------------------------------------------------------

/// Epoch 1 covers tail 10; a poller advances to epoch 2 / tail 20 while a
/// loader snapshots the pair. Any observed pair must satisfy
/// `epoch ≥ 2 ⇒ tail ≥ 20`: the publisher stores tail-then-epoch, the loader
/// reads epoch-then-tail.
///
/// Weakenings: `.publish_epoch_first` — the loader reads epoch 2 before the
/// tail store lands and then tail 10; `.load_tail_first` — the loader reads
/// tail 10, the poller publishes both, the loader reads epoch 2.
fn LoaderPair(comptime weakening: enum { none, publish_epoch_first, load_tail_first }) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const poller = 0;
        const loader = 1;

        epoch: u8 = 1,
        tail: u8 = 10,
        pub_: Publish = .{},
        load_pc: u8 = 0,
        seen_epoch: u8 = 0,
        seen_tail: u8 = 0,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                poller => self.pub_.pc == done_pc,
                loader => self.load_pc == done_pc,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                poller => self.pub_.step(&self.epoch, &self.tail, 20, 2, weakening == .publish_epoch_first),
                loader => {
                    const tail_first = weakening == .load_tail_first;
                    switch (self.load_pc) {
                        0 => {
                            if (tail_first) self.seen_tail = self.tail else self.seen_epoch = self.epoch;
                            self.load_pc = 1;
                        },
                        1 => {
                            if (tail_first) self.seen_epoch = self.epoch else self.seen_tail = self.tail;
                            self.load_pc = done_pc;
                        },
                        else => unreachable,
                    }
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.load_pc != done_pc) return;
            if (self.seen_epoch != 1 and self.seen_epoch != 2) return error.EpochFromNowhere;
            // A node stamped epoch 2 classified against tail 10 — the silent-record-loss inversion.
            if (self.seen_epoch >= 2 and self.seen_tail < 20) return error.PairInverted;
        }
        pub fn final(self: *const Self) !void {
            if (self.epoch != 2 or self.tail != 20) return error.AdvanceDidNotLand;
        }
    };
}

// ---------------------------------------------------------------------------
// 2. racing pollers
// ---------------------------------------------------------------------------

/// A cadence tick and an on-demand revalidation both publish epoch 6 over
/// epoch 5: the epoch is monotone and EXACTLY one of them observes the step
/// (two sweeps for one step would double-credit the budget gauge); a repeat
/// publish of 6 is inert; the tail is monotone.
///
/// Weakening `.epoch_store` (the CAS as a plain store): both load 5, both
/// store 6, both report the move.
fn RacingPollers(comptime weakening: enum { none, epoch_store }) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        epoch: u8 = 5,
        tail: u8 = 0,
        pubs: [2]Publish = @splat(.{}),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pubs[t].pc == done_pc;
        }
        pub fn step(self: *Self, t: usize) void {
            const p = &self.pubs[t];
            if (weakening == .epoch_store and p.pc == 2) return p.storeInsteadOfCas(&self.epoch, 6);
            p.step(&self.epoch, &self.tail, 50, 6, false);
        }
        pub fn invariant(_: *const Self) !void {}
        pub fn final(self: *const Self) !void {
            const winners = @as(u8, @intFromBool(self.pubs[0].moved)) + @intFromBool(self.pubs[1].moved);
            if (winners != 1) return error.StepNotObservedExactlyOnce;
            if (self.epoch != 6) return error.EpochNotAdvanced;
            if (self.tail != 50) return error.TailNotMonotone;
            // publish(40, 6) on epoch 6 gives up at its first load.
            if (!(6 <= self.epoch)) return error.RepeatStepWouldAdvance;
        }
    };
}

// ---------------------------------------------------------------------------
// 3. a loader racing an advance
// ---------------------------------------------------------------------------

/// The stamp/gate composition the cache relies on: a loader snapshots the
/// pair BEFORE the device read and publishes a node stamped with that
/// snapshot's epoch. The cache serves a node iff its stamp equals the
/// current epoch, so a node served after the advance (stamp 2) must have
/// been classified against a tail epoch 2 vouches for (≥ 7); a node stamped
/// 1 is rejected and reloaded. Nothing else is ever published.
///
/// Weakening `.stamp_from_reprobe` (stamp the epoch probed at publish time
/// instead of the pre-read snapshot): snapshot (1, 0), the poller publishes
/// (7, 2), the loader re-probes 2 and publishes a stale node as current.
fn PublishAcrossAdvance(comptime weakening: enum { none, stamp_from_reprobe }) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const loader = 0;
        const poller = 1;

        epoch: u8 = 1,
        tail: u8 = 0,
        /// The map: the stamp the published node carries (0 = empty).
        stamp: u8 = 0,
        pub_: Publish = .{},
        load_pc: u8 = 0,
        snap_epoch: u8 = 0,
        snap_tail: u8 = 0,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                loader => self.load_pc == done_pc,
                poller => self.pub_.pc == done_pc,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                loader => switch (self.load_pc) {
                    0 => {
                        self.snap_epoch = self.epoch;
                        self.load_pc = 1;
                    },
                    1 => {
                        self.snap_tail = self.tail;
                        self.load_pc = 2;
                    },
                    // (the device read happens here; it touches no shared word)
                    2 => switch (weakening) {
                        .none => {
                            self.stamp = self.snap_epoch;
                            self.load_pc = done_pc;
                        },
                        .stamp_from_reprobe => {
                            self.snap_epoch = self.epoch;
                            self.load_pc = 3;
                        },
                    },
                    3 => {
                        self.stamp = self.snap_epoch;
                        self.load_pc = done_pc;
                    },
                    else => unreachable,
                },
                poller => self.pub_.step(&self.epoch, &self.tail, 7, 2, false),
                else => unreachable,
            }
        }
        pub fn invariant(_: *const Self) !void {}
        pub fn final(self: *const Self) !void {
            if (self.epoch != 2) return error.AdvanceDidNotLand;
            if (self.stamp != 1 and self.stamp != 2) return error.StampFromNowhere;
            const served = self.stamp == self.epoch;
            if (served and self.snap_tail < 7) return error.StaleNodeServedAsCurrent;
        }
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "epoch_core_loader_never_sees_an_epoch_newer_than_its_tail: holds; either reversal fails" {
    try explorer.expectHolds(LoaderPair(.none), .{});
    try explorer.expectFails(LoaderPair(.publish_epoch_first), .{}, error.InvariantViolated);
    try explorer.expectFails(LoaderPair(.load_tail_first), .{}, error.InvariantViolated);
}

test "epoch_core_racing_pollers_step_the_epoch_exactly_once: holds; the epoch CAS as a store fails" {
    try explorer.expectHolds(RacingPollers(.none), .{});
    try explorer.expectFails(RacingPollers(.epoch_store), .{}, error.FinalViolated);
}

test "epoch_core_a_node_published_across_an_advance_is_adopted_or_rejected: holds; a post-read re-probe stamp fails" {
    try explorer.expectHolds(PublishAcrossAdvance(.none), .{});
    try explorer.expectFails(PublishAcrossAdvance(.stamp_from_reprobe), .{}, error.FinalViolated);
}

// The state counts the evidence note records.
test "epoch models: exhaustive within the explorer's default limits" {
    inline for (.{ LoaderPair(.none), RacingPollers(.none), PublishAcrossAdvance(.none) }) |Model| {
        var report: explorer.Report = undefined;
        try explorer.explore(Model, testing.allocator, .{}, &report);
        try testing.expect(report.states > 1);
        try testing.expect(report.states < 1000);
    }
}
