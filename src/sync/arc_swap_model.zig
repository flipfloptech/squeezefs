//! Explorer models of the `ArcSwap` split-count protocol (`arc_swap.zig`;
//! charter §9 "Model-check every lock-free core").
//!
//! The model is a plain value type: two objects A and B with strong counts
//! and freed flags, one slot word `(ptr, claims)`, two readers that each
//! load twice (claim → maybe flush → access → release) and one storer that
//! publishes B over A and then drains the slot. Every `step` is ONE atomic
//! action. The claim field is 3 bits (`pool = 8`) with the flush at 2 so a
//! four-load schedule exercises the flush without wrapping the field — the
//! protocol is the shipped one, only the widths are shrunk.
//!
//! Weakening ledger (which change fails the model):
//!   naive        — load as "read pointer, then bump strong" (two steps), the
//!                  plain atomic-pointer + refcount shape: the storer frees
//!                  between them (InvariantViolated: a freed object touched).
//!   split_swap   — the storer swaps the pointer and reads/resets the claim
//!                  count in two steps: a claim landing between them is
//!                  charged to the wrong object (freed under a reader, and a
//!                  leaked unit on the other).
//!   flush_store  — the flush resets the claim field with a plain store
//!                  instead of a CAS: a claim landing between the reader's
//!                  fetch_add and its store is erased, and the reference it
//!                  pre-paid never returns (FinalViolated: a leaked object).

const std = @import("std");
const explorer = @import("../model/explorer.zig");
const arc_swap = @import("arc_swap.zig");

const Weakening = enum { none, naive, split_swap, flush_store };

/// Program counter of a finished thread.
const done_pc: u8 = 0xff;
const pool: i32 = 8;
const flush_at: i32 = 2;
const no_obj: u8 = 0xff;

fn Model(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const storer = 2;
        const loads_per_reader = 2;

        /// The slot word: pointer (object index or `no_obj`) and claims.
        ptr: u8 = 0,
        claims: i32 = 0,
        strong: [2]i32 = .{ if (weakening == .naive) 1 else pool, 1 },
        freed: [2]bool = .{ false, false },
        frees: [2]u8 = .{ 0, 0 },

        pc: [3]u8 = @splat(0),
        /// Per reader: the loaded pointer, the observed claims, loads done.
        rp: [2]u8 = .{ no_obj, no_obj },
        rc: [2]i32 = .{ 0, 0 },
        rounds: [2]u8 = .{ 0, 0 },
        /// The storer's displaced word.
        sp: u8 = no_obj,
        sc: i32 = 0,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == done_pc;
        }

        fn drop(self: *Self, obj: u8, n: i32) void {
            self.strong[obj] -= n;
            if (self.strong[obj] == 0) {
                self.freed[obj] = true;
                self.frees[obj] += 1;
            }
        }

        fn readerStep(self: *Self, t: usize) void {
            switch (weakening) {
                .naive => switch (self.pc[t]) {
                    0 => {
                        // Plain pointer load.
                        self.rp[t] = self.ptr;
                        self.pc[t] = 1;
                    },
                    1 => {
                        // The hazard window: bump strong on whatever was loaded.
                        self.strong[self.rp[t]] += 1;
                        self.pc[t] = 3;
                    },
                    3 => self.pc[t] = 4,
                    4 => {
                        self.drop(self.rp[t], 1);
                        self.nextRound(t);
                    },
                    else => unreachable,
                },
                else => switch (self.pc[t]) {
                    0 => {
                        // ONE fetch_add: pointer and claim in the same step.
                        self.rp[t] = self.ptr;
                        self.rc[t] = self.claims;
                        self.claims += 1;
                        self.pc[t] = if (self.rc[t] >= flush_at) 1 else 3;
                    },
                    1 => {
                        // Flush deposit: move the observed claims (+ ours) into strong.
                        self.strong[self.rp[t]] += self.rc[t] + 1;
                        self.pc[t] = 2;
                    },
                    2 => {
                        // Flush reset: CAS (ptr, rc+1) → (ptr, 0); a lost CAS withdraws.
                        if (weakening == .flush_store) {
                            self.claims = 0;
                            self.pc[t] = 3;
                        } else if (self.ptr == self.rp[t] and self.claims == self.rc[t] + 1) {
                            self.claims = 0;
                            self.pc[t] = 3;
                        } else {
                            self.pc[t] = 5;
                        }
                    },
                    5 => {
                        // The withdrawal of a lost flush (never reaches zero: our own claim is held).
                        self.strong[self.rp[t]] -= self.rc[t] + 1;
                        self.pc[t] = 3;
                    },
                    3 => {
                        // Access the object through the held reference.
                        self.pc[t] = 4;
                    },
                    4 => {
                        self.drop(self.rp[t], 1);
                        self.nextRound(t);
                    },
                    else => unreachable,
                },
            }
        }

        fn nextRound(self: *Self, t: usize) void {
            self.rounds[t] += 1;
            self.rp[t] = no_obj;
            self.pc[t] = if (self.rounds[t] == loads_per_reader) done_pc else 0;
        }

        fn storerStep(self: *Self) void {
            const t = storer;
            switch (weakening) {
                .naive => switch (self.pc[t]) {
                    0 => {
                        // swap ptr → B; the displaced A loses its slot reference.
                        self.sp = self.ptr;
                        self.ptr = 1;
                        self.pc[t] = 1;
                    },
                    1 => {
                        self.drop(self.sp, 1);
                        self.pc[t] = 2;
                    },
                    2 => {
                        // Slot deinit only after the last reader (a load
                        // after `deinit` is outside the contract).
                        if (!self.readersDone()) return;
                        self.sp = self.ptr;
                        self.ptr = no_obj;
                        self.pc[t] = 3;
                    },
                    3 => {
                        self.drop(self.sp, 1);
                        self.pc[t] = done_pc;
                    },
                    else => unreachable,
                },
                else => switch (self.pc[t]) {
                    0 => {
                        // Deposit B's pool (its one caller reference joins it).
                        self.strong[1] += pool - 1;
                        self.pc[t] = 1;
                    },
                    1 => {
                        // swap word → (B, 0): pointer and claims displaced together.
                        self.sp = self.ptr;
                        self.ptr = 1;
                        if (weakening == .split_swap) {
                            self.pc[t] = 6;
                        } else {
                            self.sc = self.claims;
                            self.claims = 0;
                            self.pc[t] = 2;
                        }
                    },
                    6 => {
                        // The split: the claims read-and-reset as a second step.
                        self.sc = self.claims;
                        self.claims = 0;
                        self.pc[t] = 2;
                    },
                    2 => {
                        // Reconcile: return the unclaimed rest of A's pool.
                        self.drop(self.sp, pool - self.sc);
                        self.pc[t] = 3;
                    },
                    3 => {
                        // Slot deinit: swap word → (none, 0), after the last reader.
                        if (!self.readersDone()) return;
                        self.sp = self.ptr;
                        self.ptr = no_obj;
                        if (weakening == .split_swap) {
                            self.pc[t] = 7;
                        } else {
                            self.sc = self.claims;
                            self.claims = 0;
                            self.pc[t] = 4;
                        }
                    },
                    7 => {
                        self.sc = self.claims;
                        self.claims = 0;
                        self.pc[t] = 4;
                    },
                    4 => {
                        self.drop(self.sp, pool - self.sc);
                        self.pc[t] = done_pc;
                    },
                    else => unreachable,
                },
            }
        }

        fn readersDone(self: *const Self) bool {
            return self.done(0) and self.done(1);
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == storer) self.storerStep() else self.readerStep(t);
        }

        pub fn invariant(self: *const Self) !void {
            for (0..2) |t| {
                // A reader that holds a pointer (from its load to its release)
                // must never see the object freed under it.
                if (self.rp[t] != no_obj and self.pc[t] != 0 and self.freed[self.rp[t]]) return error.UseAfterFree;
            }
            for (0..2) |o| {
                if (self.strong[o] < 0) return error.StrongUnderflow;
                if (self.frees[o] > 1) return error.DoubleFree;
            }
        }

        pub fn final(self: *const Self) !void {
            for (0..2) |o| {
                if (!self.freed[o]) return error.Leaked;
                if (self.strong[o] != 0) return error.StrongNonZero;
            }
        }
    };
}

