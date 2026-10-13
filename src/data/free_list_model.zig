//! The exhaustive MODELS of `block_alloc.zig`'s free-list publication
//! (AGENTS.md "Model checks"; decision 0003 §W.1). Standalone,
//! dependency-free value types; every field a byte so the explorer's
//! content hash sees no padding; ONE atomic action per `step`.
//!
//! The publication of a freed block moves three words: the cell's STATE,
//! the bitmap BIT (the free list itself — a clear bit below the cursor is
//! a free block) and the free BUDGET (the entitlement a budget-then-bit
//! claim wins). The law the models prove: the state FIRST, then the bit
//! (its acq_rel clear is the release a claimant's acq_rel take acquires, so
//! the claimant's later `allocated` store lands after ours), then the
//! budget (bit before budget: a claim that won an entitlement finds its
//! clear bit). Every consumer wins an entitlement BEFORE it takes a bit,
//! except the holder's carve, which takes its bit under the holder's
//! ordering lock — the same lock the publication's bit and credit move
//! under, so the credit it debits is always there.
//!
//! | model | threads | invariant | weakening that fails it |
//! |---|---|---|---|
//! | 1 `a_claim_between_the_publication_stores_is_never_clobbered` (local free list) | publisher state→bit→budget; a budget-then-bit claim; a trim claim of the published block | the budget never underflows; at the end a clear bit reads `free`, a taken bit reads its taker's state, the budget equals the clear bits | `bit_first` (the shipped order: the claim's `allocated` is overwritten by `free`), `state_buffered` (the state store not ordered before the bit clear), `trim_bit_first` (the trim takes the bit then debits a credit not yet landed: underflow) |
//! | 2 `a_carve_between_the_publication_stores_is_never_clobbered` (holder) | publisher state→lock{bit, budget}; the carve lock{take, debit if free, virgin} then the mint's `allocated` | the budget never underflows; the minted block reads `allocated` with no phantom credit | `bit_first` (the shipped order: lock{bit} → state → budget — a carve between the state and the credit underflows, a carve + mint between the bit and the state is clobbered to `free` with a phantom credit), `credit_outside_lock` (the carve debits before the credit lands: underflow) |

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const FREE: u8 = 1;
const ALLOCATED: u8 = 2;
const LIMBO: u8 = 3;
const TRIM: u8 = 4;
const VIRGIN: u8 = 5;

// ---------------------------------------------------------------------------
// Model 1: the local (unarmed) free list.
// ---------------------------------------------------------------------------

pub const LocalWeakening = enum {
    none,
    /// The shipped order: the bit cleared, THEN the state stored, then the
    /// budget credited.
    bit_first,
    /// The state store stays in the publisher's buffer past the bit clear
    /// (the clear without its release half).
    state_buffered,
    /// `claimFreeForTrim` as shipped: the bit taken first, then a debit of
    /// a credit that may not have landed yet.
    trim_bit_first,
};

