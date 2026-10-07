//! The exhaustive MODELS of `block_alloc.zig`'s incarnation word and
//! refcount cell (AGENTS.md "Model checks"; charter §9 "the W1 clone/patch
//! fence pair"; `docs/porting/data_alloc_reclaim.md` §1.3 — the six loom
//! models ported to `model/explorer.zig`). Standalone, dependency-free
//! value types; every field a byte so the explorer's content hash sees no
//! padding; ONE atomic action per `step`.
//!
//! Memory model: sequential consistency plus an EXPLICIT per-thread store
//! buffer where a core depends on store ordering (the explorer's law). A
//! "fence" is a step that drains the buffer; a WEAKENING deletes that drain,
//! and the weakened model must fail (`expectFails`). The reader half of the
//! seqlock is a load-ordering fence: its weakening lets the validation load
//! be HOISTED above the payload load (modelled as an early load taken
//! before the payload read).
//!
//! | model | threads | invariant | weakening that fails it |
//! |---|---|---|---|
//! | 1 `validated_fill_never_serves_dead_bytes` | writer retire→payload→publish; reader snapshot→payload→still | validated ⇒ bytes' generation == snapshot's | `writer_fence`, `reader_fence` |
//! | 2 `two_reuse_cycles_still_exact` | two reuse cycles vs one fill | the same at generation precision | `writer_fence`, `reader_fence` |
//! | 3 `refcount_acquire_never_resurrects_freed_block` | cloner try_acquire vs owner release from 1 | ¬(freed ∧ acquired), freed ∨ acquired, the clone's release terminal, a second release false | `acquire_load_store` |
//! | 4 `refcount_exactly_one_terminal_release` | two releases from 2 | exactly one observes 1→0 | `release_load_store` |
//! | 5 `patch_clone_composed_never_mutates_a_validated_pin` | patch retire→fence→peek; clone acquire→fence→snapshot | ¬(pinned ∧ stable ∧ patched); re-stabilized; refcount conserved | `patch_fence`, `clone_fence` (store buffering admits the stale clone) |
//! | 6 `validated_fill_never_serves_mid_patch_bytes` | in-place two-half patch vs a validated fill | a validated fill never reads a TORN payload | `writer_fence`, `reader_fence` |

const std = @import("std");
const explorer = @import("../model/explorer.zig");

// ---------------------------------------------------------------------------
// Models 1, 2, 6: the seqlock against a validated fill.
// ---------------------------------------------------------------------------

pub const SeqlockWeakening = enum {
    none,
    /// `retire`'s trailing fence deleted: the retired word may land AFTER the
    /// payload bytes.
    writer_fence,
    /// `still`'s fence deleted: the validation load is hoisted above the
    /// payload load.
    reader_fence,
};

pub const FillConfig = struct {
    /// Retire→payload→publish cycles the writer runs (1 or 2).
    cycles: u8,
    /// Payload halves written per cycle (1 = one DMA; 2 = a mid-patch state
    /// exists between the halves).
    halves: u8,
    weakening: SeqlockWeakening = .none,
};

