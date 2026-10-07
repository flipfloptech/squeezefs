//! The coverage union — ONE law for RAM accumulation and the device overlay
//! (charter §6.2 item 3 "Coverage union = a primary contiguous run + rare
//! out-of-order runs; the write-through trigger is `union == whole block`";
//! `docs/porting/data_write_path.md` §5.2, KD-OV-2). PORTABLE, dependency-
//! free, allocation-free: a plain value type the accumulation buffer and the
//! overlay record both embed and mutate under their level-3 block lock.
//!
//! The primary run is the only state an in-order stream touches; the extras
//! are sorted, pairwise disjoint and non-abutting, and disjoint from the
//! primary. `record` is order-blind: the kernel splits an O_DIRECT write
//! into segments that arrive in any order (`FOPEN_PARALLEL_DIRECT_WRITES`
//! stays on), and the retired end-offset proxy (`write_end == block end`)
//! fired on partial coverage and missed true completion — measured −34 % at
//! t16 (`design-zero-copy-write-path.md:205`). `coverage_model.zig` proves
//! the trigger law over every arrival order and the proxy's failure.
//!
//! Capacity: the extras table is inline and bounded (`MAX_EXTRA_RUNS`); a
//! disjoint run that would overflow it is REFUSED with the state unchanged
//! so the caller takes its never-lossy exit (the accumulation buffer
//! completes its complement — zeros on a fresh block, the seed image on an
//! overwrite — and writes through; the overlay declines the piece before
//! any claim). Coverage is never lost and never over-claimed.

const std = @import("std");
const assert = std.debug.assert;

/// The most out-of-order runs one block carries before the table is full.
/// 64 = `FOLD_MAX_EXTENTS` at the shipped block: the W2 extent overlay
/// folds at that many disjoint extents (its slabs mirror these runs
/// exactly), and an overlay block holds at most `block / PATCH_MAX = 8`
/// pieces above the length floor, so a full table is reachable only by a
/// random sub-page storm into one accumulation buffer — the exit the
/// refusal exists for. 64 × 8 B = 512 B inline per block entry.
pub const MAX_EXTRA_RUNS: usize = 64;

/// `[start, end)` in bytes within the block.
pub const Run = struct {
    start: u32,
    end: u32,

    pub fn len(self: Run) u32 {
        return self.end - self.start;
    }

    /// Overlapping or abutting (one coalesced run).
    fn touches(a: Run, b: Run) bool {
        return a.start <= b.end and b.start <= a.end;
    }

    fn merge(a: Run, b: Run) Run {
        return .{ .start = @min(a.start, b.start), .end = @max(a.end, b.end) };
    }
};

pub const RecordVerdict = struct {
    /// The union became the whole block on THIS record (fires exactly once).
    completed: bool = false,
    /// The run landed as an extra (did not touch the primary).
    out_of_order: bool = false,
    /// The completion coalesced ≥ 1 extra into the primary (an out-of-order
    /// arrival sequence completed the block).
    completed_with_extras: bool = false,
};

pub const RecordError = error{
    /// A disjoint run with no extra slot left; nothing changed.
    RunTableFull,
};

