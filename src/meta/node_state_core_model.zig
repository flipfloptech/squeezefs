//! Explorer models of the node lifecycle word (`node_state_core.zig`;
//! porting spec `docs/porting/kv_node.md` §4.3; the loom models
//! `node_state_*` in the reference's `loom-models/src/lib.rs`).
//!
//! Each model is a plain value type: the word is a `u8` field (three bits),
//! the per-node write lock is a plain flag with explicit acquire/release
//! steps, the open delta is a counter under that lock, and every `step` is
//! ONE atomic action — one RMW body from `rmw` applied to the word, one
//! load, or one lock-protected plain write. The strong variant of every
//! model holds over every schedule; each weakening SPLITS one RMW into a
//! load step and a store step (the "load-check-then-store" the spec names),
//! and the explorer finds the schedule that slips between them.
//!
//! Weakening ledger (which split fails which model):
//!   1 supersede_never_loses_a_racing_apply   — `markDirty` split, `supersede` split
//!   2 evict_never_wins_against_accepted_dirt — `markDirty` split, `tryEvict` as fetchOr
//!   3 freeze_swap_conserves_records          — `endFreeze` split (the RMW outside the lock)
//!   4 at_most_one_freeze_in_flight           — `beginFreeze` as two RMWs
//!   5 forced_freeze_conserves_and_reports    — `supersede` split
//! Model 3 composes the apply and the freeze under the node lock exactly as
//! `apply_locked`/`freeze_locked` do, so the lock masks a `beginFreeze`
//! split there; the one RMW model 3 runs OUTSIDE the lock is `endFreeze`,
//! and that is the split it catches (model 4 catches the `beginFreeze` one).

const std = @import("std");
const explorer = @import("../model/explorer.zig");
const core = @import("node_state_core.zig");

const D: u8 = @intCast(core.dirty_bit);
const F: u8 = @intCast(core.freezing_bit);
const S: u8 = @intCast(core.superseded_bit);

/// Program counter value of a finished thread.
const done_pc: u8 = 0xff;

/// The RMW bodies, one per op, as pure functions of the word — a model step
/// applies one of them atomically. `tie` below proves each equals the
/// shipped op over every word value.
const rmw = struct {
    /// `markDirty`: null = refused (SUPERSEDED).
    fn markDirty(w: u8) ?u8 {
        return if (w & S != 0) null else w | D;
    }
    /// `beginFreeze`: null = refused (Superseded / AlreadyFreezing / NotDirty).
    fn beginFreeze(w: u8) ?u8 {
        if (w & S != 0 or w & F != 0 or w & D == 0) return null;
        return (w & ~D) | F;
    }
    /// `beginForcedFreeze`: null = refused (Superseded / AlreadyFreezing).
    fn beginForcedFreeze(w: u8) ?u8 {
        if (w & S != 0 or w & F != 0) return null;
        return (w & ~D) | F;
    }
    /// `endFreeze`: fetchAnd(~FREEZING); the caller reads DIRTY off the previous word.
    fn endFreeze(w: u8) u8 {
        return w & ~F;
    }
    /// `supersede`: fetchOr(SUPERSEDED); the caller reads the outcome off the previous word.
    fn supersede(w: u8) u8 {
        return w | S;
    }
    /// `tryEvict`: CAS 0 → SUPERSEDED; null = lost.
    fn tryEvict(w: u8) ?u8 {
        return if (w == 0) S else null;
    }
};

// ---------------------------------------------------------------------------
// 1. supersede vs apply
// ---------------------------------------------------------------------------

