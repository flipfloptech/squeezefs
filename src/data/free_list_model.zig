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
//! | 3 `a_quarantine_racing_a_finish_never_leaves_a_flagged_block_on_the_free_list` (0003 §Y.3) | the finish: its gate (OWED by a CAS that requires the flag), the publication (state → bit → budget), then the RE-CHECK of the flag and, set, the take-back (entitlement → bit → `limbo`, OWED by the same CAS — else published again); the admission: the flag, then the state read and, `free`, its own take (entitlement → bit → `quarantined` OWED); a claimant (entitlement → bit → `allocated`); optionally the cohort's release after the admission (ONE `fetchAnd` of the flag and OWED; an owed publish runs) | without the release: the flagged block ends off the free list (its bit set, no credit) and owned — `allocated` by the claimant, or `limbo`/`quarantined` with its publish OWED; with it: the block ends on the free list exactly once or the claimant's, nothing left OWED | `no_recheck` (the shipped finish: an admission reading `limbo` between the gate and the publication leaves the block free-listed with the flag), `recheck_before_state` (the re-check before the publication's state store: the same window), `owe_unconditional` (the take-back's plain OWED after the release cleared the cohort: an orphan), `gate_two_steps` (the shipped gate: the flag read, the release, then the OWED mark — the same orphan) |
//! | 4 `a_holders_carve_never_meets_a_bit_taken_between_its_scan_and_its_take` (0003 §Y.4) | the carve lock{scan the lowest clear bit; take it — the assert; debit if `free`; `virgin`} then the mint; a non-carve taker (the trim claim; the quarantine's free arm is the same take) lock{state `free`, entitlement, bit} → `trim_windowed` → its return: the state `free`, then lock{bit, credit} | the carve's take never finds its scanned bit set; the budget never underflows; at the end the block is the carve's (`allocated`, no credit) or back on the list (`free`, one credit) | `take_outside_lock` (the shipped trim claim and quarantine: a take between the carve's scan and its take — the carve's assert), `return_outside_lock` (the trim's bit cleared outside the lock: the carve debits a credit not yet landed) |

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
// Model 3: a quarantine admission racing a finish (the Dekker pair), and the
// cohort's release racing the OWED marks.
// ---------------------------------------------------------------------------

const QUARANTINED: u8 = 6;

pub const QuarantineWeakening = enum {
    none,
    /// The shipped finish: the gate, then the publication — no re-check.
    no_recheck,
    /// A re-check placed BEFORE the publication's state store: the
    /// admission's state read can still meet `limbo` after it.
    recheck_before_state,
    /// The take-back marks OWED by a plain RMW: a release that cleared the
    /// cohort first leaves the block nobody's.
    owe_unconditional,
    /// The finish's gate reads the flag, then marks OWED in a second step
    /// (the shipped gate): the same orphan.
    gate_two_steps,
};