/// Block X (0) is in limbo with its bit set and is being published; block
/// Y (1) is already free (bit clear, one credit in the budget) — the claim
/// wins Y's credit and scans from the lowest index, so it meets X first.
pub fn LocalModel(comptime weakening: LocalWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const P = 0;
        const W = 1;
        const T = 2;
        const X = 0;

        /// Bit b SET = block b is not reallocatable now.
        bits: u8 = 0b01,
        state: [2]u8 = .{ LIMBO, FREE },
        budget: u8 = 1,
        /// The publisher's buffered state store (0 = none).
        pending_state: u8 = 0,
        underflow: u8 = 0,
        p_pc: u8 = 0,
        w_pc: u8 = 0,
        /// The block the claim picked / took (0xff = none).
        w_pick: u8 = 0xff,
        w_took: u8 = 0xff,
        t_pc: u8 = 0,
        t_took: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        fn drainP(self: *Self) void {
            if (self.pending_state != 0) {
                self.state[X] = self.pending_state;
                self.pending_state = 0;
            }
        }

        fn debit(self: *Self) void {
            if (self.budget == 0) self.underflow = 1 else self.budget -= 1;
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                P => self.p_pc == 4,
                W => self.w_pc == 4,
                T => self.t_pc == 4,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                P => self.stepPublisher(),
                W => self.stepClaim(),
                T => self.stepTrim(),
                else => unreachable,
            }
        }

        fn stepPublisher(self: *Self) void {
            switch (weakening) {
                .bit_first => switch (self.p_pc) {
                    0 => self.bits &= ~@as(u8, 1 << X),
                    1 => self.state[X] = FREE,
                    2 => self.budget += 1,
                    3 => {},
                    else => unreachable,
                },
                else => switch (self.p_pc) {
                    // The state store, buffered until the bit's release.
                    0 => self.pending_state = FREE,
                    1 => {
                        // The bit clear (acq_rel RMW): its release half
                        // drains the state store first.
                        if (weakening != .state_buffered) self.drainP();
                        self.bits &= ~@as(u8, 1 << X);
                    },
                    2 => self.budget += 1,
                    3 => self.drainP(),
                    else => unreachable,
                },
            }
            self.p_pc += 1;
        }

        /// `allocFromFreeList`: entitlement → lowest clear bit → take →
        /// `claimBlockIdx`'s `allocated` (its last store).
        fn stepClaim(self: *Self) void {
            switch (self.w_pc) {
                0 => {
                    if (self.budget == 0) {
                        self.w_pc = 4; // no entitlement: the fresh mint's business
                        return;
                    }
                    self.budget -= 1;
                    self.w_pc = 1;
                },
                1 => {
                    // The scan (a rescan loops here until a bit shows).
                    self.w_pick = if (self.bits & 1 == 0) 0 else if (self.bits & 2 == 0) 1 else 0xff;
                    if (self.w_pick != 0xff) self.w_pc = 2;
                },
                2 => {
                    const m = @as(u8, 1) << @intCast(self.w_pick);
                    if (self.bits & m == 0) {
                        self.bits |= m;
                        self.w_took = self.w_pick;
                        self.w_pc = 3;
                    } else self.w_pc = 1;
                },
                3 => {
                    self.state[self.w_took] = ALLOCATED;
                    self.w_pc = 4;
                },
                else => unreachable,
            }
        }

        /// `claimFreeForTrim(X)`: the state gate, then (fixed) the
        /// entitlement before the bit; (shipped) the bit before the debit.
        fn stepTrim(self: *Self) void {
            switch (self.t_pc) {
                0 => {
                    if (self.state[X] != FREE) {
                        self.t_pc = 4;
                        return;
                    }
                    self.t_pc = 1;
                },
                1 => if (weakening == .trim_bit_first) {
                    if (self.bits & 1 != 0) {
                        self.t_pc = 4;
                        return;
                    }
                    self.bits |= 1;
                    self.t_took = 1;
                    self.t_pc = 2;
                } else {
                    if (self.budget == 0) {
                        self.t_pc = 4;
                        return;
                    }
                    self.budget -= 1;
                    self.t_pc = 2;
                },
                2 => if (weakening == .trim_bit_first) {
                    self.debit();
                    self.t_pc = 3;
                } else {
                    if (self.bits & 1 != 0) {
                        self.budget += 1; // refund: the window closes
                        self.t_pc = 4;
                        return;
                    }
                    self.bits |= 1;
                    self.t_took = 1;
                    self.t_pc = 3;
                },
                3 => {
                    self.state[X] = TRIM;
                    self.t_pc = 4;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.underflow != 0) return error.BudgetUnderflow;
        }

        pub fn final(self: *const Self) !void {
            var clear: u8 = 0;
            for (0..2) |b| {
                const set = self.bits & (@as(u8, 1) << @intCast(b)) != 0;
                if (!set) {
                    clear += 1;
                    if (self.state[b] != FREE) return error.ClearBitNotFree;
                    continue;
                }
                const want: u8 = if (self.w_took == b) ALLOCATED else if (b == X and self.t_took == 1) TRIM else 0;
                if (want == 0) return error.SetBitWithoutTaker;
                if (self.state[b] != want) return error.TakenBlockClobbered;
            }
            if (self.budget != clear) return error.BudgetDrift;
        }
    };
}

// ---------------------------------------------------------------------------
// Model 2: the holder's free list (the bitmap behind its ordering lock).
// ---------------------------------------------------------------------------

pub const HolderWeakening = enum {
    none,
    /// The shipped order: lock{bit} → the state → the credit.
    bit_first,
    /// The state first, but the credit after the lock: the carve meets a
    /// `free` cell with a clear bit and debits before the credit lands.
    credit_outside_lock,
};