/// Invariant #1: a commit's `markDirty` racing an SMO's `supersede` never
/// jointly loses a record — either the apply is refused (the revalidation
/// outcome) or the outcome reports `was_dirty` so the successor build carries
/// the delta; `applied ∧ ¬was_dirty` is the silently dropped record. The
/// terminal bit holds afterwards and a post-terminal apply is refused.
///
/// Weakenings: `.split_mark_dirty` (load-check-then-store) — schedule:
/// committer loads 0, SMO supersedes a clean word, committer stores DIRTY
/// (also clobbering SUPERSEDED); `.split_supersede` — SMO loads 0, committer
/// applies, SMO stores 0|SUPERSEDED and reports a clean pre-state.
fn SupersedeVsApply(comptime weakening: enum { none, split_mark_dirty, split_supersede }) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const committer = 0;
        const smo = 1;

        word: u8 = 0,
        pc: [2]u8 = @splat(0),
        /// The loaded word of a split RMW (per thread).
        tmp: [2]u8 = @splat(0),
        applied: bool = false,
        was_dirty: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == done_pc;
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                committer => switch (weakening) {
                    .split_mark_dirty => switch (self.pc[t]) {
                        0 => {
                            // load; the superseded check is on the stale copy.
                            self.tmp[t] = self.word;
                            self.pc[t] = if (self.tmp[t] & S != 0) done_pc else 1;
                        },
                        1 => {
                            // store (not a CAS): lands whatever happened since.
                            self.word = self.tmp[t] | D;
                            self.applied = true;
                            self.pc[t] = done_pc;
                        },
                        else => unreachable,
                    },
                    else => {
                        if (rmw.markDirty(self.word)) |n| {
                            self.word = n;
                            self.applied = true;
                        }
                        self.pc[t] = done_pc;
                    },
                },
                smo => switch (weakening) {
                    .split_supersede => switch (self.pc[t]) {
                        0 => {
                            self.tmp[t] = self.word;
                            self.pc[t] = 1;
                        },
                        1 => {
                            self.word = self.tmp[t] | S;
                            self.was_dirty = self.tmp[t] & D != 0;
                            self.pc[t] = done_pc;
                        },
                        else => unreachable,
                    },
                    else => {
                        const prev = self.word;
                        self.word = rmw.supersede(prev);
                        self.was_dirty = prev & D != 0;
                        self.pc[t] = done_pc;
                    },
                },
                else => unreachable,
            }
        }
        pub fn invariant(_: *const Self) !void {}
        pub fn final(self: *const Self) !void {
            if (self.applied and !self.was_dirty) return error.LostUpdate;
            if (self.word & S == 0) return error.TerminalLost;
            if (rmw.markDirty(self.word) != null) return error.PostTerminalApplyAccepted;
        }
    };
}

// ---------------------------------------------------------------------------
// 2. evict vs apply
// ---------------------------------------------------------------------------

/// Invariant #2 (dirty pinning): clock eviction's CAS from the exact clean
/// word never wins against a node that just accepted dirt — `applied ∧
/// evicted` is unrepresentable, and the loser of either race fails loud.
///
/// Weakenings: `.split_mark_dirty` — committer loads 0, evictor CASes
/// 0 → SUPERSEDED, committer stores DIRTY: both won; `.evict_fetch_or`
/// (`tryEvict` as fetchOr instead of a CAS from 0) — committer applies,
/// evictor ORs SUPERSEDED onto a dirty word: both won.
fn EvictVsApply(comptime weakening: enum { none, split_mark_dirty, evict_fetch_or }) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const evictor = 0;
        const committer = 1;

        word: u8 = 0,
        pc: [2]u8 = @splat(0),
        tmp: u8 = 0,
        applied: bool = false,
        evicted: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == done_pc;
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                evictor => {
                    switch (weakening) {
                        .evict_fetch_or => {
                            const prev = self.word;
                            self.word = prev | S;
                            self.evicted = prev & S == 0;
                        },
                        else => if (rmw.tryEvict(self.word)) |n| {
                            self.word = n;
                            self.evicted = true;
                        },
                    }
                    self.pc[t] = done_pc;
                },
                committer => switch (weakening) {
                    .split_mark_dirty => switch (self.pc[t]) {
                        0 => {
                            self.tmp = self.word;
                            self.pc[t] = if (self.tmp & S != 0) done_pc else 1;
                        },
                        1 => {
                            self.word = self.tmp | D;
                            self.applied = true;
                            self.pc[t] = done_pc;
                        },
                        else => unreachable,
                    },
                    else => {
                        if (rmw.markDirty(self.word)) |n| {
                            self.word = n;
                            self.applied = true;
                        }
                        self.pc[t] = done_pc;
                    },
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.applied and self.evicted) return error.BothWon;
        }
        pub fn final(self: *const Self) !void {
            if (self.applied == self.evicted) return error.NotExactlyOneWinner;
            if (self.applied) {
                if (self.word & D == 0) return error.AcceptedDirtInvisible;
                if (self.word & S != 0) return error.LoserEvictionSevered;
            } else if (self.word & S == 0) return error.WinnerEvictionDidNotSever;
        }
    };
}

// ---------------------------------------------------------------------------
// 3. freeze-swap vs apply (under the node lock), end_freeze outside it
// ---------------------------------------------------------------------------