/// Word layout as in the product: `gen << 1 | stable`; the payload halves
/// carry the GENERATION whose bytes they hold.
pub fn FillModel(comptime cfg: FillConfig) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const W = 0;
        const R = 1;

        word: u8 = 1, // gen 0, stable
        /// The writer's buffered word store (0 = none).
        pending_word: u8 = 0,
        payload: [2]u8 = @splat(0),
        // writer
        w_pc: u8 = 0,
        w_cycle: u8 = 0,
        // reader
        r_pc: u8 = 0,
        snap: u8 = 0,
        early_word: u8 = 0,
        seen: [2]u8 = @splat(0),
        validated: u8 = 0,
        served: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        fn drain(self: *Self) void {
            if (self.pending_word != 0) {
                self.word = self.pending_word;
                self.pending_word = 0;
            }
        }

        fn visibleWord(self: *const Self) u8 {
            return self.word;
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                W => self.w_cycle == cfg.cycles,
                R => self.r_pc == 4 + cfg.halves,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                W => self.stepWriter(),
                R => self.stepReader(),
                else => unreachable,
            }
        }

        /// Writer: retire (buffered) → [fence] → payload halves → publish.
        fn stepWriter(self: *Self) void {
            switch (self.w_pc) {
                0 => {
                    // retire: ((gen)+1) << 1 — the store sits in the buffer.
                    const cur = if (self.pending_word != 0) self.pending_word else self.word;
                    self.pending_word = ((cur >> 1) + 1) << 1;
                    self.w_pc = 1;
                },
                1 => {
                    // The trailing fence drains the word store before any
                    // later data write; the weakened writer skips it.
                    if (cfg.weakening != .writer_fence) self.drain();
                    self.w_pc = 2;
                },
                else => {
                    const half = self.w_pc - 2;
                    if (half < cfg.halves) {
                        const gen = (if (self.pending_word != 0) self.pending_word else self.word) >> 1;
                        self.payload[half] = gen;
                        self.w_pc += 1;
                    } else {
                        // publish: an RMW drains the buffer, then sets stable.
                        self.drain();
                        self.word |= 1;
                        self.w_cycle += 1;
                        self.w_pc = 0;
                    }
                },
            }
        }

        /// Reader: snapshot → [hoisted validation load] → payload halves →
        /// still.
        fn stepReader(self: *Self) void {
            switch (self.r_pc) {
                0 => {
                    const w = self.visibleWord();
                    if (w & 1 == 0) {
                        // Unstable: the fill does not serve; done.
                        self.r_pc = 4 + cfg.halves;
                        return;
                    }
                    self.snap = w;
                    self.r_pc = 1;
                },
                1 => {
                    // With the reader fence the validation load happens at
                    // the end; without it the load may execute here.
                    if (cfg.weakening == .reader_fence) self.early_word = self.visibleWord();
                    self.r_pc = 2;
                },
                else => {
                    const half = self.r_pc - 2;
                    if (half < cfg.halves) {
                        self.seen[half] = self.payload[half];
                        self.r_pc += 1;
                    } else {
                        const w = if (cfg.weakening == .reader_fence) self.early_word else self.visibleWord();
                        self.validated = if (w == self.snap) 1 else 0;
                        self.served = 1;
                        self.r_pc = 4 + cfg.halves;
                    }
                },
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.served == 0 or self.validated == 0) return;
            const snap_gen = self.snap >> 1;
            // Every half a validated fill serves carries the snapshot's
            // generation: never dead bytes, never a torn mid-patch image.
            var h: u8 = 0;
            while (h < cfg.halves) : (h += 1) {
                if (self.seen[h] != snap_gen) return error.ValidatedFillServedForeignBytes;
            }
        }

        pub fn final(self: *const Self) !void {
            if (self.word & 1 == 0 or self.pending_word != 0) return error.WriterLeftUnstable;
            if (self.word >> 1 != cfg.cycles) return error.GenerationDrift;
        }
    };
}

// ---------------------------------------------------------------------------
// Models 3, 4: the refcount cell.
// ---------------------------------------------------------------------------

pub const RefcountWeakening = enum {
    none,
    /// `tryAcquire` as a load then a store instead of a CAS.
    acquire_load_store,
    /// `release` as a load then a store instead of a CAS.
    release_load_store,
};

/// Model 3: the owner releases from 1 while the cloner tries to acquire.
/// Post-join (the sequential half of the loom model): an acquired pin's
/// release is the terminal one, and a release on the saturated cell is
/// `false`.
pub fn AcquireVsReleaseModel(comptime weakening: RefcountWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const OWNER = 0;
        const CLONER = 1;

        rc: u8 = 1,
        o_pc: u8 = 0,
        o_tmp: u8 = 0,
        /// The owner's release observed 1→0 (the block is FREED).
        freed: u8 = 0,
        c_pc: u8 = 0,
        c_tmp: u8 = 0,
        acquired: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                OWNER => self.o_pc == 2,
                CLONER => self.c_pc == 2,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                OWNER => switch (self.o_pc) {
                    0 => {
                        self.o_tmp = self.rc;
                        self.o_pc = if (self.o_tmp == 0) 2 else 1;
                    },
                    1 => {
                        if (self.rc == self.o_tmp) {
                            self.rc = self.o_tmp - 1;
                            if (self.o_tmp == 1) self.freed = 1;
                            self.o_pc = 2;
                        } else self.o_pc = 0; // CAS failed: retry
                    },
                    else => unreachable,
                },
                CLONER => switch (self.c_pc) {
                    0 => {
                        self.c_tmp = self.rc;
                        // try_acquire from NONZERO only: 0 ⇒ refused, done.
                        self.c_pc = if (self.c_tmp == 0) 2 else 1;
                    },
                    1 => {
                        // The CAS (or, weakened, a plain store of tmp + 1).
                        if (weakening == .acquire_load_store or self.rc == self.c_tmp) {
                            self.rc = self.c_tmp + 1;
                            self.acquired = 1;
                            self.c_pc = 2;
                        } else self.c_pc = 0;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            // A FREED block is never acquired: the acquire can only have
            // preceded the owner's release, which then was not terminal.
            if (self.acquired == 1 and self.freed == 1) return error.FreedBlockResurrected;
            if (self.rc > 2) return error.RefcountOvershoot;
        }

        pub fn final(self: *const Self) !void {
            if (self.freed == 0 and self.acquired == 0) return error.NeitherFreedNorAcquired;
            // The sequential half: the clone's release is the terminal one
            // (1→0), and a release on the saturated cell is false.
            var cell = self.rc;
            if (self.acquired == 1) {
                if (cell != 1) return error.CloneReleaseNotTerminal;
                cell -= 1;
            }
            if (cell != 0) return error.RefcountNotZero;
        }
    };
}

