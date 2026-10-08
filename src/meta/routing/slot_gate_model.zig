//! The cutover gate's Dekker pair as a standalone MODEL (charter §9
//! "Model-check every lock-free core … the park gate"; `docs/porting/
//! dynamic_routing.md` §3.4 "there is no third interleaving in which an
//! admitted op escapes the drain"). Dependency-free; explored exhaustively
//! by `model/explorer.zig` under TSO (a per-thread store buffer the fence
//! step drains).
//!
//! Threads: `op` (0) does `inflight += 1 ; fence ; load closed` and ENTERS
//! iff closed == 0 (else backs out); `closer` (1) does `closed = 1 ; fence ;
//! load inflight` and declares DRAINED iff inflight == 0. The invariant is
//! the gate's whole purpose: never (op entered ∧ closer drained) — an
//! admitted mutation never straddles a flip. Dropping EITHER fence lets
//! both buffered stores stay invisible while both loads read 0 (the TSO
//! store→load reordering): the weakening fails, as recorded.

const std = @import("std");
const explorer = @import("../../model/explorer.zig");

pub fn Model(comptime op_fence: bool, comptime closer_fence: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        inflight: u8 = 0,
        closed: u8 = 0,
        /// TSO store buffers: a store lands at the fence (or a later step).
        pending_inflight: bool = false,
        pending_closed: bool = false,
        entered: bool = false,
        drained: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 4;
        }

        fn drainOp(self: *Self) void {
            if (self.pending_inflight) {
                self.inflight += 1;
                self.pending_inflight = false;
            }
        }

        fn drainCloser(self: *Self) void {
            if (self.pending_closed) {
                self.closed = 1;
                self.pending_closed = false;
            }
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == 0) switch (self.pc[0]) {
                0 => {
                    // inflight.fetch_add(1): buffered under TSO.
                    self.pending_inflight = true;
                    self.pc[0] = 1;
                },
                1 => {
                    if (op_fence) self.drainOp();
                    self.pc[0] = 2;
                },
                2 => {
                    // load(closed): admitted iff open.
                    if (self.closed == 0) self.entered = true;
                    self.pc[0] = 3;
                },
                3 => {
                    self.drainOp();
                    self.pc[0] = 4;
                },
                else => unreachable,
            } else switch (self.pc[1]) {
                0 => {
                    self.pending_closed = true;
                    self.pc[1] = 1;
                },
                1 => {
                    if (closer_fence) self.drainCloser();
                    self.pc[1] = 2;
                },
                2 => {
                    // load(inflight): drained iff zero.
                    if (self.inflight == 0) self.drained = true;
                    self.pc[1] = 3;
                },
                3 => {
                    self.drainCloser();
                    self.pc[1] = 4;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.entered and self.drained) return error.AdmittedOpEscapedDrain;
        }

        pub fn final(_: *const Self) !void {}
    };
}

test "the gate's Dekker pair holds with both SeqCst fences and fails with either one removed (the recorded weakenings)" {
    try explorer.expectHolds(Model(true, true), .{});
    try explorer.expectFails(Model(false, true), .{}, error.InvariantViolated);
    try explorer.expectFails(Model(true, false), .{}, error.InvariantViolated);
    try explorer.expectFails(Model(false, false), .{}, error.InvariantViolated);
}