/// Invariant #3 (freeze-swap vs concurrent apply): the freeze swaps the
/// delta out under the lock while applies keep landing under the same lock;
/// records are conserved (`frozen + open == 2`), the dirty bit exactly
/// tracks the open delta, `redirtied ⇒ open > 0`, and the freeze window is
/// closed after `endFreeze` — which runs OUTSIDE the lock, racing the apply.
///
/// Weakening `.split_end_freeze` (load-then-store instead of fetchAnd):
/// writeback loads FREEZING, committer applies (DIRTY set, open = 1),
/// writeback stores the stale word minus FREEZING — DIRTY is lost while a
/// record sits in the open delta.
fn FreezeSwap(comptime weakening: enum { none, split_end_freeze }) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const committer = 0;
        const writeback = 1;

        word: u8 = D,
        /// The per-node write lock: a flag; acquire spins (a no-op step the
        /// explorer prunes), release is a plain step.
        locked: bool = false,
        /// Open-delta records (under the lock); seeded with one so the
        /// freeze is always legal.
        open: u8 = 1,
        frozen: u8 = 0,
        pc: [2]u8 = @splat(0),
        tmp: u8 = 0,
        applied: bool = false,
        redirtied: bool = false,
        refused: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == done_pc;
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                committer => switch (self.pc[t]) {
                    0 => if (!self.locked) {
                        self.locked = true;
                        self.pc[t] = 1;
                    },
                    1 => {
                        if (rmw.markDirty(self.word)) |n| {
                            self.word = n;
                            self.applied = true;
                        }
                        self.pc[t] = 2;
                    },
                    2 => {
                        if (self.applied) self.open += 1;
                        self.pc[t] = 3;
                    },
                    3 => {
                        self.locked = false;
                        self.pc[t] = done_pc;
                    },
                    else => unreachable,
                },
                writeback => switch (self.pc[t]) {
                    0 => if (!self.locked) {
                        self.locked = true;
                        self.pc[t] = 1;
                    },
                    1 => {
                        if (rmw.beginFreeze(self.word)) |n| self.word = n else self.refused = true;
                        self.pc[t] = 2;
                    },
                    2 => {
                        self.frozen = self.open;
                        self.pc[t] = 3;
                    },
                    3 => {
                        self.open = 0;
                        self.pc[t] = 4;
                    },
                    4 => {
                        self.locked = false;
                        self.pc[t] = 5;
                    },
                    5 => switch (weakening) {
                        .none => {
                            const prev = self.word;
                            self.word = rmw.endFreeze(prev);
                            self.redirtied = prev & D != 0;
                            self.pc[t] = done_pc;
                        },
                        .split_end_freeze => {
                            self.tmp = self.word;
                            self.pc[t] = 6;
                        },
                    },
                    6 => {
                        self.word = rmw.endFreeze(self.tmp);
                        self.redirtied = self.tmp & D != 0;
                        self.pc[t] = done_pc;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.refused) return error.FreezeRefusedOnDirtyNode;
        }
        pub fn final(self: *const Self) !void {
            if (self.frozen + self.open != 2) return error.RecordsNotConserved;
            if ((self.word & D != 0) != (self.open > 0)) return error.DirtyBitDrift;
            if (self.redirtied and self.open == 0) return error.PhantomRedirty;
            if (self.word & F != 0) return error.FreezeWindowOpen;
        }
    };
}

// ---------------------------------------------------------------------------
// 4. two racing freezes
// ---------------------------------------------------------------------------

/// Invariant #4: two racing `beginFreeze` calls on a dirty node admit
/// exactly one winner; afterwards FREEZING is set and DIRTY clear.
///
/// Weakening `.two_rmws` (`fetchAnd(~DIRTY)` then `fetchOr(FREEZING)`
/// instead of one CAS): both load DIRTY and pass the checks, both clear,
/// both set — two freezes in flight.
fn TwoFreezes(comptime weakening: enum { none, two_rmws }) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        word: u8 = D,
        pc: [2]u8 = @splat(0),
        won: [2]bool = @splat(false),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == done_pc;
        }
        pub fn step(self: *Self, t: usize) void {
            switch (weakening) {
                .none => {
                    if (rmw.beginFreeze(self.word)) |n| {
                        self.word = n;
                        self.won[t] = true;
                    }
                    self.pc[t] = done_pc;
                },
                .two_rmws => switch (self.pc[t]) {
                    0 => {
                        // load + the three checks on the loaded copy.
                        self.pc[t] = if (rmw.beginFreeze(self.word) == null) done_pc else 1;
                    },
                    1 => {
                        self.word &= ~D;
                        self.pc[t] = 2;
                    },
                    2 => {
                        self.word |= F;
                        self.won[t] = true;
                        self.pc[t] = done_pc;
                    },
                    else => unreachable,
                },
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.won[0] and self.won[1]) return error.TwoFreezesInFlight;
        }
        pub fn final(self: *const Self) !void {
            if (self.won[0] == self.won[1]) return error.NotExactlyOneWinner;
            if (self.word & F == 0) return error.WinnerNotFreezing;
            if (self.word & D != 0) return error.SwapLeftDirty;
        }
    };
}