/// Model 4: two releases from 2 — exactly one terminal.
pub fn TwoReleasesModel(comptime weakening: RefcountWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        rc: u8 = 2,
        pc: [2]u8 = @splat(0),
        tmp: [2]u8 = @splat(0),
        terminal: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 2;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (self.pc[t]) {
                0 => {
                    self.tmp[t] = self.rc;
                    self.pc[t] = if (self.tmp[t] == 0) 2 else 1;
                },
                1 => {
                    if (weakening == .release_load_store or self.rc == self.tmp[t]) {
                        self.rc = self.tmp[t] - 1;
                        if (self.tmp[t] == 1) self.terminal[t] = 1;
                        self.pc[t] = 2;
                    } else self.pc[t] = 0;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.terminal[0] == 1 and self.terminal[1] == 1) return error.TwoTerminalReleases;
        }

        pub fn final(self: *const Self) !void {
            if (self.terminal[0] + self.terminal[1] != 1) return error.NotExactlyOneTerminal;
            if (self.rc != 0) return error.RefcountNotZero;
        }
    };
}

// ---------------------------------------------------------------------------
// Model 5: the W1 patch/clone pair over two words.
// ---------------------------------------------------------------------------

pub const PairWeakening = enum {
    none,
    /// The patch's SeqCst fence between its retire and its refcount peek
    /// weakened to release/acquire: the word store may stay buffered.
    patch_fence,
    /// The clone's SeqCst fence between its pin and its snapshot weakened
    /// likewise: the refcount store may stay buffered.
    clone_fence,
};