pub const CoverageUnion = struct {
    primary: Run = .{ .start = 0, .end = 0 },
    extras: [MAX_EXTRA_RUNS]Run = undefined,
    n_extras: u8 = 0,

    pub const empty: CoverageUnion = .{};

    pub fn isEmpty(self: *const CoverageUnion) bool {
        return self.primary.len() == 0 and self.n_extras == 0;
    }

    /// `primary == [0, len)` — the write-through trigger.
    pub fn isFull(self: *const CoverageUnion, len: u32) bool {
        return self.primary.start == 0 and self.primary.end == len;
    }

    pub fn runCount(self: *const CoverageUnion) usize {
        return @as(usize, @intFromBool(self.primary.len() != 0)) + self.n_extras;
    }

    /// Record `[start, end)`. Empty range ⇒ nothing. Already full ⇒ no
    /// transition (a re-write never double-fires). First touch ⇒ primary;
    /// touching the primary ⇒ extend it and coalesce every extra it now
    /// touches; else ⇒ an extra (absorbing the extras it touches), sorted.
    pub fn record(self: *CoverageUnion, start: u32, end: u32, len: u32) RecordError!RecordVerdict {
        assert(start <= end and end <= len);
        if (start == end) return .{};
        if (self.isFull(len)) return .{};
        const run: Run = .{ .start = start, .end = end };
        if (self.primary.len() == 0) {
            self.primary = run;
            return .{ .completed = self.isFull(len) };
        }
        if (self.primary.touches(run)) {
            self.primary = self.primary.merge(run);
            const absorbed = self.coalesceExtrasIntoPrimary();
            const full = self.isFull(len);
            return .{ .completed = full, .completed_with_extras = full and absorbed != 0 };
        }
        try self.insertExtra(run);
        return .{ .out_of_order = true };
    }

    /// One retain pass suffices: extras are pairwise non-abutting, so after
    /// the primary absorbs one neighbour it cannot newly touch another
    /// unless that one is adjacent in the sorted order — which the pass
    /// visits in turn.
    fn coalesceExtrasIntoPrimary(self: *CoverageUnion) usize {
        var kept: u8 = 0;
        var absorbed: usize = 0;
        var i: usize = 0;
        while (i < self.n_extras) : (i += 1) {
            const e = self.extras[i];
            if (self.primary.touches(e)) {
                self.primary = self.primary.merge(e);
                absorbed += 1;
            } else {
                self.extras[kept] = e;
                kept += 1;
            }
        }
        self.n_extras = kept;
        return absorbed;
    }

    fn insertExtra(self: *CoverageUnion, run_in: Run) RecordError!void {
        var run = run_in;
        // Absorb the extras the new run touches (they are sorted and
        // disjoint, so the touched ones form one contiguous span).
        var kept: u8 = 0;
        var i: usize = 0;
        var tmp: [MAX_EXTRA_RUNS]Run = undefined;
        while (i < self.n_extras) : (i += 1) {
            const e = self.extras[i];
            if (run.touches(e)) {
                run = run.merge(e);
            } else {
                tmp[kept] = e;
                kept += 1;
            }
        }
        if (kept >= MAX_EXTRA_RUNS) return error.RunTableFull;
        // Sorted insert.
        var pos: usize = 0;
        while (pos < kept and tmp[pos].start < run.start) : (pos += 1) {}
        var j: usize = kept;
        while (j > pos) : (j -= 1) tmp[j] = tmp[j - 1];
        tmp[pos] = run;
        @memcpy(self.extras[0 .. kept + 1], tmp[0 .. kept + 1]);
        self.n_extras = kept + 1;
    }

    /// Would recording `pending` more disjoint runs certainly fit? The
    /// overlay's `begin_store` asks this BEFORE claiming (its union is
    /// mutated only at the CQE, so an in-flight claim is a run-to-be).
    pub fn hasHeadroom(self: *const CoverageUnion, pending: usize) bool {
        return @as(usize, self.n_extras) + pending <= MAX_EXTRA_RUNS;
    }

    /// Consume any pending transition: the union IS the whole block now
    /// (an exit that zero-completed or seeded the complement).
    pub fn setFull(self: *CoverageUnion, len: u32) void {
        self.primary = .{ .start = 0, .end = len };
        self.n_extras = 0;
    }

    pub fn coveredBytes(self: *const CoverageUnion) u64 {
        var n: u64 = self.primary.len();
        for (self.extras[0..self.n_extras]) |e| n += e.len();
        return n;
    }

    /// Is `[start, end)` entirely covered by ONE run?
    pub fn contains(self: *const CoverageUnion, start: u32, end: u32) bool {
        if (start >= end) return true;
        if (self.primary.start <= start and end <= self.primary.end) return true;
        for (self.extras[0..self.n_extras]) |e| if (e.start <= start and end <= e.end) return true;
        return false;
    }

    pub const max_runs = MAX_EXTRA_RUNS + 1;
    pub const max_gaps = MAX_EXTRA_RUNS + 2;

    /// Every run ascending (the primary merged into order).
    pub fn runsSorted(self: *const CoverageUnion, out: *[max_runs]Run) []Run {
        var n: usize = 0;
        var placed_primary = self.primary.len() == 0;
        for (self.extras[0..self.n_extras]) |e| {
            if (!placed_primary and self.primary.start < e.start) {
                out[n] = self.primary;
                n += 1;
                placed_primary = true;
            }
            out[n] = e;
            n += 1;
        }
        if (!placed_primary) {
            out[n] = self.primary;
            n += 1;
        }
        return out[0..n];
    }

    /// The uncovered spans of `[0, len)` ascending.
    pub fn gaps(self: *const CoverageUnion, len: u32, out: *[max_gaps]Run) []Run {
        var runs_buf: [max_runs]Run = undefined;
        const runs = self.runsSorted(&runs_buf);
        var n: usize = 0;
        var cursor: u32 = 0;
        for (runs) |r| {
            if (r.start > cursor) {
                out[n] = .{ .start = cursor, .end = r.start };
                n += 1;
            }
            cursor = @max(cursor, r.end);
        }
        if (cursor < len) {
            out[n] = .{ .start = cursor, .end = len };
            n += 1;
        }
        return out[0..n];
    }

    /// The covered sub-runs of `[start, end)` ascending (a reader zero-fills
    /// everything between them).
    pub fn coveredRunsIn(self: *const CoverageUnion, start: u32, end: u32, out: *[max_runs]Run) []Run {
        var runs_buf: [max_runs]Run = undefined;
        const runs = self.runsSorted(&runs_buf);
        var n: usize = 0;
        for (runs) |r| {
            const s = @max(r.start, start);
            const e = @min(r.end, end);
            if (s < e) {
                out[n] = .{ .start = s, .end = e };
                n += 1;
            }
        }
        return out[0..n];
    }
};