/// Block X is in limbo with its bit set; the budget holds no other credit.
/// The carve's critical section and the publication's are each ONE step
/// (the holder's ordering lock); the mint's `allocated` follows the carve.
pub fn HolderModel(comptime weakening: HolderWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const P = 0;
        const K = 1;

        bit: u8 = 1,
        state: u8 = LIMBO,
        budget: u8 = 0,
        underflow: u8 = 0,
        p_pc: u8 = 0,
        k_pc: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                P => self.p_pc == 3,
                K => self.k_pc == 2,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                P => self.stepPublisher(),
                K => self.stepCarve(),
                else => unreachable,
            }
        }

        fn stepPublisher(self: *Self) void {
            switch (weakening) {
                .none => switch (self.p_pc) {
                    0 => self.state = FREE,
                    1 => {
                        // lock{ bit 1→0; credit }
                        self.bit = 0;
                        self.budget += 1;
                    },
                    2 => {},
                    else => unreachable,
                },
                .bit_first => switch (self.p_pc) {
                    0 => self.bit = 0,
                    1 => self.state = FREE,
                    2 => self.budget += 1,
                    else => unreachable,
                },
                .credit_outside_lock => switch (self.p_pc) {
                    0 => self.state = FREE,
                    1 => self.bit = 0,
                    2 => self.budget += 1,
                    else => unreachable,
                },
            }
            self.p_pc += 1;
        }

        fn stepCarve(self: *Self) void {
            switch (self.k_pc) {
                0 => {
                    // lock{ the lowest clear bit taken; a `free` cell
                    // leaves the list (debit); `virgin` until the mint }.
                    // A set bit is no carve yet: the step repeats.
                    if (self.bit != 0) return;
                    self.bit = 1;
                    if (self.state == FREE) {
                        if (self.budget == 0) self.underflow = 1 else self.budget -= 1;
                    }
                    self.state = VIRGIN;
                    self.k_pc = 1;
                },
                1 => {
                    self.state = ALLOCATED; // `claimBlockIdx`
                    self.k_pc = 2;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.underflow != 0) return error.BudgetUnderflow;
        }

        pub fn final(self: *const Self) !void {
            if (self.bit != 1) return error.CarveLost;
            if (self.state != ALLOCATED) return error.MintedBlockClobbered;
            if (self.budget != 0) return error.PhantomCredit;
        }
    };
}

// ---------------------------------------------------------------------------
// Tests: every model holds; every recorded weakening fails.
// ---------------------------------------------------------------------------

test "1 a_claim_between_the_publication_stores_is_never_clobbered — holds; the shipped order, a buffered state store and a bit-first trim each fail" {
    try explorer.expectHolds(LocalModel(.none), .{});
    try explorer.expectFails(LocalModel(.bit_first), .{}, error.FinalViolated);
    try explorer.expectFails(LocalModel(.state_buffered), .{}, error.FinalViolated);
    try explorer.expectFails(LocalModel(.trim_bit_first), .{}, error.InvariantViolated);
}

/// One schedule, stepped by hand: names WHICH failure a schedule the
/// explorer may not report first reaches.
fn replay(comptime M: type, schedule: []const u8) M {
    var m = M.init();
    for (schedule) |t| m.step(t);
    return m;
}

test "2 a_carve_between_the_publication_stores_is_never_clobbered — holds; the shipped order and a credit outside the lock each fail" {
    try explorer.expectHolds(HolderModel(.none), .{});
    // The shipped order fails twice over: the explorer's first schedule is
    // the carve between the state and the credit (an absent credit
    // debited); the carve + mint between the bit and the state ends at
    // `free` on the minted block with a phantom credit (0003 §W.1).
    try explorer.expectFails(HolderModel(.bit_first), .{}, error.InvariantViolated);
    try std.testing.expectError(error.MintedBlockClobbered, replay(HolderModel(.bit_first), &.{ 0, 1, 1, 0, 0 }).final());
    try explorer.expectFails(HolderModel(.credit_outside_lock), .{}, error.InvariantViolated);
    // The same hand schedule on the law's order: the carve waits for the
    // bit, and every interleaving ends minted.
    var m = replay(HolderModel(.none), &.{ 0, 1, 1, 0, 1, 1 });
    try m.final();
}

test "state counts: the models are small enough to re-run on every gate" {
    var report: explorer.Report = undefined;
    try explorer.explore(LocalModel(.none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 20 and report.states < 20_000);
    try explorer.explore(HolderModel(.none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 3 and report.states < 200);
}
