//! Extent grants (charter §5.5 "Extent grants"; `docs/porting/
//! sym_appenders_manager.md` §5): `grant = clamp(2 × ewma_smo_rate ×
//! failover_bound_s, 8, free_heap / (4 × appenders))`, refilled at 50 %;
//! the grant is a POOL (retired images recycle; surplus returns at a
//! quiet cadence); an appender out of grant with the manager unreachable
//! refuses `GrantExhausted` (EAGAIN-class), never ENOSPC. Closure:
//! `extent_grant_extents ≡ claimed + returned + unclaimed`, asserted on
//! every mutation of the RAM pool. PORTABLE: the derivations with their
//! ties, the RAM `RegionGrant`, the return-law screens (`validateReturnRuns`,
//! `coalesceRuns`, `intersectWithRecord`), the page's `largest ≤ 4 runs`
//! projection, and the claim/return MODEL.

const std = @import("std");
const explorer = @import("../../model/explorer.zig");
const lease_clock = @import("../guard/lease_clock.zig");
const cr = @import("control_records.zig");
const page = @import("appender_page.zig");
const region = @import("region.zig");
const derive = @import("../../core/derive.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
pub const GrantRun = cr.GrantRun;

// ---------------------------------------------------------------------------
// Constants with WHY.
// ---------------------------------------------------------------------------

/// One image per pending root swap per cycle (a compaction or split of a
/// slot tree claims ≤ 2 extents) × up to 4 pending swaps per cycle — the
/// mixed tree's SMO budget (physical).
pub const GRANT_EXTENTS_FLOOR: u64 = 8;
/// A split's ≤ 3 parts + a root leaf's new root: the INTERNAL reactive
/// refill asks exactly ONE SMO's need from the reserve.
pub const SMO_IMAGES_MAX: u64 = 4;
/// More extents than slot trees is a units mistake (`W = 65,536`).
pub const GRANT_EXTENTS_MAX: u64 = 65_536;
/// The cap's divisor: a quarter of the free heap spread over the LIVE
/// appenders (in-process regions AND wire joiners).
pub const GRANT_HEAP_SHARE_DIVISOR: u64 = 4;
/// `2 ×` = one EWMA window of under-estimate.
pub const GRANT_RATE_HEADROOM: u64 = 2;
/// The pool refills when the unclaimed count halves.
pub const REFILL_FRACTION_DIVISOR: u64 = 2;

// ---------------------------------------------------------------------------
// Derivations (§5.1).
// ---------------------------------------------------------------------------

/// `CLIENT_STALE_TTL × 1000 + ladder_ms + replay_ms` — the two measured
/// terms land at the join (≈ 46.5 s shipped: 45 s + ~1.5 s).
pub fn failoverBoundMs(ladder_ms: u64, replay_ms: u64) u64 {
    return lease_clock.T_OWNER_MS + ladder_ms + replay_ms;
}

/// `clamp(2 × ewma_smo × bound_s, FLOOR, free_heap / (4 × appenders))`
/// (milli-SMO/s × ms / 1,000,000). `appenders` is `appenders_known`, never
/// below 1.
pub fn grantDerived(ewma_smo_milli_per_s: u64, failover_bound_ms: u64, free_heap_extents: u64, appenders_known: u64) u64 {
    const want = (GRANT_RATE_HEADROOM *| ewma_smo_milli_per_s) *| failover_bound_ms / 1_000_000;
    const cap = free_heap_extents / (GRANT_HEAP_SHARE_DIVISOR * @max(appenders_known, 1));
    return @min(derive.clamp(want, GRANT_EXTENTS_FLOOR, cap), GRANT_EXTENTS_MAX);
}

/// A WIRE joiner's rate is the joiner's to measure — the manager screens
/// its `want` against the heap-share cap alone: `max(free_heap / (4 ×
/// appenders_known), 8)`.
pub fn grantWireCap(free_heap_extents: u64, appenders_known: u64) u64 {
    return @max(free_heap_extents / (GRANT_HEAP_SHARE_DIVISOR * @max(appenders_known, 1)), GRANT_EXTENTS_FLOOR);
}

/// `want == 0` = the derived size (`cap` for a wire ask); an explicit want
/// past the cap is a wire integer claiming allocation authority: REJECTED.
pub fn clampGrantWant(want: u64, cap: u64) error{Rejected}!u64 {
    if (want == 0) return cap;
    if (want > cap or want > GRANT_EXTENTS_MAX) return error.Rejected;
    return want;
}

/// `SQUEEZEFS_SYM_GRANT_EXTENTS`: explicit wins verbatim (the registry
/// range `[8, 65536]` already refused the rest).
pub fn resolveGrantExtents(explicit: ?u64, derived: u64) u64 {
    return explicit orelse derived;
}

/// The join's pool floor `8 + M` — the rotor's `M` first images + the
/// SMO floor (the join's known cost class).
pub fn joinedPoolFloor(mint_slots: u64) u64 {
    return GRANT_EXTENTS_FLOOR + mint_slots;
}

/// ONE target — the recycle's keep, the shrink's mark and the ask's size:
/// `clamp(max(derived + promised, pool_floor), promised, cap)`.
pub fn joinedPoolTarget(derived: u64, promised: u64, pool_floor: u64, cap: u64) u64 {
    return derive.clamp(@max(derived +| promised, pool_floor), promised, cap);
}

/// A WIRE joiner's derived size — the charter's `2 × ewma_smo_rate ×
/// failover_bound_s` without the heap-share cap, which is the MANAGER's to
/// apply (it answers an ask past it `Rejected`; the joiner then asks `0` =
/// the cap). Clamped to `[FLOOR, GRANT_EXTENTS_MAX]`.
pub fn joinedGrantDerived(ewma_smo_milli_per_s: u64, failover_bound_ms: u64) u64 {
    const want = (GRANT_RATE_HEADROOM *| ewma_smo_milli_per_s) *| failover_bound_ms / 1_000_000;
    return derive.clamp(want, GRANT_EXTENTS_FLOOR, GRANT_EXTENTS_MAX);
}

/// The SMO rate's per-cycle fold (milli-SMO/s): `images` claimed over
/// `elapsed_ns` → `region.ewmaFold` (`7/8 · ewma + 1/8 · rate`, the
/// spec's §5.1 fold); a zero-length cycle folds nothing.
pub fn smoRateFold(ewma_milli_per_s: u64, images: u64, elapsed_ns: u64) u64 {
    if (elapsed_ns == 0) return ewma_milli_per_s;
    return region.ewmaFold(ewma_milli_per_s, (images *| 1000 *| std.time.ns_per_s) / elapsed_ns);
}

/// The pool's refill mark as the mirror's free count: `joinedRefillDue`
/// turns true once `unclaimed − promised < target / 2`, i.e. at
/// `unclaimed ≤ target / 2 + promised − 1`; the mark is the first count
/// that is due (0 when nothing can be).
pub fn joinedRefillMark(target: u64, promised: u64) u64 {
    return (target / REFILL_FRACTION_DIVISOR +| promised) -| 1;
}

/// In-process: `unclaimed × 2 ≤ refill_reference` (the unclaimed count at
/// the last refill decision).
pub fn refillDue(unclaimed: u64, refill_reference: u64) bool {
    return unclaimed * REFILL_FRACTION_DIVISOR <= refill_reference;
}

/// Joined: `headroom < derived / 2`, `headroom = unclaimed − promised`.
pub fn joinedRefillDue(headroom: u64, derived: u64) bool {
    return headroom < derived / REFILL_FRACTION_DIVISOR;
}

// ---------------------------------------------------------------------------
// The return law's screens (§4.5 `ReturnExtents`).
// ---------------------------------------------------------------------------

/// Every `start + len` must not overflow and must lie inside
/// `total_extents`; the FIRST offender is the rejection.
pub fn validateReturnRuns(runs: []const GrantRun, total_extents: u64) error{Rejected}!void {
    for (runs) |r| {
        if (r.len == 0) return error.Rejected;
        const e = std.math.add(u64, r.start, r.len) catch return error.Rejected;
        if (e > total_extents) return error.Rejected;
    }
}

fn runStartAsc(_: void, a: GrantRun, b: GrantRun) bool {
    return a.start < b.start;
}

/// Sort + merge overlapping/adjacent runs in place — the output is bounded
/// by the FRAME's length (the pre-fix path emitted one extent per frame-run
/// × record-extent and built an 8 GiB list on the manager).
pub fn coalesceRuns(runs: []GrantRun) []GrantRun {
    if (runs.len == 0) return runs;
    std.mem.sort(GrantRun, runs, {}, runStartAsc);
    var n: usize = 0;
    for (runs) |r| {
        if (n > 0 and r.start <= runs[n - 1].end()) {
            const new_end = @max(runs[n - 1].end(), r.end());
            runs[n - 1].len = @intCast(new_end - runs[n - 1].start);
        } else {
            runs[n] = r;
            n += 1;
        }
    }
    return runs[0..n];
}

/// Whether the ascending `sorted` holds `e`.
pub fn sortedContains(sorted: []const u64, e: u64) bool {
    return std.sort.binarySearch(u64, sorted, e, struct {
        fn order(ctx: u64, item: u64) std.math.Order {
            return std.math.order(ctx, item);
        }
    }.order) != null;
}

/// Whether the normalized `runs` (ascending, disjoint) hold `e` — one
/// binary search.
pub fn runsContain(runs: []const GrantRun, e: u64) bool {
    var lo: usize = 0;
    var hi: usize = runs.len;
    while (lo < hi) {
        const mid = lo + (hi - lo) / 2;
        if (e < runs[mid].start) {
            hi = mid;
        } else if (e >= runs[mid].end()) {
            lo = mid + 1;
        } else return true;
    }
    return false;
}

/// `grant − kept` (`kept` ascending), as normalized runs appended to `out`.
pub fn subtractExtents(gpa: Allocator, grant: []const GrantRun, kept: []const u64, out: *std.ArrayList(GrantRun)) Allocator.Error!void {
    for (grant) |run| {
        var start: ?u64 = null;
        var e = run.start;
        while (e <= run.end()) : (e += 1) {
            const hit = e == run.end() or sortedContains(kept, e);
            if (hit) {
                if (start) |s| {
                    try out.append(gpa, .{ .start = s, .len = @intCast(e - s) });
                    start = null;
                }
            } else if (start == null) start = e;
        }
    }
}

/// A slot lease's custody move of its tree's live images (charter §5.5
/// "Extent grants" — every extent has exactly one custodian; decision 0003
/// §AD.1): the record of the lessee the lease moves to GAINS the tree's
/// images in the grant's own entry (T1), the record of the lessee it moves
/// from LOSES them in the release's (T5). `moved` = the extents that changed
/// hands (ascending; for a gain those the record did not yet hold, for a
/// loss those it did); `runs` = the record after the move, normalized.
/// `images` ascending and deduplicated.
pub const RecordMove = struct {
    runs: std.ArrayList(GrantRun) = .empty,
    moved: std.ArrayList(u64) = .empty,

    pub fn deinit(self: *RecordMove, gpa: Allocator) void {
        self.runs.deinit(gpa);
        self.moved.deinit(gpa);
    }

    pub fn gain(gpa: Allocator, record: []const GrantRun, images: []const u64) Allocator.Error!RecordMove {
        var m: RecordMove = .{};
        errdefer m.deinit(gpa);
        for (images) |e| if (!runsContain(record, e)) try m.moved.append(gpa, e);
        try m.runs.ensureTotalCapacity(gpa, record.len + m.moved.items.len);
        m.runs.appendSliceAssumeCapacity(record);
        for (m.moved.items) |e| m.runs.appendAssumeCapacity(.{ .start = e, .len = 1 });
        m.runs.items.len = coalesceRuns(m.runs.items).len;
        return m;
    }

    pub fn lose(gpa: Allocator, record: []const GrantRun, images: []const u64) Allocator.Error!RecordMove {
        var m: RecordMove = .{};
        errdefer m.deinit(gpa);
        for (images) |e| if (runsContain(record, e)) try m.moved.append(gpa, e);
        try subtractExtents(gpa, record, m.moved.items, &m.runs);
        return m;
    }
};

/// The merge walk: `coalesced ∩ record`, output bounded by the RECORD's
/// run count (appended to `out`). Both inputs normalized.
pub fn intersectWithRecord(gpa: Allocator, coalesced: []const GrantRun, record: []const GrantRun, out: *std.ArrayList(GrantRun)) Allocator.Error!void {
    var i: usize = 0;
    var j: usize = 0;
    while (i < coalesced.len and j < record.len) {
        const a = coalesced[i];
        const b = record[j];
        const lo = @max(a.start, b.start);
        const hi = @min(a.end(), b.end());
        if (lo < hi) try out.append(gpa, .{ .start = lo, .len = @intCast(hi - lo) });
        if (a.end() < b.end()) i += 1 else j += 1;
    }
}

/// The page names the unclaimed remainder's LARGEST ≤ 4 runs (the rest
/// stays in the RAM pool unnamed, recovered as CLAIMED on a crash and
/// restored by the own-residue census).
pub fn pageRunsOf(runs: []const GrantRun) page.GrantTable {
    var sorted: [page.GRANT_RUNS_MAX]GrantRun = undefined;
    var n: usize = 0;
    for (runs) |r| {
        // Insert into the top-4 by length (ties: lower start).
        var pos = n;
        while (pos > 0 and (sorted[pos - 1].len < r.len or (sorted[pos - 1].len == r.len and sorted[pos - 1].start > r.start))) : (pos -= 1) {}
        if (pos >= page.GRANT_RUNS_MAX) continue;
        var k: usize = @min(n, page.GRANT_RUNS_MAX - 1);
        while (k > pos) : (k -= 1) sorted[k] = sorted[k - 1];
        sorted[pos] = r;
        if (n < page.GRANT_RUNS_MAX) n += 1;
    }
    var t: page.GrantTable = .{};
    // Page order: ascending start.
    std.mem.sort(GrantRun, sorted[0..n], {}, runStartAsc);
    for (sorted[0..n]) |r| t.push(.{ .start = r.start, .len = r.len }) catch unreachable;
    return t;
}

// ---------------------------------------------------------------------------
// The RAM grant: the pool.
// ---------------------------------------------------------------------------

pub const Pending = struct { extent: u64, gate_seq: u64 };

/// One region's grant in RAM. Closure on every mutation: `granted ≡
/// claimed + pending + returnable + unclaimed + returned` (the charter's
/// `claimed + returned + unclaimed` with the two in-flight sets spelled
/// out). `unclaimed` is kept SORTED so `claim` takes the lowest extent of
/// the smallest run (a recycled single is consumed before it fragments
/// the page word).
pub const RegionGrant = struct {
    gpa: Allocator,
    unclaimed: std.ArrayList(u64) = .empty,
    claimed: std.AutoHashMapUnmanaged(u64, void) = .empty,
    /// Frees parked on THIS region's tail.
    pending: std.ArrayList(Pending) = .empty,
    /// Frees the tail covered: recyclable or returnable.
    returnable: std.ArrayList(u64) = .empty,
    granted: u64 = 0,
    returned: u64 = 0,
    /// The unclaimed count at the last refill decision.
    refill_reference: u64 = 0,
    /// A leased leaf's SMO draws the GRANT: its §4.7 admission promise
    /// rides here (0 at quiesce).
    promised: u64 = 0,

    pub fn init(gpa: Allocator) RegionGrant {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *RegionGrant) void {
        self.unclaimed.deinit(self.gpa);
        self.claimed.deinit(self.gpa);
        self.pending.deinit(self.gpa);
        self.returnable.deinit(self.gpa);
    }

    pub fn held(self: *const RegionGrant) u64 {
        return self.claimed.count() + self.pending.items.len + self.returnable.items.len;
    }

    pub fn unclaimedCount(self: *const RegionGrant) u64 {
        return self.unclaimed.items.len;
    }

    /// `granted ≡ held + unclaimed + returned`.
    pub fn closureHolds(self: *const RegionGrant) bool {
        return self.granted == self.held() + self.unclaimedCount() + self.returned;
    }

    fn checkClosure(self: *const RegionGrant) void {
        assert(self.closureHolds());
    }

    /// A grant reply's runs enter the unclaimed set (sorted, deduped
    /// against what the pool already holds — a verbatim replay adds
    /// nothing). Answers the extents actually added.
    pub fn absorb(self: *RegionGrant, runs: []const GrantRun) Allocator.Error!u64 {
        var added: u64 = 0;
        for (runs) |r| {
            var e = r.start;
            while (e < r.end()) : (e += 1) {
                if (self.holdsExtent(e)) continue;
                try self.insertUnclaimed(e);
                added += 1;
            }
        }
        self.granted += added;
        if (added != 0) self.refill_reference = self.unclaimedCount();
        self.checkClosure();
        return added;
    }

    /// A slot lease's custody move (0003 §AD.1) as the RAM grant sees it:
    /// IN — the granted tree's live images become CLAIMED (granted by the
    /// transfer); OUT — the released tree's images leave the claimed set
    /// (the closure reads them as returned). Answers the extents that
    /// moved here.
    pub fn transferIn(self: *RegionGrant, images: []const u64) Allocator.Error!u64 {
        try self.claimed.ensureUnusedCapacity(self.gpa, @intCast(images.len));
        var n: u64 = 0;
        for (images) |e| {
            if (self.holdsExtent(e)) continue;
            self.claimed.putAssumeCapacity(e, {});
            n += 1;
        }
        self.granted += n;
        self.checkClosure();
        return n;
    }

    pub fn transferOut(self: *RegionGrant, images: []const u64) u64 {
        var n: u64 = 0;
        for (images) |e| {
            if (self.claimed.remove(e)) n += 1;
        }
        self.returned += n;
        self.checkClosure();
        return n;
    }

    fn holdsExtent(self: *const RegionGrant, e: u64) bool {
        if (self.claimed.contains(e)) return true;
        if (std.sort.binarySearch(u64, self.unclaimed.items, e, orderU64) != null) return true;
        for (self.pending.items) |p| if (p.extent == e) return true;
        for (self.returnable.items) |r| if (r == e) return true;
        return false;
    }

    fn orderU64(ctx: u64, item: u64) std.math.Order {
        return std.math.order(ctx, item);
    }

    fn insertUnclaimed(self: *RegionGrant, e: u64) Allocator.Error!void {
        const idx = std.sort.lowerBound(u64, self.unclaimed.items, e, orderU64);
        try self.unclaimed.insert(self.gpa, idx, e);
    }

    /// The unclaimed set as normalized runs (appended to `out`).
    pub fn unclaimedRuns(self: *const RegionGrant, gpa: Allocator, out: *std.ArrayList(GrantRun)) Allocator.Error!void {
        for (self.unclaimed.items) |e| {
            if (out.items.len > 0 and out.items[out.items.len - 1].end() == e) {
                out.items[out.items.len - 1].len += 1;
            } else try out.append(gpa, .{ .start = e, .len = 1 });
        }
    }

    /// The lowest extent of the SMALLEST unclaimed run, or `GrantExhausted`.
    pub fn claim(self: *RegionGrant) error{ GrantExhausted, OutOfMemory }!u64 {
        const items = self.unclaimed.items;
        if (items.len == 0) return error.GrantExhausted;
        // Find the smallest run; ties → the lowest.
        var best_start: usize = 0;
        var best_len: usize = std.math.maxInt(usize);
        var i: usize = 0;
        while (i < items.len) {
            var j = i + 1;
            while (j < items.len and items[j] == items[j - 1] + 1) : (j += 1) {}
            if (j - i < best_len) {
                best_len = j - i;
                best_start = i;
            }
            i = j;
        }
        const e = self.unclaimed.orderedRemove(best_start);
        try self.claimed.put(self.gpa, e, {});
        self.checkClosure();
        return e;
    }

    /// A claimed extent whose image is retired: its free parks on this
    /// region's tail at `gate_seq` (a POSITION).
    pub fn freePending(self: *RegionGrant, extent: u64, gate_seq: u64) error{ NotClaimed, OutOfMemory }!void {
        if (!self.claimed.remove(extent)) return error.NotClaimed;
        try self.pending.append(self.gpa, .{ .extent = extent, .gate_seq = gate_seq });
        self.checkClosure();
    }

    /// The durable tail moved: every gate ≤ `tail` is returnable.
    pub fn advanceDurable(self: *RegionGrant, tail: u64) Allocator.Error!u64 {
        var released: u64 = 0;
        var i: usize = 0;
        while (i < self.pending.items.len) {
            if (self.pending.items[i].gate_seq <= tail) {
                const p = self.pending.swapRemove(i);
                try self.returnable.append(self.gpa, p.extent);
                released += 1;
            } else i += 1;
        }
        self.checkClosure();
        return released;
    }

    /// The POOL: a drained returnable batch re-enters the OWN unclaimed set,
    /// lowest first, while the pool is below `keep`; what does not fit is
    /// the SURPLUS the cadence returns (left in `returnable`). Answers the
    /// recycled count.
    pub fn recycle(self: *RegionGrant, keep: u64) Allocator.Error!u64 {
        std.mem.sort(u64, self.returnable.items, {}, std.sort.asc(u64));
        var recycled: u64 = 0;
        while (self.returnable.items.len > 0 and self.unclaimedCount() < keep) {
            const e = self.returnable.orderedRemove(0);
            try self.insertUnclaimed(e);
            recycled += 1;
        }
        self.checkClosure();
        return recycled;
    }

    /// The quiet cadence's shrink: unclaimed above `target` leaves the
    /// pool, smallest runs first, into `out` (the return's runs); the page
    /// is rewritten WITHOUT them BEFORE the return lands (the caller's
    /// order). The extents stay accounted as RETURNED only once `returnLanded`.
    pub fn shrinkTo(self: *RegionGrant, target: u64, out: *std.ArrayList(u64)) Allocator.Error!void {
        while (self.unclaimedCount() > target) {
            const items = self.unclaimed.items;
            var best_start: usize = 0;
            var best_len: usize = std.math.maxInt(usize);
            var i: usize = 0;
            while (i < items.len) {
                var j = i + 1;
                while (j < items.len and items[j] == items[j - 1] + 1) : (j += 1) {}
                if (j - i < best_len) {
                    best_len = j - i;
                    best_start = i;
                }
                i = j;
            }
            const e = self.unclaimed.orderedRemove(best_start);
            try out.append(self.gpa, e);
            try self.returnable.append(self.gpa, e);
        }
        self.checkClosure();
    }

    /// A shrink the page write refused: the extents go back unclaimed.
    pub fn undoShrink(self: *RegionGrant, extents: []const u64) Allocator.Error!void {
        for (extents) |e| {
            for (self.returnable.items, 0..) |r, i| if (r == e) {
                _ = self.returnable.swapRemove(i);
                break;
            };
            try self.insertUnclaimed(e);
        }
        self.checkClosure();
    }

    /// Returnable extents the manager's `ReturnExtents` cleared (dropped
    /// from RAM BEFORE the durable return — the caller's order).
    pub fn returnLanded(self: *RegionGrant, extents: []const u64) void {
        for (extents) |e| {
            for (self.returnable.items, 0..) |r, i| if (r == e) {
                _ = self.returnable.swapRemove(i);
                self.returned += 1;
                break;
            };
        }
        self.checkClosure();
    }

    /// The surplus the cadence returns: every returnable extent.
    pub fn surplus(self: *const RegionGrant) []const u64 {
        return self.returnable.items;
    }

    /// The return screen for an in-process region: CLAIMED (a live image)
    /// or PENDING (its free parked behind the tail) ⇒ `GrantHeld`.
    pub fn returnable_(self: *const RegionGrant, extent: u64) error{GrantHeld}!void {
        if (self.claimed.contains(extent)) return error.GrantHeld;
        for (self.pending.items) |p| if (p.extent == extent) return error.GrantHeld;
    }

    /// The refill decision's 50 % law against the reference.
    pub fn refillIsDue(self: *const RegionGrant) bool {
        return refillDue(self.unclaimedCount(), self.refill_reference);
    }

    pub fn noteRefillDecision(self: *RegionGrant) void {
        self.refill_reference = self.unclaimedCount();
    }
};

// ---------------------------------------------------------------------------
// The claim/return model.
// ---------------------------------------------------------------------------

/// A claimer and a returner over ONE region grant with ONE guard word (the
/// region's `grant` spin lock): the returner drops an unclaimed extent from
/// RAM, then the durable return clears its bit; the claimer takes the
/// lowest unclaimed. The invariant: an extent is never both CLAIMED (a live
/// image about to be written) and RETURNED (the manager may re-grant it).
/// The weakening — the returner reads the unclaimed set and clears the bit
/// without the lock — lets a claim land between the read and the drop.
pub fn ClaimReturnModel(comptime weakened: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        /// Extent 7: 0 unclaimed, 1 claimed, 2 returned-from-RAM.
        state: u8 = 0,
        lock: bool = false,
        pc: [2]u8 = @splat(0),
        claimer_got: bool = false,
        returner_cleared_bit: bool = false,
        returner_saw_unclaimed: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] >= 3;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == 0) {
                // The claimer: lock, take if unclaimed, unlock.
                switch (self.pc[0]) {
                    0 => if (!self.lock) {
                        self.lock = true;
                        self.pc[0] = 1;
                    },
                    1 => {
                        if (self.state == 0) {
                            self.state = 1;
                            self.claimer_got = true;
                        }
                        self.lock = false;
                        self.pc[0] = 3;
                    },
                    else => {},
                }
                return;
            }
            // The returner.
            if (weakened) {
                switch (self.pc[1]) {
                    0 => {
                        // Read without the lock.
                        self.returner_saw_unclaimed = self.state == 0;
                        self.pc[1] = 1;
                    },
                    1 => {
                        if (self.returner_saw_unclaimed) {
                            // Durable return first, RAM drop after.
                            self.returner_cleared_bit = true;
                        }
                        self.pc[1] = 2;
                    },
                    2 => {
                        if (self.returner_saw_unclaimed and self.state == 0) self.state = 2;
                        self.pc[1] = 3;
                    },
                    else => {},
                }
                return;
            }
            switch (self.pc[1]) {
                0 => if (!self.lock) {
                    self.lock = true;
                    self.pc[1] = 1;
                },
                1 => {
                    // Under the lock: unclaimed ⇒ drop from RAM first.
                    self.returner_saw_unclaimed = self.state == 0;
                    if (self.returner_saw_unclaimed) self.state = 2;
                    self.lock = false;
                    self.pc[1] = 2;
                },
                2 => {
                    // The durable return of what RAM no longer holds.
                    if (self.returner_saw_unclaimed) self.returner_cleared_bit = true;
                    self.pc[1] = 3;
                },
                else => {},
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.claimer_got and self.returner_cleared_bit) return error.ClaimedAndReturned;
        }

        pub fn final(self: *const Self) !void {
            // Exactly one of them owns the extent at the end.
            if (self.claimer_got == self.returner_cleared_bit) return error.NoOwnerOrTwo;
        }
    };
}