// ---------------------------------------------------------------------------
// 5. the forced-freeze SMO arm vs a racing apply
// ---------------------------------------------------------------------------

/// Invariant #5 (the forced-compaction nudge): the `compact_node_forced`
/// freeze arm (ordinary freeze if the overlay is non-empty, forced
/// otherwise) composes with lock-serialized applies like the ordinary
/// freeze — a racing apply lands in the frozen pile or, after the swap,
/// re-sets DIRTY so the SMO's `supersede` reports `was_dirty` for the
/// bounded second merge; a second forced freeze is refused; `supersede`
/// always sees `was_freezing`; records are conserved.
///
/// Weakening `.split_supersede` (load-then-store instead of fetchOr): the
/// SMO loads FREEZING, the committer applies after the swap, the SMO stores
/// the stale word plus SUPERSEDED — `was_dirty` is false for an applied
/// record and DIRTY itself is clobbered.
fn ForcedFreeze(comptime weakening: enum { none, split_supersede }) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const committer = 0;
        const smo = 1;

        word: u8 = 0,
        locked: bool = false,
        open: u8 = 0,
        frozen: u8 = 0,
        pc: [2]u8 = @splat(0),
        tmp: u8 = 0,
        applied: bool = false,
        was_dirty: bool = false,
        /// A protocol assertion the loom model makes inline failed.
        bug: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == done_pc;
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                committer => switch (self.pc[t]) {
                    0 => if (!self.locked) {
                        self.locked = true;
                        self.pc[t] = 1;
                    },
                    1 => {
                        if (rmw.markDirty(self.word)) |n| {
                            self.word = n;
                            self.applied = true;
                        }
                        self.pc[t] = 2;
                    },
                    2 => {
                        if (self.applied) self.open += 1;
                        self.pc[t] = 3;
                    },
                    3 => {
                        self.locked = false;
                        self.pc[t] = done_pc;
                    },
                    else => unreachable,
                },
                smo => switch (self.pc[t]) {
                    0 => if (!self.locked) {
                        self.locked = true;
                        self.pc[t] = 1;
                    },
                    1 => {
                        // The freeze arm: ordinary when a delta is open, forced when empty.
                        const next = if (self.open > 0) rmw.beginFreeze(self.word) else rmw.beginForcedFreeze(self.word);
                        if (next) |n| self.word = n else self.bug = true;
                        self.pc[t] = 2;
                    },
                    2 => {
                        self.frozen = self.open;
                        self.pc[t] = 3;
                    },
                    3 => {
                        self.open = 0;
                        self.pc[t] = 4;
                    },
                    4 => {
                        self.locked = false;
                        self.pc[t] = 5;
                    },
                    5 => {
                        // The nudge holds a freeze in flight (acquire load).
                        if (self.word & F == 0) self.bug = true;
                        self.pc[t] = 6;
                    },
                    6 => {
                        // forced vs forced: at most one freeze in flight.
                        if (rmw.beginForcedFreeze(self.word) != null) self.bug = true;
                        self.pc[t] = 7;
                    },
                    7 => switch (weakening) {
                        .none => {
                            const prev = self.word;
                            self.word = rmw.supersede(prev);
                            self.was_dirty = prev & D != 0;
                            if (prev & F == 0) self.bug = true;
                            self.pc[t] = 9;
                        },
                        .split_supersede => {
                            self.tmp = self.word;
                            self.pc[t] = 8;
                        },
                    },
                    8 => {
                        self.word = self.tmp | S;
                        self.was_dirty = self.tmp & D != 0;
                        if (self.tmp & F == 0) self.bug = true;
                        self.pc[t] = 9;
                    },
                    9 => {
                        self.word = rmw.endFreeze(self.word);
                        self.pc[t] = 10;
                    },
                    10 => {
                        if (self.word & F != 0) self.bug = true;
                        self.pc[t] = done_pc;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.bug) return error.ProtocolAssertionFailed;
        }
        pub fn final(self: *const Self) !void {
            if (self.frozen + self.open != @intFromBool(self.applied)) return error.RecordsNotConserved;
            if (self.applied and self.frozen == 0 and !self.was_dirty) return error.PostSwapApplyInvisibleToSmo;
        }
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

