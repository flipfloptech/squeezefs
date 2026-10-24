//! The park gate's two lock-free invariants as a standalone MODEL (charter
//! §9 "Model-check every lock-free core"; `docs/porting/sym_wire_
//! membership_recovery.md` §2.6 "Two model-checked invariants").
//! Dependency-free; explored exhaustively by `model/explorer.zig` under TSO
//! (a per-thread store buffer the fence step drains).
//!
//! Invariant 1 — the Dekker pair: committer `try_enter` does `inflight +=
//! 1 ; fence ; load state` and is ADMITTED iff the word is not PARKED (else
//! it withdraws); the parker does `state = PARKED ; fence ; load inflight`
//! and HOLDS the acks of whatever it saw in flight. Never (admitted ∧ the
//! parker read 0): a commit the parker never saw would land and be acked
//! past the park — the acked-then-lost shape. The weakening (either fence
//! removed) lets both buffered stores stay invisible while both loads read
//! the pre-race value.
//!
//! Invariant 2 — release and expiry are exclusive: two racing callers each
//! CAS the word off PARKED; exactly one moves it (a held ack is released OR
//! failed, never both, never neither). The weakening is a load-then-store
//! on the word instead of the CAS: both callers read PARKED and both act.
//!
//! Invariant 3 — the reclaim re-arms `T_self` BEFORE it releases (the
//! double park, 2026-10-24): the beat's reclaim publishes the grant's
//! deadline (`MemberLeaseWords.renewed`, a Release store) and only then
//! CASes the word `PARKED → RUNNING`; a committer at the pre-admission gate
//! that sees RUNNING judges `T_self` (`SelfFence`) against that deadline,
//! so it never parks the gate a second time on the pre-reclaim one. Under
//! TSO the deadline's store sits in the beat's buffer until the CAS (an
//! RMW) drains it. The weakening is the release before the learn: a
//! committer reads RUNNING and the stale deadline and parks a member whose
//! lease the successor just granted.

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

pub const RUNNING: u8 = 0;
pub const PARKED: u8 = 1;
pub const EXPIRED: u8 = 2;

pub const Weakening = struct {
    enter_fence: bool = true,
    park_fence: bool = true,
};

/// Invariant 1: one committer, one parker.
pub fn DekkerModel(comptime w: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        inflight: u8 = 0,
        state: u8 = RUNNING,
        pending_inflight: bool = false,
        pending_park: bool = false,
        admitted: bool = false,
        /// What the parker read as in flight (null = not yet).
        parker_saw: ?u8 = null,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 4;
        }

        fn drainEnter(self: *Self) void {
            if (self.pending_inflight) {
                self.inflight += 1;
                self.pending_inflight = false;
            }
        }

        fn drainPark(self: *Self) void {
            if (self.pending_park) {
                self.state = PARKED;
                self.pending_park = false;
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => switch (self.pc[0]) {
                    0 => {
                        self.pending_inflight = true;
                        self.pc[0] = 1;
                    },
                    1 => {
                        if (w.enter_fence) self.drainEnter();
                        self.pc[0] = 2;
                    },
                    2 => {
                        if (self.state == RUNNING) {
                            self.admitted = true;
                        } else {
                            // Withdraw: the RMW drains the buffer first.
                            self.drainEnter();
                            self.inflight -= 1;
                        }
                        self.pc[0] = 3;
                    },
                    3 => {
                        // The buffered store lands at a later step.
                        self.drainEnter();
                        self.pc[0] = 4;
                    },
                    else => unreachable,
                },
                1 => switch (self.pc[1]) {
                    0 => {
                        self.pending_park = true;
                        self.pc[1] = 1;
                    },
                    1 => {
                        if (w.park_fence) self.drainPark();
                        self.pc[1] = 2;
                    },
                    2 => {
                        self.parker_saw = self.inflight;
                        self.pc[1] = 3;
                    },
                    3 => {
                        self.drainPark();
                        self.pc[1] = 4;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.admitted) if (self.parker_saw) |n| if (n == 0) return error.AdmittedCommitUnseenByPark;
        }

        pub fn final(_: *const Self) !void {}
    };
}

