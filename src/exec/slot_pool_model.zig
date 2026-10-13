//! The exhaustive MODEL of a lane's fiber slot pool against the idle stack
//! trimmer (AGENTS.md "Model checks"; `exec/lane.zig` `spawn`, `recycle`,
//! `trimIdleStacks`; decision 0003 §W.4). Standalone, dependency-free
//! value type; every field a byte; ONE atomic action per `step`.
//!
//! The pool is the free ring (modelled as a set: a pop takes any slot it
//! holds) plus one claim word per slot: FREE (quiescent: in the ring or on
//! its way back), ARMED (a spawn owns the stack), TRIMMING (the trimmer is
//! returning its pages). The trimmer claims a FREE slot IN PLACE — it
//! never takes a slot out of the ring — and a spawn claims the slot it
//! popped (FREE → ARMED), waiting out a trim. A recycle stores FREE only
//! after the fiber switched out, pushes the slot back, and only then
//! drops the live count, so `live == 0` means every slot is in the ring.
//!
//! Two slots: slot 0 runs a fiber the recycler R finishes, slot 1 is free.
//! The trimmer T makes one pass over both slots. The quiescer Q waits for
//! `live == 0`, then spawns onto BOTH slots (the capacity: every one must
//! be obtainable once nothing is live).
//!
//! | model | invariant | weakening that fails it |
//! |---|---|---|
//! | `the_trimmer_never_trims_a_running_stack_and_never_shorts_the_pool` | T never trims a stack in use; Q's pops after `live == 0` always find a slot | `trimmer_pops` (the shipped trimmer: it popped the slot out of the ring — Q finds the pool short), `decrement_before_push` (the shipped recycle: `live` dropped before the push — Q finds the pool short), `free_before_switch_out` (the slot released while its fiber still runs — T trims a running stack), `spawn_skips_claim` (a spawn using its slot without the claim — T trims a running stack) |

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const FREE: u8 = 0;
const ARMED: u8 = 1;
const TRIMMING: u8 = 2;

pub const Weakening = enum {
    none,
    trimmer_pops,
    decrement_before_push,
    free_before_switch_out,
    spawn_skips_claim,
};