// The model's RMW bodies ARE the shipped ops: every word value, every op.
test "tie: model RMW bodies equal the shipped NodeState ops over every word" {
    var w: u8 = 0;
    while (w < 8) : (w += 1) {
        var st: core.NodeState = .{ .word = .init(w) };
        const md = rmw.markDirty(w);
        try testing.expectEqual(md != null, if (st.markDirty()) |_| true else |_| false);
        try testing.expectEqual(md orelse w, @as(u8, @intCast(st.word.load(.monotonic))));

        st = .{ .word = .init(w) };
        const bf = rmw.beginFreeze(w);
        try testing.expectEqual(bf != null, if (st.beginFreeze()) |_| true else |_| false);
        try testing.expectEqual(bf orelse w, @as(u8, @intCast(st.word.load(.monotonic))));

        st = .{ .word = .init(w) };
        const ff = rmw.beginForcedFreeze(w);
        try testing.expectEqual(ff != null, if (st.beginForcedFreeze()) |_| true else |_| false);
        try testing.expectEqual(ff orelse w, @as(u8, @intCast(st.word.load(.monotonic))));

        if (w & F != 0) {
            // The legal domain of endFreeze (outside it the op fires a tripwire).
            st = .{ .word = .init(w) };
            try testing.expectEqual(w & D != 0, st.endFreeze());
            try testing.expectEqual(rmw.endFreeze(w), @as(u8, @intCast(st.word.load(.monotonic))));
        }

        st = .{ .word = .init(w) };
        if (st.supersede()) |outcome| {
            try testing.expect(w & S == 0);
            try testing.expectEqual(w & D != 0, outcome.was_dirty);
            try testing.expectEqual(w & F != 0, outcome.was_freezing);
        } else |_| try testing.expect(w & S != 0);
        try testing.expectEqual(rmw.supersede(w), @as(u8, @intCast(st.word.load(.monotonic))));

        st = .{ .word = .init(w) };
        const ev = rmw.tryEvict(w);
        try testing.expectEqual(ev != null, st.tryEvict());
        try testing.expectEqual(ev orelse w, @as(u8, @intCast(st.word.load(.monotonic))));
    }
}

test "node_state_supersede_never_loses_a_racing_apply: holds; markDirty split and supersede split fail" {
    try explorer.expectHolds(SupersedeVsApply(.none), .{});
    try explorer.expectFails(SupersedeVsApply(.split_mark_dirty), .{}, error.FinalViolated);
    try explorer.expectFails(SupersedeVsApply(.split_supersede), .{}, error.FinalViolated);
}

test "node_state_evict_never_wins_against_accepted_dirt: holds; markDirty split and fetchOr eviction fail" {
    try explorer.expectHolds(EvictVsApply(.none), .{});
    try explorer.expectFails(EvictVsApply(.split_mark_dirty), .{}, error.InvariantViolated);
    try explorer.expectFails(EvictVsApply(.evict_fetch_or), .{}, error.InvariantViolated);
}

test "node_state_freeze_swap_conserves_records: holds; endFreeze split fails" {
    try explorer.expectHolds(FreezeSwap(.none), .{});
    try explorer.expectFails(FreezeSwap(.split_end_freeze), .{}, error.FinalViolated);
}

test "node_state_at_most_one_freeze_in_flight: holds; beginFreeze as two RMWs fails" {
    try explorer.expectHolds(TwoFreezes(.none), .{});
    try explorer.expectFails(TwoFreezes(.two_rmws), .{}, error.InvariantViolated);
}

test "node_state_forced_freeze_conserves_and_reports_freezing: holds; supersede split fails" {
    try explorer.expectHolds(ForcedFreeze(.none), .{});
    try explorer.expectFails(ForcedFreeze(.split_supersede), .{}, error.FinalViolated);
}

// The state counts the evidence note records.
test "node_state models: exhaustive within the explorer's default limits" {
    inline for (.{ SupersedeVsApply(.none), EvictVsApply(.none), FreezeSwap(.none), TwoFreezes(.none), ForcedFreeze(.none) }) |Model| {
        var report: explorer.Report = undefined;
        try explorer.explore(Model, testing.allocator, .{}, &report);
        try testing.expect(report.states > 1);
        try testing.expect(report.states < 1000);
    }
}