pub fn PatchCloneModel(comptime weakening: PairWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const PATCH = 0;
        const CLONE = 1;

        word: u8 = 1, // gen 0, stable (STABLE_FIRST)
        rc: u8 = 1,
        /// The patch's buffered word store (0 = none).
        p_pending_word: u8 = 0,
        /// The clone's buffered refcount store (0 = none).
        c_pending_rc: u8 = 0,
        p_pc: u8 = 0,
        patched: u8 = 0,
        c_pc: u8 = 0,
        pinned: u8 = 0,
        snap_stable: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        fn drainPatch(self: *Self) void {
            if (self.p_pending_word != 0) {
                self.word = self.p_pending_word;
                self.p_pending_word = 0;
            }
        }

        fn drainClone(self: *Self) void {
            if (self.c_pending_rc != 0) {
                self.rc = self.c_pending_rc;
                self.c_pending_rc = 0;
            }
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                PATCH => self.p_pc == 4,
                CLONE => self.c_pc == 3,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                PATCH => switch (self.p_pc) {
                    0 => {
                        // retire: the RMW's store, buffered until the fence
                        // (the C11 shape without SeqCst: an AcqRel RMW on one
                        // word orders nothing against a later load of another).
                        self.p_pending_word = ((self.word >> 1) + 1) << 1;
                        self.p_pc = 1;
                    },
                    1 => {
                        // cross_word_fence (SeqCst): drains the store buffer.
                        if (weakening != .patch_fence) self.drainPatch();
                        self.p_pc = 2;
                    },
                    2 => {
                        // peek refcount == 1 ⇒ sole owner ⇒ patch in place.
                        self.patched = if (self.rc == 1) 1 else 0;
                        self.p_pc = 3;
                    },
                    3 => {
                        // publish (re-stabilize under the new generation, in
                        // both arms): an RMW drains the buffer.
                        self.drainPatch();
                        self.word |= 1;
                        self.p_pc = 4;
                    },
                    else => unreachable,
                },
                CLONE => switch (self.c_pc) {
                    0 => {
                        // try_acquire: the CAS's store, buffered likewise.
                        if (self.rc == 0) {
                            self.c_pc = 3;
                            return;
                        }
                        self.c_pending_rc = self.rc + 1;
                        self.pinned = 1;
                        self.c_pc = 1;
                    },
                    1 => {
                        if (weakening != .clone_fence) self.drainClone();
                        self.c_pc = 2;
                    },
                    2 => {
                        // snapshot == STABLE_FIRST: the PRE-PATCH stable word
                        // — the clone would copy bytes the patch is mutating
                        // (a post-publish word is a new generation and fine).
                        // The pin is HELD from here to the join.
                        self.snap_stable = if (self.word == 1) 1 else 0;
                        self.c_pc = 3;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            // The clone holds a pin taken under the pre-patch stable word
            // AND the patch saw itself sole owner: the patch mutates bytes
            // under a validated pin — the store-buffering outcome both
            // fences exist to forbid.
            if (self.pinned == 1 and self.snap_stable == 1 and self.patched == 1) return error.PatchUnderValidatedPin;
        }

        pub fn final(self: *const Self) !void {
            if (self.word & 1 == 0 or self.p_pending_word != 0) return error.PatchLeftUnstable;
            // Post-join: the clone's pending store has landed (the unpin
            // RMW drains it) and its sequential unpin conserves the count.
            var rc = if (self.c_pending_rc != 0) self.c_pending_rc else self.rc;
            if (self.pinned == 1) {
                if (rc != 2) return error.RefcountNotConserved;
                rc -= 1;
            }
            if (rc != 1) return error.RefcountNotConserved;
        }
    };
}

// ---------------------------------------------------------------------------
// Tests: every model holds; every recorded weakening fails.
// ---------------------------------------------------------------------------

test "1 incarnation_validated_fill_never_serves_dead_bytes — holds; both fence weakenings fail" {
    try explorer.expectHolds(FillModel(.{ .cycles = 1, .halves = 1 }), .{});
    try explorer.expectFails(FillModel(.{ .cycles = 1, .halves = 1, .weakening = .writer_fence }), .{}, error.InvariantViolated);
    try explorer.expectFails(FillModel(.{ .cycles = 1, .halves = 1, .weakening = .reader_fence }), .{}, error.InvariantViolated);
}

test "2 incarnation_two_reuse_cycles_still_exact — holds at generation precision; both fence weakenings fail" {
    try explorer.expectHolds(FillModel(.{ .cycles = 2, .halves = 1 }), .{});
    try explorer.expectFails(FillModel(.{ .cycles = 2, .halves = 1, .weakening = .writer_fence }), .{}, error.InvariantViolated);
    try explorer.expectFails(FillModel(.{ .cycles = 2, .halves = 1, .weakening = .reader_fence }), .{}, error.InvariantViolated);
}

test "3 refcount_acquire_never_resurrects_freed_block — holds; a load/store acquire fails" {
    try explorer.expectHolds(AcquireVsReleaseModel(.none), .{});
    try explorer.expectFails(AcquireVsReleaseModel(.acquire_load_store), .{}, error.InvariantViolated);
}

test "4 refcount_exactly_one_terminal_release — holds; a load/store release fails" {
    try explorer.expectHolds(TwoReleasesModel(.none), .{});
    try explorer.expectFails(TwoReleasesModel(.release_load_store), .{}, error.FinalViolated);
}

test "5 patch_clone_composed_never_mutates_a_validated_pin — holds; weakening EITHER SeqCst fence admits the stale clone" {
    try explorer.expectHolds(PatchCloneModel(.none), .{});
    try explorer.expectFails(PatchCloneModel(.patch_fence), .{}, error.InvariantViolated);
    try explorer.expectFails(PatchCloneModel(.clone_fence), .{}, error.InvariantViolated);
}

test "6 incarnation_validated_fill_never_serves_mid_patch_bytes — holds; both fence weakenings fail" {
    try explorer.expectHolds(FillModel(.{ .cycles = 1, .halves = 2 }), .{});
    try explorer.expectFails(FillModel(.{ .cycles = 1, .halves = 2, .weakening = .writer_fence }), .{}, error.InvariantViolated);
    try explorer.expectFails(FillModel(.{ .cycles = 1, .halves = 2, .weakening = .reader_fence }), .{}, error.InvariantViolated);
}

test "state counts: the models are small enough to re-run on every gate" {
    var report: explorer.Report = undefined;
    try explorer.explore(FillModel(.{ .cycles = 2, .halves = 1 }), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 10 and report.states < 2000);
    try explorer.explore(PatchCloneModel(.none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 10 and report.states < 500);
}