// ---------------------------------------------------------------------------
// Tests (spec §7 "Manager / grants").
// ---------------------------------------------------------------------------

const testing = std.testing;

test "grant size derivation: floor 8, the heap-share cap, the 2× headroom; the failover bound; the wire cap; want screening; the knob tie; the pool target and the refill laws" {
    try testing.expectEqual(@as(u64, 46_500), failoverBoundMs(1000, 500));
    // ewma 0 ⇒ the floor; the 8 k-peak row: 5 SMO/s × 2 × 46.5 s ≈ 465
    // extents (spec §6.4 "≈ 470"); a thin heap caps it.
    try testing.expectEqual(GRANT_EXTENTS_FLOOR, grantDerived(0, 46_500, 1 << 20, 195));
    try testing.expectEqual(@as(u64, 465), grantDerived(5000, 46_500, 1 << 22, 195));
    try testing.expectEqual(@as(u64, 100), grantDerived(5000, 46_500, 400 * 195, 195));
    // The cap below the floor: the floor wins (clamp's lo-wins law).
    try testing.expectEqual(GRANT_EXTENTS_FLOOR, grantDerived(5000, 46_500, 10, 195));
    try testing.expectEqual(GRANT_EXTENTS_MAX, grantDerived(1 << 40, 46_500, 1 << 40, 1));
    try testing.expectEqual(@as(u64, 1 << 18), grantWireCap(1 << 22, 4));
    try testing.expectEqual(GRANT_EXTENTS_FLOOR, grantWireCap(4, 4));
    try testing.expectEqual(@as(u64, 100), try clampGrantWant(0, 100));
    try testing.expectEqual(@as(u64, 60), try clampGrantWant(60, 100));
    try testing.expectError(error.Rejected, clampGrantWant(101, 100));
    try testing.expectError(error.Rejected, clampGrantWant(GRANT_EXTENTS_MAX + 1, 1 << 40));
    try testing.expectEqual(@as(u64, 77), resolveGrantExtents(77, 8));
    try testing.expectEqual(@as(u64, 8), resolveGrantExtents(null, 8));
    try testing.expectEqual(@as(u64, 8 + 16), joinedPoolFloor(16));
    try testing.expectEqual(@as(u64, 24), joinedPoolTarget(8, 0, 24, 1000));
    try testing.expectEqual(@as(u64, 110), joinedPoolTarget(100, 10, 24, 1000));
    try testing.expectEqual(@as(u64, 50), joinedPoolTarget(100, 10, 24, 50));
    try testing.expectEqual(@as(u64, 60), joinedPoolTarget(1, 60, 24, 50)); // promised floors even above the cap
    try testing.expect(refillDue(4, 8) and !refillDue(5, 8) and refillDue(0, 0));
    try testing.expect(joinedRefillDue(3, 8) and !joinedRefillDue(4, 8));
}