/// Invariant 2: a releaser and an expirer racing off PARKED.
pub fn ExclusiveExitModel(comptime cas: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        state: u8 = PARKED,
        /// The load-then-store weakening's read value per thread.
        seen: [2]?u8 = .{ null, null },
        acted: [2]bool = .{ false, false },
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 2;
        }

        pub fn step(self: *Self, t: usize) void {
            const target: u8 = if (t == 0) RUNNING else EXPIRED;
            if (cas) {
                // ONE atomic action: CAS(PARKED → target).
                if (self.state == PARKED) {
                    self.state = target;
                    self.acted[t] = true;
                }
                self.pc[t] = 2;
                return;
            }
            switch (self.pc[t]) {
                0 => {
                    self.seen[t] = self.state;
                    self.pc[t] = 1;
                },
                1 => {
                    if (self.seen[t].? == PARKED) {
                        self.state = target;
                        self.acted[t] = true;
                    }
                    self.pc[t] = 2;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.acted[0] and self.acted[1]) return error.ReleasedAndExpired;
        }

        pub fn final(self: *const Self) !void {
            if (!self.acted[0] and !self.acted[1]) return error.NeitherMovedTheWord;
        }
    };
}

/// Invariant 3: the beat's reclaim (learn the deadline, release the word)
/// and one committer judging `T_self` at the pre-admission gate. The clock
/// stands between the two deadlines: due on the pre-reclaim one, not on
/// the reclaim's.
pub fn ReArmModel(comptime learn_first: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const NOW: u8 = 5;
        const OLD_DEADLINE: u8 = 4;
        const NEW_DEADLINE: u8 = 9;

        state: u8 = PARKED,
        deadline: u8 = OLD_DEADLINE,
        /// The beat's buffered deadline store (TSO).
        pending_deadline: ?u8 = null,
        /// What the committer read of the deadline after seeing RUNNING.
        seen_deadline: ?u8 = null,
        saw_running: bool = false,
        reparked: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 3;
        }

        fn drain(self: *Self) void {
            if (self.pending_deadline) |d| {
                self.deadline = d;
                self.pending_deadline = null;
            }
        }

        fn release(self: *Self) void {
            // The CAS is an RMW: it drains the buffer first.
            self.drain();
            if (self.state == PARKED) self.state = RUNNING;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => switch (self.pc[0]) {
                    0 => {
                        if (learn_first) self.pending_deadline = NEW_DEADLINE else self.release();
                        self.pc[0] = 1;
                    },
                    1 => {
                        if (learn_first) self.release() else self.pending_deadline = NEW_DEADLINE;
                        self.pc[0] = 2;
                    },
                    2 => {
                        // The buffered store lands at a later step.
                        self.drain();
                        self.pc[0] = 3;
                    },
                    else => unreachable,
                },
                1 => switch (self.pc[1]) {
                    0 => {
                        // The pre-admission gate's word load: PARKED waits
                        // (the model ends the committer there).
                        self.saw_running = self.state == RUNNING;
                        self.pc[1] = if (self.saw_running) 1 else 3;
                    },
                    1 => {
                        self.seen_deadline = self.deadline;
                        self.pc[1] = 2;
                    },
                    2 => {
                        // `selfFenceDue`: `now ≥ deadline` parks (the CAS).
                        if (NOW >= self.seen_deadline.? and self.state == RUNNING) {
                            self.state = PARKED;
                            self.reparked = true;
                        }
                        self.pc[1] = 3;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.reparked) return error.ReparkedOnThePreReclaimDeadline;
        }

        pub fn final(_: *const Self) !void {}
    };
}

test "the park gate's Dekker pair holds with both fences; each fence removed fails (an admitted commit the park never saw)" {
    try explorer.expectHolds(DekkerModel(.{}), .{});
    try explorer.expectFails(DekkerModel(.{ .enter_fence = false }), .{}, error.InvariantViolated);
    try explorer.expectFails(DekkerModel(.{ .park_fence = false }), .{}, error.InvariantViolated);
}

test "release and expiry are exclusive under the CAS; the load-then-store weakening lets both act" {
    try explorer.expectHolds(ExclusiveExitModel(true), .{});
    try explorer.expectFails(ExclusiveExitModel(false), .{}, error.InvariantViolated);
}

test "the reclaim re-arms T_self before it releases: a committer that sees RUNNING judges the reclaim's deadline and never re-parks the gate; the release before the learn re-parks a member its successor just granted" {
    try explorer.expectHolds(ReArmModel(true), .{});
    try explorer.expectFails(ReArmModel(false), .{}, error.InvariantViolated);
}
