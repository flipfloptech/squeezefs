//! The commit door's Dekker pair as a standalone MODEL (charter §9
//! "Model-check every lock-free core"; `docs/porting/sym_slot_leases.md`
//! §1.5 "Port: ONE model with THREE threads"). Dependency-free; explored
//! exhaustively by `model/explorer.zig` under TSO (a per-thread store
//! buffer the fence step drains). Rung 5's cutover-gate model
//! (`routing/slot_gate_model.zig`, retired) is this model's two-thread
//! special case (`committers = 1`, the apply arm off): the one gate, one
//! model law of spec §3.6.
//!
//! Threads: committer 0 does `inflight += 1 ; fence ; load state` and
//! PASSES iff the state is not releasing (else backs out); the closer does
//! `state = releasing ; fence ; load inflight` and declares DRAINED iff
//! `inflight == 0` — then SNAPSHOTS the node under its lock. Committer 1
//! is the belt's apply arm — a mutation reaching the leaf WITHOUT a token
//! (a token holder's apply is admitted structurally: its token is in the
//! count the drain waits on): it takes the node lock, reads the verdict
//! INSIDE it (`releasing ⇒ refused`), and pushes its record iff allowed.
//!
//! Invariants: (a) never (committer 0 passed ∧ the closer read 0) — a
//! commit that passed the door is drained by the release; (b) never
//! (committer 1 applied ∧ its record not in the snapshot) — a record never
//! lands on a slot after its flush snapshotted. Weakenings (each verified
//! RED): either SeqCst fence removed lets both buffered stores stay
//! invisible while both loads read the pre-race value (a); the apply arm
//! reading the verdict BEFORE taking the node lock (check-then-lock) lets
//! `Allowed` be observed, `releasing` raised, an empty snapshot handed
//! over, and the record land after it (b).

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

pub const Weakening = struct {
    op_fence: bool = true,
    closer_fence: bool = true,
    /// The apply arm reads the verdict before the node lock.
    check_then_lock: bool = false,
    /// 1 = rung 5's cutover pair alone; 2 = with the apply arm.
    committers: u8 = 2,
};

pub fn Model(comptime w: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 1 + w.committers;

        inflight: u8 = 0,
        releasing: u8 = 0,
        /// TSO store buffers: a store lands at the fence (or a later step).
        pending_inflight: bool = false,
        pending_releasing: bool = false,
        passed: bool = false,
        drained: bool = false,
        /// The node lock the apply arm and the snapshot share.
        node_locked: bool = false,
        /// The record landed in the node (the apply arm's push).
        record_in_node: bool = false,
        /// The closer's snapshot taken (and whether it held the record).
        snapshotted: bool = false,
        snapshot_has_record: bool = false,
        /// The apply arm's verdict as read (check-then-lock reads early).
        verdict_allowed: bool = false,
        applied: bool = false,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.pc[0] == 4,
                1 => self.pc[1] == 5,
                2 => self.pc[2] == 4,
                else => unreachable,
            };
        }

        fn drainOp(self: *Self) void {
            if (self.pending_inflight) {
                self.inflight += 1;
                self.pending_inflight = false;
            }
        }

        fn drainCloser(self: *Self) void {
            if (self.pending_releasing) {
                self.releasing = 1;
                self.pending_releasing = false;
            }
        }

        /// The belt's verdict for a token-less apply, read under (or,
        /// weakened, before) the lock: `releasing ⇒ refused`.
        fn verdict(self: *const Self) bool {
            return self.releasing == 0;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => switch (self.pc[0]) {
                    0 => {
                        // inflight.fetch_add(1): buffered under TSO.
                        self.pending_inflight = true;
                        self.pc[0] = 1;
                    },
                    1 => {
                        if (w.op_fence) self.drainOp();
                        self.pc[0] = 2;
                    },
                    2 => {
                        // load(releasing): passed iff open; else the
                        // token is backed out (`leaveToken`, an RMW).
                        if (self.releasing == 0) {
                            self.passed = true;
                        } else {
                            self.drainOp();
                            self.inflight -= 1;
                        }
                        self.pc[0] = 3;
                    },
                    3 => {
                        self.drainOp();
                        self.pc[0] = 4;
                    },
                    else => unreachable,
                },
                1 => switch (self.pc[1]) {
                    0 => {
                        self.pending_releasing = true;
                        self.pc[1] = 1;
                    },
                    1 => {
                        if (w.closer_fence) self.drainCloser();
                        self.pc[1] = 2;
                    },
                    2 => {
                        // load(inflight): drained iff zero.
                        if (self.inflight == 0) self.drained = true;
                        self.pc[1] = 3;
                    },
                    3 => {
                        self.drainCloser();
                        // The flush: lock the node, snapshot it (a drained
                        // closer; an undrained one would wait for the
                        // token — modelled by snapshotting whatever is
                        // there once drained).
                        if (self.drained and !self.node_locked) {
                            self.node_locked = true;
                            self.pc[1] = 4;
                        } else if (!self.drained) {
                            // Not drained: the real closer waits for the
                            // token; the model lets it finish without a
                            // snapshot (nothing to assert).
                            self.pc[1] = 5;
                        }
                    },
                    4 => {
                        self.snapshotted = true;
                        self.snapshot_has_record = self.record_in_node;
                        self.node_locked = false;
                        self.pc[1] = 5;
                    },
                    else => unreachable,
                },
                2 => switch (self.pc[2]) {
                    0 => {
                        if (w.check_then_lock) {
                            self.verdict_allowed = self.verdict();
                            self.pc[2] = 1;
                        } else {
                            // lock first; the verdict comes under it.
                            if (!self.node_locked) {
                                self.node_locked = true;
                                self.pc[2] = 2;
                            }
                        }
                    },
                    1 => {
                        if (!self.node_locked) {
                            self.node_locked = true;
                            self.pc[2] = 3;
                        }
                    },
                    2 => {
                        self.verdict_allowed = self.verdict();
                        self.pc[2] = 3;
                    },
                    3 => {
                        if (self.verdict_allowed) {
                            self.record_in_node = true;
                            self.applied = true;
                        }
                        self.node_locked = false;
                        self.pc[2] = 4;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.passed and self.drained) return error.PassedCommitEscapedDrain;
            if (self.snapshotted and self.applied and !self.snapshot_has_record) return error.AppliedAfterSnapshot;
        }

        pub fn final(_: *const Self) !void {}
    };
}

test "the door's pair holds with both fences and the verdict under the lock; each fence removed fails (the pair's weakening); check-then-lock fails (the apply arm's weakening)" {
    try explorer.expectHolds(Model(.{}), .{});
    try explorer.expectFails(Model(.{ .op_fence = false }), .{}, error.InvariantViolated);
    try explorer.expectFails(Model(.{ .closer_fence = false }), .{}, error.InvariantViolated);
    try explorer.expectFails(Model(.{ .check_then_lock = true }), .{}, error.InvariantViolated);
}

test "rung 5's cutover gate is the two-thread special case: holds with both fences, fails with either removed" {
    try explorer.expectHolds(Model(.{ .committers = 1 }), .{});
    try explorer.expectFails(Model(.{ .committers = 1, .op_fence = false }), .{}, error.InvariantViolated);
    try explorer.expectFails(Model(.{ .committers = 1, .closer_fence = false }), .{}, error.InvariantViolated);
}