// ---------------------------------------------------------------------------
// Tests — one per verdict, the laws of §5.2.
// ---------------------------------------------------------------------------

const testing = std.testing;
const BS: u32 = 4096 * 8;

test "coverage_union_is_overlap_safe_and_order_blind: in-order, end-first and shuffled arrivals complete exactly once" {
    // In order.
    var a: CoverageUnion = .empty;
    try testing.expect(!(try a.record(0, 4096, BS)).completed);
    try testing.expect(!(try a.record(4096, 16384, BS)).completed);
    const v = try a.record(16384, BS, BS);
    try testing.expect(v.completed and !v.completed_with_extras);
    try testing.expect(a.isFull(BS));
    try testing.expectEqual(@as(usize, 1), a.runCount());
    // A re-write of a full block never fires again.
    try testing.expectEqual(RecordVerdict{}, try a.record(0, 4096, BS));
    // End first: the tail parks as the primary, the head completes.
    var b: CoverageUnion = .empty;
    try testing.expect(!(try b.record(16384, BS, BS)).completed);
    try testing.expect(!b.isFull(BS));
    const w = try b.record(0, 16384, BS);
    try testing.expect(w.completed and !w.out_of_order);
    // Shuffled thirds: the middle is out of order, the last completes WITH extras.
    var c: CoverageUnion = .empty;
    _ = try c.record(0, 8192, BS);
    const ooo = try c.record(20480, BS, BS);
    try testing.expect(ooo.out_of_order and !ooo.completed);
    try testing.expectEqual(@as(usize, 2), c.runCount());
    const fin = try c.record(8192, 20480, BS);
    try testing.expect(fin.completed and fin.completed_with_extras);
    try testing.expectEqual(@as(u8, 0), c.n_extras);
}

