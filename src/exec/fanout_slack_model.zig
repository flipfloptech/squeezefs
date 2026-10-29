//! The exhaustive MODEL of a FUSE lane's optional fan-out bound against
//! the handler its slot delivers (AGENTS.md "Model checks"; `exec/lane.zig`
//! `spawnOptional`, `spawnSlot`, `recycle`; rung-10-open-count-stranded).
//! Standalone, dependency-free value type; every field a byte; ONE atomic
//! action per `step`.
//!
//! The lane holds `H + K` fibers: H for the handlers its slots deliver, K
//! for optional fan-out (`Io.async`, `Io.Group.async`). An optional spawn
//! reserves a unit of K on one word with an RMW and gives it back when the
//! RMW found the word at K (the caller then runs the member inline); a
//! reserved spawn pops a slot; a recycle pushes the slot back and only then
//! returns the unit. A handler spawn pops a slot with no reservation.
//!
//! H = 1, K = 1: two optional spawners O1, O2 (each reserves, runs, and
//! recycles once) and one handler spawner D that must always find a slot.
//!
//! | model | invariant | weakening that fails it |
//! |---|---|---|
//! | `an_optional_fan_out_never_takes_a_handlers_fiber` | D never finds the pool empty; at most K optional fibers run | `check_then_add` (the bound read, then incremented in a second step: both spawners pass the check — D finds the pool empty), `no_bound` (the shipped spawn: optional fan-out takes any free slot — D finds the pool empty) |

const std = @import("std");
const explorer = @import("../model/explorer.zig");

pub const Weakening = enum { none, check_then_add, no_bound };

const H: u8 = 1;
const K: u8 = 1;

pub fn FanoutSlackModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const handler_t = 2;

        const Opc = enum(u8) { reserve, add, undo, pop, run, push, release, done };
        const Dpc = enum(u8) { pop, run, push, done };

        free: u8 = H + K,
        optional_live: u8 = 0,
        /// The value each optional spawner's reservation read.
        seen: [2]u8 = .{ 0, 0 },
        opc: [2]Opc = .{ .reserve, .reserve },
        running: [2]bool = .{ false, false },
        dpc: Dpc = .pop,
        handler_refused: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            if (t == handler_t) return self.dpc == .done;
            return self.opc[t] == .done;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == handler_t) return self.stepHandler();
            switch (self.opc[t]) {
                .reserve => switch (weakening) {
                    // fetchAdd: the read and the claim in one action.
                    .none => {
                        self.seen[t] = self.optional_live;
                        self.optional_live += 1;
                        self.opc[t] = if (self.seen[t] >= K) .undo else .pop;
                    },
                    .check_then_add => {
                        self.seen[t] = self.optional_live;
                        self.opc[t] = if (self.seen[t] >= K) .done else .add;
                    },
                    .no_bound => self.opc[t] = .pop,
                },
                .add => {
                    self.optional_live += 1;
                    self.opc[t] = .pop;
                },
                // Over the bound: the unit goes back, the member runs inline.
                .undo => {
                    self.optional_live -= 1;
                    self.opc[t] = .done;
                },
                .pop => {
                    if (self.free == 0) {
                        // The capacity refused it: the reservation goes back.
                        self.opc[t] = if (weakening == .no_bound) .done else .undo;
                        return;
                    }
                    self.free -= 1;
                    self.running[t] = true;
                    self.opc[t] = .run;
                },
                .run => {
                    self.running[t] = false;
                    self.opc[t] = .push;
                },
                .push => {
                    self.free += 1;
                    self.opc[t] = if (weakening == .no_bound) .done else .release;
                },
                .release => {
                    self.optional_live -= 1;
                    self.opc[t] = .done;
                },
                .done => unreachable,
            }
        }

        fn stepHandler(self: *Self) void {
            switch (self.dpc) {
                .pop => {
                    if (self.free == 0) {
                        self.handler_refused = true;
                        self.dpc = .done;
                        return;
                    }
                    self.free -= 1;
                    self.dpc = .run;
                },
                .run => self.dpc = .push,
                .push => {
                    self.free += 1;
                    self.dpc = .done;
                },
                .done => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.handler_refused) return error.HandlerRefused;
            if (@as(u8, @intFromBool(self.running[0])) + @intFromBool(self.running[1]) > K) return error.OverBound;
        }

        pub fn final(self: *const Self) !void {
            if (self.free != H + K) return error.SlotLost;
            if (self.optional_live != 0) return error.UnitLost;
        }
    };
}

test "an_optional_fan_out_never_takes_a_handlers_fiber — holds; a bound read and incremented in two steps, and no bound at all (the shipped spawn), refuse the handler" {
    try explorer.expectHolds(FanoutSlackModel(.none), .{});
    try explorer.expectFails(FanoutSlackModel(.check_then_add), .{}, error.InvariantViolated);
    try explorer.expectFails(FanoutSlackModel(.no_bound), .{}, error.InvariantViolated);
}