test "a wire joiner's derived size, its SMO-rate fold and its refill mark: the smoke's storm (≈ 20 images/s) at the 46.5 s bound asks ≈ 1,860 (the manager's heap share caps it); ewma 0 is the floor; the fold is 7/8 + 1/8 of images/elapsed; a zero-length cycle folds nothing; the max is the ceiling; the mark is the first free count `joinedRefillDue` judges due" {
    try testing.expectEqual(GRANT_EXTENTS_FLOOR, joinedGrantDerived(0, 46_500));
    try testing.expectEqual(@as(u64, 1860), joinedGrantDerived(20_000, 46_500));
    try testing.expectEqual(GRANT_EXTENTS_MAX, joinedGrantDerived(1 << 40, 46_500));
    // 64 first images in one second = 64,000 milli-SMO/s, folded 1/8 in.
    try testing.expectEqual(@as(u64, 8_000), smoRateFold(0, 64, std.time.ns_per_s));
    // 7/8 · 20,000 + 1/8 · 4,000 (4 images in a second).
    try testing.expectEqual(@as(u64, 18_000), smoRateFold(20_000, 4, std.time.ns_per_s));
    try testing.expectEqual(@as(u64, 20_000), smoRateFold(20_000, 7, 0));
    // The floor-sized pool (72) the storm drained in the kill row: with
    // the measured rate the target is the law's, not the join's floor.
    try testing.expect(joinedPoolTarget(joinedGrantDerived(8_000, 46_500), 0, joinedPoolFloor(64), GRANT_EXTENTS_MAX) > 72);
    // The mark: target 72, nothing promised ⇒ due at 35 free, not at 36.
    try testing.expectEqual(@as(u64, 35), joinedRefillMark(72, 0));
    try testing.expect(joinedRefillDue(35, 72) and !joinedRefillDue(36, 72));
    try testing.expectEqual(@as(u64, 45), joinedRefillMark(72, 10));
    try testing.expect(joinedRefillDue(45 - 10, 72) and !joinedRefillDue(46 - 10, 72));
    try testing.expectEqual(@as(u64, 0), joinedRefillMark(1, 0));
}