test "_tracks_out_of_order_runs_and_gaps: extras sorted, disjoint, non-abutting; gaps and covered runs exact; overlapping rewrites inside the union stay exact" {
    var u: CoverageUnion = .empty;
    _ = try u.record(8192, 12288, BS); // primary
    _ = try u.record(24576, 28672, BS); // extra
    _ = try u.record(0, 4096, BS); // extra before the primary
    try testing.expectEqual(@as(u8, 2), u.n_extras);
    try testing.expectEqual(@as(u32, 0), u.extras[0].start);
    try testing.expectEqual(@as(u32, 24576), u.extras[1].start);
    var runs: [CoverageUnion.max_runs]Run = undefined;
    const rs = u.runsSorted(&runs);
    try testing.expectEqual(@as(usize, 3), rs.len);
    try testing.expectEqual(Run{ .start = 8192, .end = 12288 }, rs[1]);
    var gaps_buf: [CoverageUnion.max_gaps]Run = undefined;
    const gs = u.gaps(BS, &gaps_buf);
    try testing.expectEqual(@as(usize, 3), gs.len);
    try testing.expectEqual(Run{ .start = 4096, .end = 8192 }, gs[0]);
    try testing.expectEqual(Run{ .start = 12288, .end = 24576 }, gs[1]);
    try testing.expectEqual(Run{ .start = 28672, .end = BS }, gs[2]);
    try testing.expectEqual(@as(u64, 12288), u.coveredBytes());
    try testing.expect(u.contains(8192, 10000));
    try testing.expect(!u.contains(4000, 5000));
    var cov: [CoverageUnion.max_runs]Run = undefined;
    const cr = u.coveredRunsIn(2048, 26624, &cov);
    try testing.expectEqual(@as(usize, 3), cr.len);
    try testing.expectEqual(Run{ .start = 2048, .end = 4096 }, cr[0]);
    try testing.expectEqual(Run{ .start = 24576, .end = 26624 }, cr[2]);
    // An abutting extra coalesces with its neighbour (never two abutting extras).
    _ = try u.record(4096, 8192, BS); // touches the primary AND extras[0]
    try testing.expectEqual(@as(u8, 1), u.n_extras);
    try testing.expectEqual(Run{ .start = 0, .end = 12288 }, u.primary);
    // A rewrite spanning two extras merges them into one.
    _ = try u.record(16384, 20480, BS);
    _ = try u.record(18432, 26624, BS);
    try testing.expectEqual(@as(u8, 1), u.n_extras);
    try testing.expectEqual(Run{ .start = 16384, .end = 28672 }, u.extras[0]);
}

test "_set_full_claims_everything: setFull consumes every pending transition; the empty range is nothing" {
    var u: CoverageUnion = .empty;
    try testing.expect(u.isEmpty());
    try testing.expectEqual(RecordVerdict{}, try u.record(100, 100, BS));
    try testing.expect(u.isEmpty());
    _ = try u.record(0, 100, BS);
    _ = try u.record(5000, 6000, BS);
    u.setFull(BS);
    try testing.expect(u.isFull(BS));
    try testing.expectEqual(@as(u8, 0), u.n_extras);
    var gaps_buf: [CoverageUnion.max_gaps]Run = undefined;
    try testing.expectEqual(@as(usize, 0), u.gaps(BS, &gaps_buf).len);
    try testing.expectEqual(RecordVerdict{}, try u.record(0, BS, BS));
}

test "the extras table is bounded: the 65th disjoint run is refused with the state unchanged; headroom accounts for pending claims; a touching run still lands" {
    var u: CoverageUnion = .empty;
    const big: u32 = 4096 * 256;
    _ = try u.record(0, 4096, big);
    var i: u32 = 0;
    while (i < MAX_EXTRA_RUNS) : (i += 1) _ = try u.record((2 * i + 2) * 4096, (2 * i + 3) * 4096, big);
    try testing.expectEqual(@as(u8, MAX_EXTRA_RUNS), u.n_extras);
    try testing.expect(!u.hasHeadroom(1));
    const before = u;
    try testing.expectError(error.RunTableFull, u.record((2 * MAX_EXTRA_RUNS + 10) * 4096, (2 * MAX_EXTRA_RUNS + 11) * 4096, big));
    try testing.expectEqual(before.n_extras, u.n_extras);
    try testing.expectEqual(before.primary, u.primary);
    // A run touching an existing extra lands without a new slot.
    _ = try u.record(3 * 4096, 4 * 4096, big);
    try testing.expectEqual(@as(u8, MAX_EXTRA_RUNS - 1), u.n_extras); // it bridged extras[0] and extras[1] into one
    try testing.expect(u.hasHeadroom(1));
    try testing.expect(!u.hasHeadroom(2));
}