const testing = std.testing;

test "arc_swap_load_store: the split-count protocol holds; naive two-step load fails" {
    try explorer.expectHolds(Model(.none), .{});
    try explorer.expectFails(Model(.naive), .{}, error.InvariantViolated);
}

test "arc_swap_load_store: a swap split into pointer-then-count fails" {
    try explorer.expectFails(Model(.split_swap), .{}, error.InvariantViolated);
}

test "arc_swap_load_store: a flush done as a store instead of a CAS fails (the erased claim is a reference that never returns)" {
    try explorer.expectFails(Model(.flush_store), .{}, error.FinalViolated);
}

test "arc_swap models: exhaustive within the explorer's default limits" {
    var report: explorer.Report = undefined;
    try explorer.explore(Model(.none), testing.allocator, .{}, &report);
    try testing.expect(report.states > 1);
    try testing.expect(report.states < 100_000);
}

// The model's shapes are the shipped ones: the word packs pointer and
// claims, a load claims exactly one, the flush fires at `flush_at` and
// resets the field, and the pool is the whole claim range.
test "tie: the shipped word arithmetic matches the model's" {
    try testing.expectEqual(@as(u64, 1) << 48, arc_swap.one_claim);
    try testing.expectEqual(@as(u64, 1) << 16, arc_swap.pool);
    try testing.expectEqual(arc_swap.pool / 2, arc_swap.flush_at);
    const Payload = struct { v: u8 };
    const a = try arc_swap.Arc(Payload).create(testing.allocator, .{ .v = 1 });
    var slot: arc_swap.ArcSwap(Payload) = .init(a);
    defer slot.deinit();
    try testing.expectEqual(arc_swap.pool, a.strongCount());
    const r1 = slot.load();
    try testing.expectEqual(@as(u64, 1), slot.claims());
    const r2 = slot.load();
    try testing.expectEqual(@as(u64, 2), slot.claims());
    // Claims are pre-paid: the strong count did not move.
    try testing.expectEqual(arc_swap.pool, a.strongCount());
    r1.release();
    r2.release();
    try testing.expectEqual(arc_swap.pool - 2, a.strongCount());
}