test "grant − reached is normalized runs" {
    var out: std.ArrayList(GrantRun) = .empty;
    defer out.deinit(testing.allocator);
    try subtractExtents(testing.allocator, &.{ .{ .start = 10, .len = 6 }, .{ .start = 30, .len = 2 } }, &.{ 12, 13, 30, 31 }, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    try testing.expectEqual(@as(u64, 10), out.items[0].start);
    try testing.expectEqual(@as(u32, 2), out.items[0].len);
    try testing.expectEqual(@as(u64, 14), out.items[1].start);
    try testing.expectEqual(@as(u32, 2), out.items[1].len);
}

test "a lease's custody move (0003 §AD.1): a gain adds exactly the images the record did not hold and normalizes (an image adjacent to a run joins it); a loss removes exactly the images it held, splitting runs; gain then lose of the same images restores the record; runsContain is the runs' membership" {
    const gpa = testing.allocator;
    const record = [_]GrantRun{ .{ .start = 10, .len = 4 }, .{ .start = 40, .len = 2 } };
    for ([_]u64{ 9, 14, 39, 42 }) |e| try testing.expect(!runsContain(&record, e));
    for ([_]u64{ 10, 13, 40, 41 }) |e| try testing.expect(runsContain(&record, e));
    // T1: 12 is held already; 14 joins [10, 14); 20 and 50 are new runs.
    var g = try RecordMove.gain(gpa, &record, &.{ 12, 14, 20, 50 });
    defer g.deinit(gpa);
    try testing.expectEqualSlices(u64, &.{ 14, 20, 50 }, g.moved.items);
    try testing.expectEqualSlices(GrantRun, &.{ .{ .start = 10, .len = 5 }, .{ .start = 20, .len = 1 }, .{ .start = 40, .len = 2 }, .{ .start = 50, .len = 1 } }, g.runs.items);
    // T5 of the same images off the gained record: back to the record.
    var l = try RecordMove.lose(gpa, g.runs.items, &.{ 14, 20, 50 });
    defer l.deinit(gpa);
    try testing.expectEqualSlices(u64, &.{ 14, 20, 50 }, l.moved.items);
    try testing.expectEqualSlices(GrantRun, &record, l.runs.items);
    // A loss of an image the record never held moves nothing; one inside a
    // run splits it.
    var s = try RecordMove.lose(gpa, &record, &.{ 11, 77 });
    defer s.deinit(gpa);
    try testing.expectEqualSlices(u64, &.{11}, s.moved.items);
    try testing.expectEqualSlices(GrantRun, &.{ .{ .start = 10, .len = 1 }, .{ .start = 12, .len = 2 }, .{ .start = 40, .len = 2 } }, s.runs.items);
    // The RAM grant follows: IN claims the images it does not hold, OUT
    // returns the claimed ones it held; the closure holds across both.
    var rg = RegionGrant.init(gpa);
    defer rg.deinit();
    _ = try rg.absorb(&record);
    const c = try rg.claim();
    try testing.expectEqual(@as(u64, 2), try rg.transferIn(&.{ c, 50, 51 }));
    try testing.expect(rg.closureHolds());
    try testing.expectEqual(@as(u64, 1), rg.transferOut(&.{ 50, 99 }));
    try testing.expect(rg.closureHolds());
    try testing.expectEqual(rg.granted, rg.held() + rg.unclaimedCount() + rg.returned);
}

test "the return law's screens: a run past the volume or overflowing is rejected at the first offender; coalesce is bounded by the frame; the intersection by the record" {
    try validateReturnRuns(&.{ .{ .start = 0, .len = 5 }, .{ .start = 95, .len = 5 } }, 100);
    try testing.expectError(error.Rejected, validateReturnRuns(&.{.{ .start = 96, .len = 5 }}, 100));
    try testing.expectError(error.Rejected, validateReturnRuns(&.{.{ .start = std.math.maxInt(u64), .len = 1 }}, 100));
    try testing.expectError(error.Rejected, validateReturnRuns(&.{.{ .start = 1, .len = 0 }}, 100));
    var runs = [_]GrantRun{ .{ .start = 10, .len = 5 }, .{ .start = 12, .len = 10 }, .{ .start = 1, .len = 2 }, .{ .start = 22, .len = 1 }, .{ .start = 40, .len = 1 } };
    const c = coalesceRuns(&runs);
    try testing.expectEqualSlices(GrantRun, &.{ .{ .start = 1, .len = 2 }, .{ .start = 10, .len = 13 }, .{ .start = 40, .len = 1 } }, c);
    // 250 K copies of one run coalesce to ONE.
    const many = try testing.allocator.alloc(GrantRun, 250_000);
    defer testing.allocator.free(many);
    for (many) |*r| r.* = .{ .start = 7, .len = 3 };
    try testing.expectEqual(@as(usize, 1), coalesceRuns(many).len);
    var out: std.ArrayList(GrantRun) = .empty;
    defer out.deinit(testing.allocator);
    try intersectWithRecord(testing.allocator, c, &.{ .{ .start = 0, .len = 12 }, .{ .start = 20, .len = 5 } }, &out);
    try testing.expectEqualSlices(GrantRun, &.{ .{ .start = 1, .len = 2 }, .{ .start = 10, .len = 2 }, .{ .start = 20, .len = 3 } }, out.items);
    // The page names the largest 4 runs, ascending.
    const t = pageRunsOf(&.{ .{ .start = 100, .len = 1 }, .{ .start = 10, .len = 5 }, .{ .start = 50, .len = 3 }, .{ .start = 70, .len = 3 }, .{ .start = 30, .len = 9 }, .{ .start = 90, .len = 2 } });
    try testing.expectEqualSlices(page.Extent, &.{ .{ .start = 10, .len = 5 }, .{ .start = 30, .len = 9 }, .{ .start = 50, .len = 3 }, .{ .start = 70, .len = 3 } }, t.slice());
    try testing.expectEqual(@as(u8, 2), pageRunsOf(&.{ .{ .start = 1, .len = 1 }, .{ .start = 5, .len = 1 } }).len);
    try testing.expectEqual(@as(u8, 0), pageRunsOf(&.{}).len);
}

test "the RAM grant claims the smallest run first, parks frees on its tail, recycles past it, shrinks smallest-first, and the closure holds across every step" {
    const gpa = testing.allocator;
    var g = RegionGrant.init(gpa);
    defer g.deinit();
    try testing.expectEqual(@as(u64, 7), try g.absorb(&.{ .{ .start = 10, .len = 4 }, .{ .start = 20, .len = 1 }, .{ .start = 30, .len = 2 } }));
    try testing.expectEqual(@as(u64, 7), g.granted);
    try testing.expectEqual(@as(u64, 7), g.refill_reference);
    // A verbatim replay adds nothing.
    try testing.expectEqual(@as(u64, 0), try g.absorb(&.{.{ .start = 10, .len = 4 }}));
    // The smallest run (20) first, then the pair, then the quad.
    try testing.expectEqual(@as(u64, 20), try g.claim());
    try testing.expectEqual(@as(u64, 30), try g.claim());
    try testing.expectEqual(@as(u64, 31), try g.claim());
    try testing.expectEqual(@as(u64, 10), try g.claim());
    try testing.expect(g.closureHolds());
    try testing.expectEqual(@as(u64, 4), g.held());
    try testing.expect(g.refillIsDue()); // 3 × 2 ≤ 7
    // Frees park on the tail; the tail covering releases them.
    try g.freePending(20, 500);
    try g.freePending(30, 900);
    try testing.expectError(error.NotClaimed, g.freePending(20, 1));
    try testing.expectError(error.GrantHeld, g.returnable_(31));
    try testing.expectError(error.GrantHeld, g.returnable_(20));
    try g.returnable_(11);
    try testing.expectEqual(@as(u64, 1), try g.advanceDurable(600));
    try testing.expectEqual(@as(u64, 1), g.returnable.items.len);
    try testing.expectEqual(@as(u64, 20), g.returnable.items[0]);
    // Recycle below keep; past keep the rest is surplus.
    try testing.expectEqual(@as(u64, 1), try g.recycle(10));
    try testing.expectEqual(@as(u64, 4), g.unclaimedCount());
    try testing.expectEqual(@as(u64, 1), try g.advanceDurable(1000));
    try testing.expectEqual(@as(u64, 0), try g.recycle(4));
    try testing.expectEqualSlices(u64, &.{30}, g.surplus());
    g.returnLanded(&.{30});
    try testing.expectEqual(@as(u64, 1), g.returned);
    try testing.expect(g.closureHolds());
    // Shrink to 2: smallest runs first (the single 20 then the run 11..13's... lowest).
    var out: std.ArrayList(u64) = .empty;
    defer out.deinit(gpa);
    try g.shrinkTo(2, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    try testing.expectEqual(@as(u64, 20), out.items[0]);
    try testing.expectEqual(@as(u64, 2), g.unclaimedCount());
    try g.undoShrink(out.items);
    try testing.expectEqual(@as(u64, 4), g.unclaimedCount());
    out.clearRetainingCapacity();
    try g.shrinkTo(2, &out);
    g.returnLanded(out.items);
    try testing.expectEqual(@as(u64, 3), g.returned);
    try testing.expect(g.closureHolds());
    var runs: std.ArrayList(GrantRun) = .empty;
    defer runs.deinit(gpa);
    try g.unclaimedRuns(gpa, &runs);
    try testing.expectEqual(@as(u64, 2), runs.items[0].len + (if (runs.items.len > 1) runs.items[1].len else 0));
    // Exhaustion is the typed class.
    _ = try g.claim();
    _ = try g.claim();
    try testing.expectError(error.GrantExhausted, g.claim());
    try testing.expect(g.closureHolds());
}

test "property: the closure holds under a random walk of claim / free / advance / recycle / shrink / return" {
    const gpa = testing.allocator;
    var g = RegionGrant.init(gpa);
    defer g.deinit();
    var prng = std.Random.DefaultPrng.init(0x6A17);
    const r = prng.random();
    var out: std.ArrayList(u64) = .empty;
    defer out.deinit(gpa);
    var tail: u64 = 0;
    for (0..4000) |_| {
        switch (r.intRangeLessThan(u8, 0, 7)) {
            0 => _ = try g.absorb(&.{.{ .start = r.intRangeLessThan(u64, 0, 2000), .len = r.intRangeAtMost(u32, 1, 6) }}),
            1 => _ = g.claim() catch |e| try testing.expectEqual(error.GrantExhausted, e),
            2 => {
                var it = g.claimed.keyIterator();
                if (it.next()) |k| try g.freePending(k.*, tail + r.intRangeAtMost(u64, 0, 50));
            },
            3 => {
                tail += r.intRangeAtMost(u64, 0, 40);
                _ = try g.advanceDurable(tail);
            },
            4 => _ = try g.recycle(r.intRangeAtMost(u64, 0, 40)),
            5 => {
                out.clearRetainingCapacity();
                try g.shrinkTo(r.intRangeAtMost(u64, 0, 30), &out);
                if (r.boolean()) try g.undoShrink(out.items) else g.returnLanded(out.items);
            },
            else => g.returnLanded(g.surplus()),
        }
        try testing.expect(g.closureHolds());
    }
}

test "model: a claim and a return of one extent under the grant lock never produce a claimed-and-returned extent; the lockless return fails" {
    try explorer.expectHolds(ClaimReturnModel(false), .{});
    try explorer.expectFails(ClaimReturnModel(true), .{}, error.InvariantViolated);
}