/// Block X is in limbo (its bit set, no credit) and its finish is running;
/// an admission and a claimant race it; with `with_release`, the cohort's
/// release follows the admission (the jobs sink's admit → preempt →
/// release) and races the OWED marks. The finish's state store and its
/// flag re-check are seq_cst, as are the admission's flag RMW and state
/// read (block_alloc.zig: the Dekker pair) — the model's SC steps.
pub fn QuarantineFinishModel(comptime weakening: QuarantineWeakening, comptime with_release: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = if (with_release) 4 else 3;
        const P = 0;
        const Q = 1;
        const C = 2;
        const R = 3;
        const END: u8 = 0xff;

        flag: u8 = 0,
        owed: u8 = 0,
        state: u8 = LIMBO,
        bit: u8 = 1,
        budget: u8 = 0,
        double_free: u8 = 0,
        p_pc: u8 = 0,
        q_pc: u8 = 0,
        c_pc: u8 = 0,
        r_pc: u8 = 0,
        c_took: u8 = 0,
        /// The publication's re-check runs once per publication; `pub_n`
        /// counts publications (the finish's, a take-back's re-publish,
        /// the release's).
        pub_n: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                P => self.p_pc == END,
                Q => self.q_pc == END,
                C => self.c_pc == END,
                R => self.r_pc == END,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                P => self.stepFinish(),
                Q => self.stepAdmit(),
                C => self.stepClaim(),
                R => self.stepRelease(),
                else => unreachable,
            }
        }

        fn clearBit(self: *Self) void {
            if (self.bit == 0) self.double_free = 1;
            self.bit = 0;
        }

        /// `finishFree`: the gate; the publication; the re-check; the
        /// take-back; its OWED mark (or the re-publish).
        fn stepFinish(self: *Self) void {
            switch (self.p_pc) {
                0 => {
                    // The gate: OWED by a CAS that requires the flag.
                    if (self.flag != 0) {
                        if (weakening == .gate_two_steps) {
                            self.p_pc = 20;
                            return;
                        }
                        self.owed = 1;
                        self.p_pc = END;
                        return;
                    }
                    self.p_pc = if (weakening == .recheck_before_state) 10 else 1;
                },
                20 => {
                    self.owed = 1; // the shipped second step
                    self.p_pc = END;
                },
                10 => {
                    if (self.flag != 0) {
                        self.owed = 1;
                        self.p_pc = END;
                        return;
                    }
                    self.p_pc = 1;
                },
                1 => {
                    self.state = FREE;
                    self.p_pc = 2;
                },
                2 => {
                    self.clearBit();
                    self.p_pc = 3;
                },
                3 => {
                    self.budget += 1;
                    self.pub_n += 1;
                    self.p_pc = if (weakening == .no_recheck or weakening == .recheck_before_state) END else 4;
                },
                4 => self.p_pc = if (self.flag != 0) 5 else END,
                5 => {
                    if (self.budget == 0) {
                        self.p_pc = END;
                        return;
                    }
                    self.budget -= 1;
                    self.p_pc = 6;
                },
                6 => {
                    if (self.bit != 0) {
                        self.budget += 1;
                        self.p_pc = END;
                        return;
                    }
                    self.bit = 1;
                    self.p_pc = 7;
                },
                7 => {
                    self.state = LIMBO;
                    self.p_pc = 8;
                },
                8 => {
                    // `oweOrRepublish`: OWED only while the cohort holds it.
                    if (weakening == .owe_unconditional or self.flag != 0) {
                        self.owed = 1;
                        self.p_pc = END;
                        return;
                    }
                    self.p_pc = 1; // released meanwhile: publish again
                },
                else => unreachable,
            }
        }

        /// `quarantineBlock`: the flag, then the state read; `free` takes
        /// the block out (entitlement → bit); anything else is the flag alone.
        fn stepAdmit(self: *Self) void {
            switch (self.q_pc) {
                0 => {
                    self.flag = 1;
                    self.q_pc = 1;
                },
                1 => self.q_pc = if (self.state == FREE) 2 else END,
                2 => {
                    if (self.budget == 0) {
                        self.q_pc = END;
                        return;
                    }
                    self.budget -= 1;
                    self.q_pc = 3;
                },
                3 => {
                    if (self.bit != 0) {
                        self.budget += 1;
                        self.q_pc = END;
                        return;
                    }
                    self.bit = 1;
                    self.q_pc = 4;
                },
                4 => {
                    self.state = QUARANTINED;
                    self.owed = 1;
                    self.q_pc = END;
                },
                else => unreachable,
            }
        }

        /// A free-list claim: entitlement → bit → `allocated`.
        fn stepClaim(self: *Self) void {
            switch (self.c_pc) {
                0 => {
                    if (self.budget == 0) {
                        self.c_pc = END;
                        return;
                    }
                    self.budget -= 1;
                    self.c_pc = 1;
                },
                1 => {
                    if (self.bit != 0) {
                        self.budget += 1;
                        self.c_pc = END;
                        return;
                    }
                    self.bit = 1;
                    self.c_pc = 2;
                },
                2 => {
                    self.state = ALLOCATED;
                    self.c_took = 1;
                    self.c_pc = END;
                },
                else => unreachable,
            }
        }

        /// The cohort's release after the admission (`releaseQuarantinedCell`):
        /// ONE `fetchAnd` of the flag and OWED; an owed publish runs (its
        /// re-check finds the flag gone).
        fn stepRelease(self: *Self) void {
            switch (self.r_pc) {
                0 => {
                    if (self.q_pc != END) return;
                    const was_owed = self.flag != 0 and self.owed != 0;
                    self.flag = 0;
                    self.owed = 0;
                    self.r_pc = if (was_owed) 1 else END;
                },
                1 => {
                    self.state = FREE;
                    self.r_pc = 2;
                },
                2 => {
                    self.clearBit();
                    self.r_pc = 3;
                },
                3 => {
                    self.budget += 1;
                    self.pub_n += 1;
                    self.r_pc = END;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.double_free != 0) return error.DoubleFree;
        }

        pub fn final(self: *const Self) !void {
            if (!with_release) {
                if (self.bit == 0) return error.FlaggedBlockOnFreeList;
                if (self.budget != 0) return error.BudgetDrift;
                switch (self.state) {
                    ALLOCATED => if (self.c_took == 0) return error.Unowned,
                    LIMBO, QUARANTINED => if (self.owed == 0) return error.LostBlock,
                    else => return error.Unowned,
                }
                return;
            }
            // Released: the block is on the free list once, or the claim's.
            if (self.owed != 0) return error.StuckOwed;
            switch (self.state) {
                ALLOCATED => if (self.c_took == 0 or self.bit != 1 or self.budget != 0) return error.Unowned,
                FREE => if (self.bit != 0 or self.budget != 1) return error.BudgetDrift,
                else => return error.LostBlock,
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Model 4: the holder's non-carve takers under its ordering lock.
// ---------------------------------------------------------------------------

pub const HolderTakerWeakening = enum {
    none,
    /// The shipped trim claim and quarantine: the entitlement and the bit
    /// taken without the holder's lock.
    take_outside_lock,
    /// The trim's return clears its bit and credits outside the lock.
    return_outside_lock,
};

/// Block X is free on the holder (bit clear, one credit). The carve wants
/// one block; the taker trims X and returns it. `lock` = 0 free, 1 the
/// carve, 2 the taker; a thread meeting a held lock spins (its step
/// changes nothing).
pub fn HolderTakerModel(comptime weakening: HolderTakerWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const K = 0;
        const T = 1;
        const END: u8 = 0xff;

        bit: u8 = 0,
        state: u8 = FREE,
        budget: u8 = 1,
        lock: u8 = 0,
        underflow: u8 = 0,
        carve_assert: u8 = 0,
        k_pc: u8 = 0,
        k_pick: u8 = 0,
        t_pc: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                K => self.k_pc == END,
                T => self.t_pc == END,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                K => self.stepCarve(),
                T => self.stepTaker(),
                else => unreachable,
            }
        }

        fn acquire(self: *Self, who: u8) bool {
            if (self.lock != 0) return false;
            self.lock = who;
            return true;
        }

        /// `DataBitmap.carveRun` over one block, then the mint.
        fn stepCarve(self: *Self) void {
            switch (self.k_pc) {
                0 => {
                    if (self.acquire(1)) self.k_pc = 1;
                },
                1 => {
                    if (self.bit == 0) {
                        self.k_pick = 1;
                        self.k_pc = 2;
                    } else {
                        self.lock = 0; // Full
                        self.k_pc = END;
                    }
                },
                2 => {
                    // `holderTakeBit` and its `assert(ok)`.
                    if (self.bit != 0) {
                        self.carve_assert = 1;
                    } else {
                        self.bit = 1;
                        if (self.state == FREE) {
                            if (self.budget == 0) self.underflow = 1 else self.budget -= 1;
                        }
                        self.state = VIRGIN;
                    }
                    self.k_pc = 3;
                },
                3 => {
                    self.lock = 0;
                    self.k_pc = 4;
                },
                4 => {
                    self.state = ALLOCATED;
                    self.k_pc = END;
                },
                else => unreachable,
            }
        }

        /// `claimFreeForTrim` then `returnFromTrim`.
        fn stepTaker(self: *Self) void {
            const locked_take = weakening != .take_outside_lock;
            const locked_return = weakening != .return_outside_lock;
            switch (self.t_pc) {
                0 => {
                    if (!locked_take) {
                        self.t_pc = 1;
                    } else if (self.acquire(2)) self.t_pc = 1;
                },
                1 => {
                    if (self.state != FREE or self.budget == 0) {
                        if (locked_take) self.lock = 0;
                        self.t_pc = END;
                        return;
                    }
                    self.budget -= 1;
                    self.t_pc = 2;
                },
                2 => {
                    if (self.bit != 0) {
                        self.budget += 1;
                        if (locked_take) self.lock = 0;
                        self.t_pc = END;
                        return;
                    }
                    self.bit = 1;
                    self.t_pc = 3;
                },
                3 => {
                    if (locked_take) self.lock = 0;
                    self.t_pc = 4;
                },
                4 => {
                    self.state = TRIM;
                    self.t_pc = 5;
                },
                5 => {
                    self.state = FREE; // the return: the state first
                    self.t_pc = 6;
                },
                6 => {
                    if (!locked_return) {
                        self.bit = 0;
                        self.t_pc = 7;
                    } else if (self.acquire(2)) self.t_pc = 7;
                },
                7 => {
                    if (locked_return) self.bit = 0;
                    self.budget += 1;
                    self.t_pc = 8;
                },
                8 => {
                    if (locked_return) self.lock = 0;
                    self.t_pc = END;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.carve_assert != 0) return error.CarveTookASetBit;
            if (self.underflow != 0) return error.BudgetUnderflow;
        }

        pub fn final(self: *const Self) !void {
            if (self.k_pick != 0) {
                if (self.bit != 1 or self.state != ALLOCATED) return error.CarveLost;
                if (self.budget != 0) return error.PhantomCredit;
            } else {
                if (self.bit != 0 or self.state != FREE) return error.TrimNotReturned;
                if (self.budget != 1) return error.BudgetDrift;
            }
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

test "3 a_quarantine_racing_a_finish_never_leaves_a_flagged_block_on_the_free_list — holds with and without the cohort's release; the shipped finish (no re-check), a re-check before the publication, an unconditional OWED mark and the two-step gate each fail" {
    try explorer.expectHolds(QuarantineFinishModel(.none, false), .{});
    try explorer.expectHolds(QuarantineFinishModel(.none, true), .{});
    try explorer.expectFails(QuarantineFinishModel(.no_recheck, false), .{}, error.FinalViolated);
    try explorer.expectFails(QuarantineFinishModel(.recheck_before_state, false), .{}, error.FinalViolated);
    try explorer.expectFails(QuarantineFinishModel(.owe_unconditional, true), .{}, error.FinalViolated);
    try explorer.expectFails(QuarantineFinishModel(.gate_two_steps, true), .{}, error.FinalViolated);
    // The shipped window by hand: the gate passes, the admission reads
    // `limbo` (the flag alone), the publication lists the flagged block.
    try std.testing.expectError(error.FlaggedBlockOnFreeList, replay(QuarantineFinishModel(.no_recheck, false), &.{ 0, 1, 1, 0, 0, 0, 2 }).final());
    // The same schedule on the law: the re-check takes it back OWED.
    var m = replay(QuarantineFinishModel(.none, false), &.{ 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 2 });
    try m.final();
    // The orphan: the take-back's block is limbo when the release clears
    // the cohort, then a plain OWED mark lands on nobody's block.
    try std.testing.expectError(error.StuckOwed, replay(QuarantineFinishModel(.owe_unconditional, true), &.{ 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 3, 0, 2 }).final());
}

test "4 a_holders_carve_never_meets_a_bit_taken_between_its_scan_and_its_take — holds; a take outside the holder's lock (the carve's assert) and a return outside it (an underflow) each fail" {
    try explorer.expectHolds(HolderTakerModel(.none), .{});
    try explorer.expectFails(HolderTakerModel(.take_outside_lock), .{}, error.InvariantViolated);
    try std.testing.expectError(error.CarveTookASetBit, replay(HolderTakerModel(.take_outside_lock), &.{ 0, 0, 1, 1, 1, 0 }).invariant());
    try explorer.expectFails(HolderTakerModel(.return_outside_lock), .{}, error.InvariantViolated);
}

test "state counts: the models are small enough to re-run on every gate" {
    var report: explorer.Report = undefined;
    try explorer.explore(LocalModel(.none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 20 and report.states < 20_000);
    try explorer.explore(HolderModel(.none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 3 and report.states < 200);
    try explorer.explore(QuarantineFinishModel(.none, true), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 20 and report.states < 50_000);
    try explorer.explore(HolderTakerModel(.none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 10 and report.states < 5_000);
}