pub fn SlotPoolModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const R = 0;
        const T = 1;
        const Q = 2;

        /// Bit i set = slot i is in the free ring.
        ring: u8 = 0b10,
        claim: [2]u8 = .{ ARMED, FREE },
        in_use: [2]u8 = .{ 1, 0 },
        live: u8 = 1,
        /// T trimmed a stack in use / Q found the pool short after `live == 0`.
        trimmed_running: u8 = 0,
        pool_short: u8 = 0,
        r_pc: u8 = 0,
        t_pc: u8 = 0,
        /// The trimmer's slot under way (`trimmer_pops`: the one it popped).
        t_slot: u8 = 0,
        t_held: u8 = 0,
        q_pc: u8 = 0,
        q_slot: [2]u8 = .{ 0xff, 0xff },

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                R => self.r_pc == 4,
                T => self.t_pc == 6,
                Q => self.q_pc == 6,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                R => self.stepRecycle(),
                T => self.stepTrimmer(),
                Q => self.stepQuiescer(),
                else => unreachable,
            }
        }

        /// The fiber on slot 0 finishes: its switch-out, the FREE store,
        /// the push, the live decrement (each weakening reorders two).
        fn stepRecycle(self: *Self) void {
            const order: [4]u8 = switch (weakening) {
                .free_before_switch_out => .{ 1, 0, 2, 3 },
                .decrement_before_push => .{ 0, 1, 3, 2 },
                else => .{ 0, 1, 2, 3 },
            };
            switch (order[self.r_pc]) {
                0 => self.in_use[0] = 0,
                1 => self.claim[0] = FREE,
                2 => self.ring |= 1,
                3 => self.live -= 1,
                else => unreachable,
            }
            self.r_pc += 1;
        }

        /// One pass over slots 0 and 1: claim (or, shipped, pop), trim,
        /// release (or push back).
        fn stepTrimmer(self: *Self) void {
            const slot: u8 = self.t_pc / 3;
            const m: u8 = @as(u8, 1) << @intCast(slot);
            switch (self.t_pc % 3) {
                0 => {
                    if (weakening == .trimmer_pops) {
                        if (self.ring & m != 0) {
                            self.ring &= ~m;
                            self.t_held = 1;
                        } else self.t_held = 0;
                    } else {
                        self.t_held = @intFromBool(self.claim[slot] == FREE);
                        if (self.t_held == 1) self.claim[slot] = TRIMMING;
                    }
                    self.t_slot = slot;
                    // Nothing to trim: the next slot.
                    if (self.t_held == 0) {
                        self.t_pc += 3;
                        return;
                    }
                },
                1 => if (self.in_use[slot] != 0) {
                    self.trimmed_running = 1;
                },
                2 => {
                    if (weakening == .trimmer_pops) self.ring |= m else self.claim[slot] = FREE;
                    self.t_held = 0;
                },
                else => unreachable,
            }
            self.t_pc += 1;
        }

        /// Wait for `live == 0`, then two spawns: pop, claim, run.
        fn stepQuiescer(self: *Self) void {
            switch (self.q_pc) {
                0 => if (self.live == 0) {
                    self.q_pc = 1;
                },
                1, 3 => {
                    const i = (self.q_pc - 1) / 2;
                    const pick: u8 = if (self.ring & 1 != 0) 0 else if (self.ring & 2 != 0) 1 else 0xff;
                    if (pick == 0xff) {
                        self.pool_short = 1;
                        self.q_pc = 6;
                        return;
                    }
                    self.ring &= ~(@as(u8, 1) << @intCast(pick));
                    self.q_slot[i] = pick;
                    self.q_pc += 1;
                },
                2, 4 => {
                    const i = (self.q_pc - 2) / 2;
                    const s = self.q_slot[i];
                    if (weakening == .spawn_skips_claim) {
                        self.in_use[s] = 1;
                        self.q_pc += 1;
                        return;
                    }
                    // CAS FREE → ARMED; a TRIMMING slot is waited out.
                    if (self.claim[s] == FREE) {
                        self.claim[s] = ARMED;
                        self.in_use[s] = 1;
                        self.q_pc += 1;
                    }
                },
                5 => self.q_pc = 6,
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.trimmed_running != 0) return error.TrimmedARunningStack;
            if (self.pool_short != 0) return error.PoolShortAfterQuiescence;
        }

        pub fn final(self: *const Self) !void {
            if (self.ring != 0 or self.live != 0) return error.PoolDrift;
            for (self.in_use) |u| if (u != 1) return error.SpawnLost;
            // The claim-less spawn's whole point is that it never arms.
            if (weakening != .spawn_skips_claim) for (self.claim) |c| if (c != ARMED) return error.ClaimDrift;
        }
    };
}

test "the_trimmer_never_trims_a_running_stack_and_never_shorts_the_pool — holds; the shipped trimmer (pops) and the shipped recycle (decrement before push) short the pool; a release before the switch-out and a spawn without its claim trim a running stack" {
    try explorer.expectHolds(SlotPoolModel(.none), .{});
    try explorer.expectFails(SlotPoolModel(.trimmer_pops), .{}, error.InvariantViolated);
    try explorer.expectFails(SlotPoolModel(.decrement_before_push), .{}, error.InvariantViolated);
    try explorer.expectFails(SlotPoolModel(.free_before_switch_out), .{}, error.InvariantViolated);
    try explorer.expectFails(SlotPoolModel(.spawn_skips_claim), .{}, error.InvariantViolated);
}

test "state counts: the slot pool model is small enough to re-run on every gate" {
    var report: explorer.Report = undefined;
    try explorer.explore(SlotPoolModel(.none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 20 and report.states < 20_000);
}
