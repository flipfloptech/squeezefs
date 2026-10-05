//! The lock-free extent-allocator core (charter §5.3 "Space", §5.1 allocator
//! bitmap; `docs/porting/kv_alloc_superblock.md` §4.3–§4.5). Standalone and
//! dependency-free: no device, no counters, no log — the I/O half
//! (`alloc_ext.zig`) adds bytes and observability on top, and the exhaustive
//! model (`alloc_core_model.zig`) re-states every protocol here one atomic
//! action per step for `model/explorer.zig`.
//!
//! Three protocols share one core:
//!
//! - **Claim** = budget-then-bit. One bit per extent in `words` (set ==
//!   allocated OR pending-free); a `fetchOr` decides the unique 0→1 winner.
//!   The claim first wins an entitlement on the partition's `free_budget`
//!   (a CAS against the class floor), THEN scans for a clear bit. Invariant:
//!   `clear_bits ≥ free_budget + budget-winners-without-a-bit`. Every
//!   release path CLEARS THE BIT BEFORE INCREMENTING THE BUDGET, so a budget
//!   winner's clear bit is already visible when its scan runs (the budget
//!   CAS's acquire pairs with the release's acq_rel increment). A racing
//!   claimer may steal the specific bit a scanner eyed — never the last one
//!   it is entitled to — so the scan retries and terminates; `claim_rescan_limit`
//!   bounds the retry (RES-14: the pre-fix loop spun forever on drift).
//! - **Pending-free coverage gate.** A freed extent enters its OWNER
//!   partition's bounded Vyukov FIFO as `(extent, gate_seq)` with the bit
//!   STILL SET and the budget NOT credited. `gate_seq` is the freeing SMO
//!   entry's free-record journal POSITION. It is released only by
//!   `advanceDurableIn(writer, tail)` where `tail` is the `journal_tail_seq`
//!   of a root-ledger record written by THAT appender that a COMPLETED
//!   barrier covers: `gate ≤ tail` (tails are entry boundaries and the free
//!   record is never record 0 of an SMO entry, so `≤` ≡ the design's strict
//!   `tail > free.seq`). Coverage of the TAIL — never durability of a ledger
//!   RECORD — is what proves every flip of the freeing entry is materialized,
//!   so any ledger slot a mount may select references only never-overwritten
//!   extents. The drain stops at the first uncovered entry; complete because
//!   gates are non-decreasing in push order (one serialized producer per
//!   partition). The FIFO cap is a PRESSURE VALVE, never a liveness gate:
//!   `freePendingForced` parks in an unbounded overflow at cap (the flush
//!   pass's own compactions and mount-side re-parking must never be refused —
//!   refusing them closes the pinned-floor dependency cycle {parked frees ↔
//!   pinned tail ↔ refused compaction}, P2 2026-07-26 §9).
//! - **Reserve accounting.** `reserve` extents (`max(8, 2 %)`) are claimable
//!   by `.internal` only; a `.user` claim fails once the budget would dip
//!   to-or-below the reserve — ENOSPC while compaction can still fold and
//!   free space (no write-to-free-space deadlock).
//!
//! **Append partitioning** splits the bitmap by PAGE (the A/B write unit),
//! ownership `page % writers` (interleaved: a dense low-end allocation spreads
//! evenly). Each partition carries its own budget, hint, reserve floor,
//! FIFO + overflow and coverage clock — gate seqs are positions in the
//! freeing appender's OWN ring, so one shared clock would release a peer's
//! parked extent that replay still routes into. Solo is one partition owning
//! everything: the shipped flat sweep, the shipped costs.

const std = @import("std");
const Atomic = std.atomic.Value;

/// Who is asking for an extent.
pub const AllocClass = enum {
    /// A user-op allocation: refused once the free budget would dip
    /// to-or-below the compaction reserve — the op surfaces ENOSPC.
    user,
    /// Compaction / checkpoint / SMO internals: may consume the reserve so
    /// the tree can always fold appends and free space at user ENOSPC.
    internal,
};

pub const ClaimError = error{
    /// For `.user`: `free ≤ reserve` with the reserve intact (ENOSPC). For
    /// `.internal`: a genuinely exhausted heap.
    NoSpace,
    /// RES-14: the budget CAS won an entitlement the bitmap could not honour
    /// after `claim_rescan_limit` full rescans. Budget and bitmap disagree,
    /// which the protocol says is impossible; kept distinct from `NoSpace`
    /// so an operator never reads a broken invariant as an honest full heap.
    /// The entitlement is returned to the budget before this is raised.
    InvariantDrift,
};

pub const PendingFreeError = error{
    /// The bounded FIFO is genuinely full (cursor distance == cap): the
    /// caller forces a checkpoint + durable advance, never reuses unsafely.
    PendingFreeFull,
};

/// Full-bitmap rescans a claim makes before declaring `InvariantDrift`. A
/// rescan is only ever needed because a concurrent release landed behind the
/// cursor, so the legitimate need is O(racing releases); 64 is a generous
/// ceiling on that, not a tuning knob (the pre-fix loop had no exit at all).
pub const claim_rescan_limit: u32 = 64;

/// The smallest legal FIFO: a 1-slot Vyukov ring aliases its stamps — "full
/// at position p" and "vacant for p+1" both read `p + 1`, silently
/// overwriting an un-drained entry (a coverage-gate bypass).
pub const min_pending_cap: usize = 2;

/// Consecutive barriered cycles with retirements parked, no release and no
/// tail advance before the volume latches FAILED (the wedge class). Healthy
/// convergence is ≤ 2 cycles (the first discharges dying floors, the
/// second's tail covers the parked frees); 8 leaves only a tail nothing can
/// discharge (a stuck in-flight reservation).
pub const pending_free_force_cycles: u64 = 8;

/// The §4.7 compaction reserve: `max(8, total / 50)`. 8 = three roots plus
/// growth headroom (`plan` requires reserve + 4); 2 % = the measured
/// fold-and-free headroom. Internal-class only, so compaction can always run
/// at user ENOSPC.
pub fn compactionReserveExtents(total: u64) u64 {
    return @max(8, total / 50);
}

/// The heap-admission floor for net-zero compactions (deletes, overwrites):
/// half the reserve. The other half stays the flush pass's own for interior
/// cascades and projection under-estimates; splits need the WHOLE reserve.
pub fn compactionFloorExtents(reserve: u64) u64 {
    return reserve / 2;
}

/// How the bitmap is split across appenders: the partition unit is the
/// bitmap PAGE (the A/B write unit) and ownership is `page % writers`.
pub const PartitionMap = struct {
    writers: u64,
    extents_per_page: u64,

    /// The shipped single-appender map: one partition owning every page.
    pub fn solo(extents_per_page: u64) PartitionMap {
        return new(1, extents_per_page);
    }

    pub fn new(writers: u64, extents_per_page: u64) PartitionMap {
        std.debug.assert(writers > 0);
        std.debug.assert(extents_per_page > 0);
        return .{ .writers = writers, .extents_per_page = extents_per_page };
    }

    pub fn isSolo(self: PartitionMap) bool {
        return self.writers == 1;
    }

    pub fn ownerOfPage(self: PartitionMap, page: u64) u64 {
        return page % self.writers;
    }

    /// Solo answers 0 without a division (the claim/release hot path).
    pub fn ownerOfExtent(self: PartitionMap, extent: u64) u64 {
        if (self.writers == 1) return 0;
        return self.ownerOfPage(extent / self.extents_per_page);
    }
};

/// One FIFO slot, Vyukov-stamped: a producer at position `pos` fills the
/// slot only while `stamp == pos` and publishes with `stamp = pos + 1`; the
/// consumer that wins `pos` vacates with `stamp = pos + cap`.
const PendingSlot = struct {
    stamp: Atomic(u64),
    extent: Atomic(u64),
    retire_seq: Atomic(u64),
};

const OverflowEntry = struct { extent: u64, gate: u64 };

/// The overflow's guard. A spin lock, not a futex: this is the cold pressure
/// path (empty in the steady state — `overflow_len` keeps the hot paths off
/// it), the critical section is one deque push or pop, and no `std.Io`
/// handle exists on the paths that reach it (charter §12 clocks/mutex law).
const SpinLock = struct {
    state: Atomic(u32) = .init(0),

    fn lock(self: *SpinLock) void {
        // acquire pairs with unlock's release store: the critical section
        // sees the previous holder's deque mutations.
        while (self.state.cmpxchgWeak(0, 1, .acquire, .monotonic) != null) std.atomic.spinLoopHint();
    }

    fn unlock(self: *SpinLock) void {
        self.state.store(0, .release);
    }
};

/// One appender's half of the allocator: its budget, scan hint, FIFO +
/// overflow and its own coverage clock. Cursors sit on their own lines: the
/// producer (SMO task) and consumer (post-barrier drain) touch different
/// words and must not false-share.
const Partition = struct {
    free_budget: Atomic(u64) align(std.atomic.cache_line),
    hint: Atomic(u64),
    /// Non-decreasing-gate debug guard (per partition: one serialized
    /// producer each).
    last_retire_seq: Atomic(u64),
    pending_head: Atomic(u64) align(std.atomic.cache_line),
    pending_tail: Atomic(u64) align(std.atomic.cache_line),
    durable_seq: Atomic(u64),
    pending: []PendingSlot,
    overflow_lock: SpinLock,
    overflow: std.ArrayList(OverflowEntry),
    /// Index of the overflow's front entry (a deque over an ArrayList: pops
    /// advance the front, a drain that empties it resets both).
    overflow_front: usize,
    /// Lock-free mirror of the overflow's length for the hot-path emptiness
    /// check.
    overflow_len: Atomic(u64),

    fn init(gpa: std.mem.Allocator, budget: u64, first_extent: u64, pending_cap: usize) !Partition {
        const pending = try gpa.alloc(PendingSlot, pending_cap);
        for (pending, 0..) |*slot, i| slot.* = .{ .stamp = .init(i), .extent = .init(0), .retire_seq = .init(0) };
        return .{
            .free_budget = .init(budget),
            .hint = .init(first_extent),
            .last_retire_seq = .init(0),
            .pending_head = .init(0),
            .pending_tail = .init(0),
            .durable_seq = .init(0),
            .pending = pending,
            .overflow_lock = .{},
            .overflow = .empty,
            .overflow_front = 0,
            .overflow_len = .init(0),
        };
    }

    fn deinit(self: *Partition, gpa: std.mem.Allocator) void {
        gpa.free(self.pending);
        self.overflow.deinit(gpa);
    }

    fn fifoCount(self: *const Partition) u64 {
        const head = self.pending_head.load(.acquire);
        const tail = self.pending_tail.load(.acquire);
        return head -| tail;
    }
};

/// Extents in `writer`'s partition — its initial free budget.
fn partitionExtentCount(total: u64, map: PartitionMap, writer: u64) u64 {
    if (map.isSolo()) return total;
    const epp = map.extents_per_page;
    const pages = (total + epp - 1) / epp;
    var count: u64 = 0;
    var page = writer;
    while (page < pages) : (page += map.writers) {
        const lo = page * epp;
        count += @min(lo + epp, total) - lo;
    }
    return count;
}

/// The first extent `writer` owns (its scan-hint seed).
fn firstExtentOf(map: PartitionMap, writer: u64) u64 {
    return if (map.isSolo()) 0 else writer * map.extents_per_page;
}

pub const ExtCore = struct {
    gpa: std.mem.Allocator,
    /// One bit per extent, SHARED across partitions (a bit is a bit); every
    /// mutation routes to the OWNING partition's accounting.
    words: []Atomic(u64),
    total: u64,
    /// Per-partition reserve (the whole-volume reserve on a solo volume).
    reserve: u64,
    map: PartitionMap,
    parts: []Partition,

    /// An all-free solo core: the shipped single-appender allocator.
    pub fn init(gpa: std.mem.Allocator, total: u64, reserve: u64, pending_cap: usize) !ExtCore {
        // One partition owning every page: `extents_per_page = total` makes
        // the whole bitmap page 0 by construction.
        return initPartitioned(gpa, total, reserve, pending_cap, PartitionMap.solo(total));
    }

    /// `reserve` and `pending_cap` are WHOLE-volume figures divided across
    /// appenders (each keeps its own fold-and-free headroom; a partition's
    /// SMO rate is ~1/N of the volume's, so the footprint stays constant).
    pub fn initPartitioned(gpa: std.mem.Allocator, total: u64, reserve: u64, pending_cap: usize, map: PartitionMap) !ExtCore {
        std.debug.assert(total > 0);
        std.debug.assert(reserve < total);
        std.debug.assert(pending_cap >= min_pending_cap);
        const writers = map.writers;
        const per_reserve = if (writers == 1) reserve else (reserve + writers - 1) / writers;
        const per_pending: usize = if (writers == 1) pending_cap else @max(min_pending_cap, pending_cap / @as(usize, @intCast(writers)));

        const words = try gpa.alloc(Atomic(u64), (total + 63) / 64);
        errdefer gpa.free(words);
        for (words) |*w| w.* = .init(0);

        const parts = try gpa.alloc(Partition, @intCast(writers));
        var built: usize = 0;
        errdefer {
            for (parts[0..built]) |*p| p.deinit(gpa);
            gpa.free(parts);
        }
        for (parts, 0..) |*p, w| {
            const budget = partitionExtentCount(total, map, w);
            std.debug.assert(per_reserve < budget or budget == 0);
            p.* = try Partition.init(gpa, budget, firstExtentOf(map, w), per_pending);
            built += 1;
        }
        return .{ .gpa = gpa, .words = words, .total = total, .reserve = per_reserve, .map = map, .parts = parts };
    }

    pub fn deinit(self: *ExtCore) void {
        for (self.parts) |*p| p.deinit(self.gpa);
        self.gpa.free(self.parts);
        self.gpa.free(self.words);
        self.* = undefined;
    }

    /// INDEX, never modulo: `writer % parts.len` on a runtime length emits a
    /// 64-bit division on the claim/release path — measured ~7 ns/claim,
    /// ≈ 11 % of a solo claim (`.benchmarks/2026-08-05-mw-partitioned-append.md`
    /// §3). An out-of-range writer is a caller bug.
    fn part(self: *const ExtCore, writer: u64) *Partition {
        std.debug.assert(writer < self.parts.len);
        return &self.parts[@intCast(writer)];
    }

    /// Claimable extents across every partition (excludes allocated and
    /// pending-free).
    pub fn freeExtents(self: *const ExtCore) u64 {
        var sum: u64 = 0;
        for (self.parts) |*p| sum += p.free_budget.load(.acquire);
        return sum;
    }

    pub fn freeExtentsIn(self: *const ExtCore, writer: u64) u64 {
        return self.part(writer).free_budget.load(.acquire);
    }

    /// Parked retirements awaiting coverage: the FIFO plus the overflow,
    /// summed over partitions (the wedge detector counts every parked
    /// retirement wherever it is parked).
    pub fn pendingCount(self: *const ExtCore) u64 {
        var sum: u64 = 0;
        for (self.parts) |*p| sum += p.fifoCount() + p.overflow_len.load(.acquire);
        return sum;
    }

    pub fn pendingCountIn(self: *const ExtCore, writer: u64) u64 {
        const p = self.part(writer);
        return p.fifoCount() + p.overflow_len.load(.acquire);
    }

    /// Producer-side headroom probe (the at-cap protocol's admission check).
    /// Sound for the serialized SMO task — the FIFO's only producer — because
    /// a racing drain only ever VACATES slots, so headroom observed here
    /// still holds at the later push (the model pins exactly this). Speaks
    /// only for the bounded FIFO; forced retirements never consult it.
    pub fn pendingHasRoomIn(self: *const ExtCore, writer: u64) bool {
        const p = self.part(writer);
        return p.fifoCount() < p.pending.len;
    }

    pub fn pendingHasRoom(self: *const ExtCore) bool {
        return self.pendingHasRoomIn(0);
    }

    /// Free FIFO slots in `writer`'s partition (a merge parks two).
    pub fn pendingRoomIn(self: *const ExtCore, writer: u64) u64 {
        const p = self.part(writer);
        return @as(u64, p.pending.len) -| p.fifoCount();
    }

    pub fn durableSeqIn(self: *const ExtCore, writer: u64) u64 {
        return self.part(writer).durable_seq.load(.acquire);
    }

    pub fn durableSeq(self: *const ExtCore) u64 {
        return self.durableSeqIn(0);
    }

    pub fn claim(self: *const ExtCore, class: AllocClass) ClaimError!u64 {
        return self.claimIn(0, class);
    }

    /// Budget-then-bit (module docs). `.user` must leave the partition's
    /// reserve behind; the entitlement CAS is acq_rel so the winner sees
    /// every bit a release cleared before its budget increment.
    pub fn claimIn(self: *const ExtCore, writer: u64, class: AllocClass) ClaimError!u64 {
        const p = self.part(writer);
        const floor: u64 = switch (class) {
            .user => self.reserve,
            .internal => 0,
        };
        var free = p.free_budget.load(.acquire);
        while (true) {
            if (free <= floor) return error.NoSpace;
            // acq_rel pairs with `release`'s acq_rel `fetchAdd` on the budget.
            free = p.free_budget.cmpxchgWeak(free, free - 1, .acq_rel, .acquire) orelse break;
        }
        const start = @min(p.hint.load(.monotonic), self.total);
        var pass: u32 = 0;
        while (pass < claim_rescan_limit) : (pass += 1) {
            if (self.map.isSolo()) {
                // The shipped flat sweep: `start..total` then `0..start`.
                var k: u64 = 0;
                while (k < self.total) : (k += 1) {
                    const idx = if (start + k < self.total) start + k else start + k - self.total;
                    if (self.tryTakeBit(idx)) {
                        p.hint.store(idx + 1, .monotonic);
                        return idx;
                    }
                }
            } else if (self.scanPartition(writer, p, start)) |idx| {
                return idx;
            }
            // A release between the budget win and this pass can land behind
            // the cursor; the budget win guarantees a clear bit exists.
            std.atomic.spinLoopHint();
        }
        // RES-14: hand the entitlement back and refuse loud in its own class.
        _ = p.free_budget.fetchAdd(1, .acq_rel);
        return error.InvariantDrift;
    }

    /// The unique-winner bit take: the 0→1 `fetchOr` winner is unique.
    fn tryTakeBit(self: *const ExtCore, idx: u64) bool {
        const mask = @as(u64, 1) << @intCast(idx % 64);
        return self.words[@intCast(idx / 64)].fetchOr(mask, .acq_rel) & mask == 0;
    }

    /// One partitioned scan pass over `writer`'s own pages only, ascending
    /// from `start`'s page and wrapping once to the partition's first page;
    /// pass 3 covers the head of the hint's own page that pass 1 skipped.
    /// Page granularity skips a foreign page in one step instead of one
    /// modulo per bit. `writer`'s pages are `{writer, writer+n, …}`.
    fn scanPartition(self: *const ExtCore, writer: u64, p: *Partition, start: u64) ?u64 {
        const epp = self.map.extents_per_page;
        const n = self.map.writers;
        const pages = (self.total + epp - 1) / epp;
        const start_page = @min(start / epp, pages);
        const first = if (start_page <= writer) writer else writer + ((start_page - writer + n - 1) / n) * n;

        // Pass 1: owned pages from `first` to the end; the hint's own page
        // starts at the hint.
        var page = first;
        while (page < pages) : (page += n) {
            const lo = page * epp;
            const hi = @min(lo + epp, self.total);
            const from = if (page == start_page and start > lo and start < hi) start else lo;
            if (self.takeRange(p, from, hi)) |idx| return idx;
        }
        // Pass 2: wrap — owned pages below `first`.
        page = writer;
        while (page < @min(first, pages)) : (page += n) {
            const lo = page * epp;
            if (self.takeRange(p, lo, @min(lo + epp, self.total))) |idx| return idx;
        }
        // Pass 3: the head of the hint's own page, skipped by pass 1.
        if (start_page < pages and self.map.ownerOfPage(start_page) == writer) {
            const lo = start_page * epp;
            if (start > lo) {
                if (self.takeRange(p, lo, @min(start, self.total))) |idx| return idx;
            }
        }
        return null;
    }

    fn takeRange(self: *const ExtCore, p: *Partition, from: u64, to: u64) ?u64 {
        var idx = from;
        while (idx < to) : (idx += 1) {
            if (self.tryTakeBit(idx)) {
                p.hint.store(idx + 1, .monotonic);
                return idx;
            }
        }
        return null;
    }

    /// TEST SEAM (RES-14): inflate the budget without clearing a bit — the
    /// drift the protocol says cannot happen, so the bounded-and-loud refusal
    /// can be pinned deterministically.
    pub fn inflateFreeBudgetForTest(self: *const ExtCore, n: u64) void {
        _ = self.part(0).free_budget.fetchAdd(n, .acq_rel);
    }

    /// Mount seeding ONLY: sets the bit THEN decrements the owner's budget,
    /// transiently violating the invariant in the other direction — hence
    /// `*ExtCore` (never concurrent with claims). Idempotent; out of range
    /// ignored.
    pub fn markAllocated(self: *ExtCore, extent: u64) void {
        if (extent >= self.total) return;
        const mask = @as(u64, 1) << @intCast(extent % 64);
        if (self.words[@intCast(extent / 64)].fetchOr(mask, .acq_rel) & mask == 0) {
            const prev = self.part(self.map.ownerOfExtent(extent)).free_budget.fetchSub(1, .acq_rel);
            std.debug.assert(prev >= 1);
        }
    }

    /// Bit cleared, THEN the owner's budget incremented (module docs). Only
    /// for extents no root-ledger record references (an unpublished build,
    /// or the covered drain below). Idempotent; out of range ignored.
    pub fn release(self: *const ExtCore, extent: u64) void {
        if (extent >= self.total) return;
        const mask = @as(u64, 1) << @intCast(extent % 64);
        if (self.words[@intCast(extent / 64)].fetchAnd(~mask, .acq_rel) & mask != 0) {
            const p = self.part(self.map.ownerOfExtent(extent));
            // acq_rel pairs with the claim's budget CAS: the cleared bit is
            // visible to whoever wins this increment.
            _ = p.free_budget.fetchAdd(1, .acq_rel);
            // Prefer reusing low freed extents (locality under churn).
            _ = p.hint.fetchMin(extent, .monotonic);
        }
    }

    fn bitIsSet(self: *const ExtCore, extent: u64) bool {
        const mask = @as(u64, 1) << @intCast(extent % 64);
        return self.words[@intCast(extent / 64)].load(.acquire) & mask != 0;
    }

    /// Park `extent` in its OWNER's FIFO gated on `retire_seq` (the free
    /// record's journal position). Contract: `extent < total`, bit set, gates
    /// non-decreasing per partition (the serialized SMO task).
    pub fn freePending(self: *const ExtCore, extent: u64, retire_seq: u64) PendingFreeError!void {
        std.debug.assert(extent < self.total);
        std.debug.assert(self.bitIsSet(extent));
        const p = self.part(self.map.ownerOfExtent(extent));
        const prev_retire = p.last_retire_seq.fetchMax(retire_seq, .acq_rel);
        std.debug.assert(prev_retire <= retire_seq);
        const cap: u64 = p.pending.len;
        var pos = p.pending_head.load(.acquire);
        while (true) {
            const slot = &p.pending[@intCast(pos % cap)];
            const stamp = slot.stamp.load(.acquire);
            if (stamp == pos) {
                // Vacant for this position: claim it.
                if (p.pending_head.cmpxchgWeak(pos, pos + 1, .acq_rel, .acquire)) |cur| {
                    pos = cur;
                    continue;
                }
                slot.extent.store(extent, .monotonic);
                slot.retire_seq.store(retire_seq, .monotonic);
                // release pairs with the consumer's acquire stamp load: the
                // payload stores above are visible once the stamp publishes.
                slot.stamp.store(pos + 1, .release);
                return;
            } else if (stamp < pos) {
                // A lap-old stamp is either (a) genuinely full — cursor
                // distance == cap — or (b) a draining consumer won the
                // previous-lap entry (tail bumped) but has not stored the
                // vacate stamp yet. (b) must NOT read as full: the admission
                // headroom probe reads the cursors, so a cursor-level vacancy
                // must be pushable. Distinguish by cursor distance and spin
                // out the consumer's store (it is instructions away).
                const tail = p.pending_tail.load(.acquire);
                if (pos -| tail >= cap) return error.PendingFreeFull;
                std.atomic.spinLoopHint();
                pos = p.pending_head.load(.acquire);
            } else {
                // A racing producer advanced the head past our read.
                pos = p.pending_head.load(.acquire);
            }
        }
    }

    /// `freePending` that can never be refused by the valve: at cap the
    /// entry parks in the unbounded overflow on the same gate and clock.
    /// Returns whether it overflowed. The only failure is the process's
    /// allocator running dry — a different class from the valve, surfaced
    /// typed rather than swallowed.
    pub fn freePendingForced(self: *const ExtCore, extent: u64, retire_seq: u64) std.mem.Allocator.Error!bool {
        self.freePending(extent, retire_seq) catch {
            const p = self.part(self.map.ownerOfExtent(extent));
            p.overflow_lock.lock();
            defer p.overflow_lock.unlock();
            if (p.overflow.items.len > p.overflow_front) {
                std.debug.assert(p.overflow.items[p.overflow.items.len - 1].gate <= retire_seq);
            }
            try p.overflow.append(self.gpa, .{ .extent = extent, .gate = retire_seq });
            p.overflow_len.store(p.overflow.items.len - p.overflow_front, .release);
            return true;
        };
        return false;
    }

    /// `advanceDurableIn(0, …)`: the solo/authority clock.
    pub fn advanceDurable(self: *const ExtCore, seq: u64, ctx: anytype, comptime onRelease: fn (@TypeOf(ctx), u64) void) u64 {
        return self.advanceDurableIn(0, seq, ctx, onRelease);
    }

    /// Advance `writer`'s coverage clock to `seq` (monotonic `fetchMax`;
    /// stale advances are no-ops) and drain every entry it now covers: bit
    /// cleared, budget credited — claimable again, and only now. `onRelease`
    /// fires per released extent (the wrapper dirties its page); returns the
    /// count.
    pub fn advanceDurableIn(self: *const ExtCore, writer: u64, seq: u64, ctx: anytype, comptime onRelease: fn (@TypeOf(ctx), u64) void) u64 {
        const p = self.part(writer);
        _ = p.durable_seq.fetchMax(seq, .acq_rel);
        const durable = p.durable_seq.load(.acquire);
        const cap: u64 = p.pending.len;
        var released: u64 = 0;
        while (true) {
            const pos = p.pending_tail.load(.acquire);
            const head = p.pending_head.load(.acquire);
            if (pos == head) break;
            const slot = &p.pending[@intCast(pos % cap)];
            // acquire pairs with the producer's release stamp store.
            const stamp = slot.stamp.load(.acquire);
            // Unpublished producer (or a racing consumer just vacated it): a
            // later advance drains it.
            if (stamp != pos + 1) break;
            // Peek is stable while stamp == pos + 1: a producer reuses the
            // slot only after a consumer stamps pos + cap, which only the
            // tail-CAS winner below does.
            const retire_seq = slot.retire_seq.load(.monotonic);
            // FIFO order + non-decreasing gates ⇒ nothing behind this entry
            // is releasable either.
            if (retire_seq > durable) break;
            const extent = slot.extent.load(.monotonic);
            if (p.pending_tail.cmpxchgWeak(pos, pos + 1, .acq_rel, .acquire) == null) {
                // Exclusive owner: vacate for the producer a lap ahead, then
                // release the extent.
                slot.stamp.store(pos + cap, .release);
                self.release(extent);
                released += 1;
                onRelease(ctx, extent);
            }
        }
        // The overflow drains on the SAME clock with the same front-stop;
        // lock-free when empty (the steady state).
        if (p.overflow_len.load(.acquire) > 0) {
            p.overflow_lock.lock();
            defer p.overflow_lock.unlock();
            while (p.overflow_front < p.overflow.items.len) {
                const front = p.overflow.items[p.overflow_front];
                if (front.gate > durable) break;
                p.overflow_front += 1;
                self.release(front.extent);
                released += 1;
                onRelease(ctx, front.extent);
            }
            if (p.overflow_front == p.overflow.items.len) {
                p.overflow.clearRetainingCapacity();
                p.overflow_front = 0;
            }
            p.overflow_len.store(p.overflow.items.len - p.overflow_front, .release);
        }
        return released;
    }

    /// Allocated or pending-free (bit set). Out of range reads as free.
    pub fn isAllocated(self: *const ExtCore, extent: u64) bool {
        if (extent >= self.total) return false;
        return self.bitIsSet(extent);
    }

    /// Snapshot the bitmap words into `out` (bit set == allocated or
    /// pending-free) — the wrapper serializes these into A/B pages.
    pub fn snapshotWords(self: *const ExtCore, out: []u64) void {
        std.debug.assert(out.len >= self.words.len);
        for (self.words, 0..) |*w, i| out[i] = w.load(.acquire);
    }

    pub fn wordCount(self: *const ExtCore) usize {
        return self.words.len;
    }
};

// ---------------------------------------------------------------------------
// Heap admission: user commits never claim; they PROMISE projected SMO
// extents (`heap_promised`) and refuse growth ENOSPC while compactions and
// deletes are admitted down to the floor.
// ---------------------------------------------------------------------------

pub const AdmissionVerdict = union(enum) {
    /// Admitted: `delta` more extents are promised (0 when an earlier member
    /// already covers the need).
    admitted: struct { delta: u64, is_split: bool },
    /// Refused: the ENOSPC surface's `{free: claimable, reserve: floor}`.
    no_space: struct { free: u64, reserve: u64 },
};

/// One touched leaf's admission: `need` = `smo_extents_for_parts(fold, parts)`
/// (+1 for a root leaf's new root), `already_promised` = what this leaf's
/// guard holds. `is_split = need > 1`; `claimable = free − promised`;
/// admitted iff `claimable − delta ≥ (is_split ? reserve : reserve/2)`.
pub fn admitLeaf(free: u64, promised: u64, reserve: u64, need: u64, already_promised: u64) AdmissionVerdict {
    const is_split = need > 1;
    const delta = need -| already_promised;
    if (delta == 0) return .{ .admitted = .{ .delta = 0, .is_split = is_split } };
    const floor = if (is_split) reserve else compactionFloorExtents(reserve);
    const claimable = free -| promised;
    if (claimable -| delta >= floor) return .{ .admitted = .{ .delta = delta, .is_split = is_split } };
    return .{ .no_space = .{ .free = claimable, .reserve = floor } };
}

/// Whether the SMALLEST split (two parts plus rounding and cascade extents,
/// `min_split = smo_extents_for_parts(fold_capacity + 1, 2)`) could be
/// promised above the reserve right now — the same arithmetic the admission
/// runs, so the heap-full posture and the refusals agree.
pub fn heapGrowthFloorClear(free: u64, promised: u64, min_split: u64, reserve: u64) bool {
    return free -| promised -| min_split >= reserve;
}

// ---------------------------------------------------------------------------
// The two classes of a non-advancing cycle: the counted, non-terminal SPACE
// standstill vs the WEDGE terminal.
// ---------------------------------------------------------------------------

/// What one barriered checkpoint cycle observed.
pub const CycleObservations = struct {
    pending_before: u64,
    pending_after: u64,
    tail_before: u64,
    tail: u64,
    /// Nodes the flush pass deferred for an exhausted extent GRANT.
    deferred_for_grant: u64,
    /// Nodes the flush pass deferred for `NoSpace`.
    deferred_for_space: u64,
    /// `heapGrowthFloorClear` at the end of the flush pass.
    growth_floor_clear: bool,
};

pub const CycleVerdict = enum {
    /// A retirement released or the tail advanced: the wedge rung resets.
    progress,
    /// Tail pinned by a node whose lessee is out of grant: the manager's
    /// refill owns the retry; the rung neither advances nor resets.
    manager_dependency,
    /// Tail pinned by a node that could not get an EXTENT: capacity, not
    /// corruption. Counted, never terminal, rung untouched.
    space_standstill,
    /// Nothing deferred, retirements parked, no release, no tail advance:
    /// the rung advanced and is still below the bound.
    wedge_rung,
    /// The rung reached `pending_free_force_cycles`: the volume latches
    /// FAILED (EIO until remount).
    wedge_failed,
};

pub const PostureTransition = enum { none, entered_heap_full, left_heap_full };

pub const CycleOutcome = struct {
    verdict: CycleVerdict,
    /// `meta_kv_heap_full` latch movement this cycle (loud once per
    /// transition).
    posture: PostureTransition,
    /// The wedge rung after this cycle (`pending_free_stalled_cycles`).
    stalled_cycles: u64,
};

/// The pure audit: the flush pass's heap-full latch plus the wedged-tail
/// rung. The wrapper counts and logs; nothing here has side effects.
pub const SpaceAudit = struct {
    stalled_cycles: u64 = 0,
    heap_full: bool = false,
    /// `meta_kv_heap_full_cycles`.
    heap_full_cycles: u64 = 0,
    failed: bool = false,

    pub fn tick(self: *SpaceAudit, obs: CycleObservations) CycleOutcome {
        // The flush-pass half: a pass that deferred for space is the space
        // class (counted per cycle, latched once); a pass that deferred
        // nothing clears the latch once the growth floor is clear again.
        var posture: PostureTransition = .none;
        if (obs.deferred_for_space > 0) {
            self.heap_full_cycles += 1;
            if (!self.heap_full) {
                self.heap_full = true;
                posture = .entered_heap_full;
            }
        } else if (obs.growth_floor_clear and self.heap_full) {
            self.heap_full = false;
            posture = .left_heap_full;
        }
        // The post-barrier audit.
        const verdict: CycleVerdict = blk: {
            if (obs.pending_after == 0 or obs.pending_after < obs.pending_before or obs.tail > obs.tail_before) {
                self.stalled_cycles = 0;
                break :blk .progress;
            }
            if (obs.deferred_for_grant > 0) break :blk .manager_dependency;
            if (obs.deferred_for_space > 0) break :blk .space_standstill;
            self.stalled_cycles += 1;
            if (self.stalled_cycles >= pending_free_force_cycles) {
                self.failed = true;
                break :blk .wedge_failed;
            }
            break :blk .wedge_rung;
        };
        return .{ .verdict = verdict, .posture = posture, .stalled_cycles = self.stalled_cycles };
    }
};

// ---------------------------------------------------------------------------
// Tests (PURE; the exhaustive models live in alloc_core_model.zig).
// ---------------------------------------------------------------------------

const testing = std.testing;

test {
    _ = @import("alloc_core_model.zig");
}

fn collect(list: *std.ArrayList(u64), extent: u64) void {
    list.append(testing.allocator, extent) catch @panic("oom");
}

fn drain(core: *const ExtCore, seq: u64) !std.ArrayList(u64) {
    var out: std.ArrayList(u64) = .empty;
    _ = core.advanceDurable(seq, &out, collect);
    return out;
}

fn ignoreRelease(_: void, _: u64) void {}

test "compaction reserve policy: max(8, total/50) pinned at 1, 100, 400, 450, 1000, 4 Mi; floor = reserve/2" {
    try testing.expectEqual(@as(u64, 8), compactionReserveExtents(1));
    try testing.expectEqual(@as(u64, 8), compactionReserveExtents(100));
    try testing.expectEqual(@as(u64, 8), compactionReserveExtents(400));
    try testing.expectEqual(@as(u64, 9), compactionReserveExtents(450));
    try testing.expectEqual(@as(u64, 20), compactionReserveExtents(1000));
    try testing.expectEqual(@as(u64, 83_886), compactionReserveExtents(4_194_304));
    try testing.expectEqual(@as(u64, 4), compactionFloorExtents(8));
    try testing.expectEqual(@as(u64, 41_943), compactionFloorExtents(83_886));
}

test "partition map: solo owns everything without a division; interleaved page ownership otherwise" {
    const solo = PartitionMap.solo(32576);
    try testing.expect(solo.isSolo());
    try testing.expectEqual(@as(u64, 0), solo.ownerOfExtent(1_000_000));
    const two = PartitionMap.new(2, 2);
    try testing.expectEqual(@as(u64, 0), two.ownerOfExtent(1));
    try testing.expectEqual(@as(u64, 1), two.ownerOfExtent(2));
    try testing.expectEqual(@as(u64, 0), two.ownerOfExtent(4));
    try testing.expectEqual(@as(u64, 2), partitionExtentCount(4, two, 0));
    try testing.expectEqual(@as(u64, 1), partitionExtentCount(3, two, 1));
    try testing.expectEqual(@as(u64, 2), firstExtentOf(two, 1));
}

test "core claims unique, reserve isolated: 16/4 — 12 unique user claims then NoSpace; 4 internal; exhausted refuses internal" {
    var core = try ExtCore.init(testing.allocator, 16, 4, 8);
    defer core.deinit();
    var seen: [16]bool = @splat(false);
    var i: usize = 0;
    while (i < 12) : (i += 1) {
        const e = try core.claim(.user);
        try testing.expect(!seen[@intCast(e)]);
        seen[@intCast(e)] = true;
    }
    try testing.expectError(error.NoSpace, core.claim(.user));
    try testing.expectEqual(@as(u64, 4), core.freeExtents());
    i = 0;
    while (i < 4) : (i += 1) {
        const e = try core.claim(.internal);
        try testing.expect(!seen[@intCast(e)]);
        seen[@intCast(e)] = true;
    }
    try testing.expectError(error.NoSpace, core.claim(.internal));
    try testing.expectEqual(@as(u64, 0), core.freeExtents());
    for (seen) |s| try testing.expect(s);
}

test "pending free gated until the covering tail: parked extents unclaimable at any pressure; advance(1) none, advance(2) exactly the first; repeat is a no-op" {
    var core = try ExtCore.init(testing.allocator, 2, 0, 4);
    defer core.deinit();
    const a = try core.claim(.user);
    const b = try core.claim(.user);
    try core.freePending(a, 2);
    try core.freePending(b, 3);
    try testing.expectError(error.NoSpace, core.claim(.internal));
    try testing.expectEqual(@as(u64, 2), core.pendingCount());
    var none = try drain(&core, 1);
    defer none.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 0), none.items.len);
    try testing.expectError(error.NoSpace, core.claim(.internal));
    var first = try drain(&core, 2);
    defer first.deinit(testing.allocator);
    try testing.expectEqualSlices(u64, &.{a}, first.items);
    try testing.expectEqual(@as(u64, 2), core.durableSeq());
    var again = try drain(&core, 2);
    defer again.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 0), again.items.len);
    try testing.expectEqual(a, try core.claim(.internal));
    try testing.expectEqual(@as(u64, 1), core.pendingCount());
    // A stale advance never moves the clock backwards.
    var stale = try drain(&core, 1);
    defer stale.deinit(testing.allocator);
    try testing.expectEqual(@as(u64, 2), core.durableSeq());
    try testing.expect(core.isAllocated(b));
}

test "pending free cap and lap recycling: cap 2 refuses the third, extent stays claimed; a drain recycles the slots" {
    var core = try ExtCore.init(testing.allocator, 6, 0, 2);
    defer core.deinit();
    const a = try core.claim(.user);
    const b = try core.claim(.user);
    const c = try core.claim(.user);
    try core.freePending(a, 1);
    try core.freePending(b, 2);
    try testing.expect(!core.pendingHasRoom());
    try testing.expectEqual(@as(u64, 0), core.pendingRoomIn(0));
    try testing.expectError(error.PendingFreeFull, core.freePending(c, 3));
    try testing.expect(core.isAllocated(c));
    try testing.expectEqual(@as(u64, 2), core.pendingCount());
    var d1 = try drain(&core, 2);
    defer d1.deinit(testing.allocator);
    try testing.expectEqualSlices(u64, &.{ a, b }, d1.items);
    // Second lap: positions 2 and 3 reuse slots 0 and 1.
    try core.freePending(c, 3);
    const d = try core.claim(.user);
    try core.freePending(d, 4);
    try testing.expectError(error.PendingFreeFull, core.freePending(a, 5));
    var d2 = try drain(&core, 4);
    defer d2.deinit(testing.allocator);
    try testing.expectEqualSlices(u64, &.{ c, d }, d2.items);
    try testing.expectEqual(@as(u64, 0), core.pendingCount());
}

test "forced retirement overflows at cap, drains on the same clock with the front-stop, and conserves extents" {
    var core = try ExtCore.init(testing.allocator, 3, 0, 2);
    defer core.deinit();
    const a = try core.claim(.user);
    const b = try core.claim(.user);
    const c = try core.claim(.user);
    try core.freePending(a, 3);
    try core.freePending(b, 5);
    try testing.expect(try core.freePendingForced(c, 7));
    try testing.expectEqual(@as(u64, 3), core.pendingCount());
    var d6 = try drain(&core, 6);
    defer d6.deinit(testing.allocator);
    try testing.expectEqualSlices(u64, &.{ a, b }, d6.items);
    try testing.expectEqual(@as(u64, 1), core.pendingCount());
    try testing.expect(core.isAllocated(c));
    var d8 = try drain(&core, 8);
    defer d8.deinit(testing.allocator);
    try testing.expectEqualSlices(u64, &.{c}, d8.items);
    try testing.expectEqual(@as(u64, 0), core.pendingCount());
    try testing.expectEqual(@as(u64, 3), core.freeExtents());
    // With FIFO room the forced path lands in the ring, never the overflow.
    const e = try core.claim(.user);
    try testing.expect(!try core.freePendingForced(e, 9));
}

test "RES-14: a budget the bitmap cannot honour refuses InvariantDrift after bounded rescans and hands the entitlement back" {
    var core = try ExtCore.init(testing.allocator, 2, 0, 2);
    defer core.deinit();
    _ = try core.claim(.user);
    _ = try core.claim(.user);
    core.inflateFreeBudgetForTest(1);
    try testing.expectError(error.InvariantDrift, core.claim(.user));
    try testing.expectEqual(@as(u64, 1), core.freeExtents());
}

test "partitioned: claims stay in the owner's pages, hints seed per partition, releases credit the owner" {
    var core = try ExtCore.initPartitioned(testing.allocator, 6, 0, 4, PartitionMap.new(2, 2));
    defer core.deinit();
    // Pages: 0 → {0,1} writer 0; 1 → {2,3} writer 1; 2 → {4,5} writer 0.
    try testing.expectEqual(@as(u64, 4), core.freeExtentsIn(0));
    try testing.expectEqual(@as(u64, 2), core.freeExtentsIn(1));
    try testing.expectEqual(@as(u64, 2), core.claimIn(1, .internal));
    try testing.expectEqual(@as(u64, 3), core.claimIn(1, .internal));
    try testing.expectError(error.NoSpace, core.claimIn(1, .internal));
    try testing.expectEqual(@as(u64, 0), core.claimIn(0, .internal));
    try testing.expectEqual(@as(u64, 1), core.claimIn(0, .internal));
    try testing.expectEqual(@as(u64, 4), core.claimIn(0, .internal));
    try testing.expectEqual(@as(u64, 5), core.claimIn(0, .internal));
    try testing.expectError(error.NoSpace, core.claimIn(0, .internal));
    // A release of writer 1's extent credits writer 1, whoever calls it.
    core.release(3);
    try testing.expectEqual(@as(u64, 1), core.freeExtentsIn(1));
    try testing.expectEqual(@as(u64, 0), core.freeExtentsIn(0));
    // The wrap: with hint past page 2, the scan wraps to page 0 then the
    // head of the hint's own page.
    core.release(0);
    core.release(5);
    try testing.expectEqual(@as(u64, 0), core.claimIn(0, .internal));
    try testing.expectEqual(@as(u64, 5), core.claimIn(0, .internal));
}

test "partitioned coverage clocks never cross: writer 0's tail releases only writer 0's parked extent" {
    var core = try ExtCore.initPartitioned(testing.allocator, 4, 0, 4, PartitionMap.new(2, 2));
    defer core.deinit();
    const mine = try core.claimIn(0, .internal);
    const theirs = try core.claimIn(1, .internal);
    try core.freePending(mine, 100);
    try core.freePending(theirs, 100);
    var r0: std.ArrayList(u64) = .empty;
    defer r0.deinit(testing.allocator);
    _ = core.advanceDurableIn(0, 100, &r0, collect);
    try testing.expectEqualSlices(u64, &.{mine}, r0.items);
    try testing.expect(core.isAllocated(theirs));
    try testing.expectEqual(@as(u64, 0), core.durableSeqIn(1));
    try testing.expectEqual(@as(u64, 1), core.pendingCountIn(1));
    try testing.expectEqual(@as(u64, 0), core.advanceDurableIn(1, 99, {}, ignoreRelease));
    try testing.expectEqual(@as(u64, 1), core.advanceDurableIn(1, 100, {}, ignoreRelease));
    try testing.expect(!core.isAllocated(theirs));
}

test "concurrent claims unique across a pending drain: 2048 extents, 32 parked, 8 claimers vs a drainer" {
    const total: u64 = 2048;
    var core = try ExtCore.init(testing.allocator, total, 0, 64);
    defer core.deinit();
    var i: u64 = 0;
    while (i < 32) : (i += 1) {
        const e = try core.claim(.internal);
        try core.freePending(e, i + 1);
    }
    const Shared = struct {
        core: *const ExtCore,
        owners: [total]Atomic(u32) = @splat(.init(0)),
        dups: Atomic(u64) = .init(0),
        claimed: Atomic(u64) = .init(0),
        drift: Atomic(u64) = .init(0),

        fn claimer(self: *@This(), id: u32) void {
            while (true) {
                const e = self.core.claim(.internal) catch |err| switch (err) {
                    error.NoSpace => {
                        // Parked extents drain concurrently; keep claiming
                        // until the drainer has released everything.
                        if (self.core.pendingCount() == 0) return;
                        std.atomic.spinLoopHint();
                        continue;
                    },
                    error.InvariantDrift => {
                        _ = self.drift.fetchAdd(1, .monotonic);
                        return;
                    },
                };
                if (self.owners[@intCast(e)].swap(id, .acq_rel) != 0) _ = self.dups.fetchAdd(1, .monotonic);
                _ = self.claimed.fetchAdd(1, .monotonic);
            }
        }

        fn drainer(self: *@This()) void {
            var seq: u64 = 1;
            while (seq <= 32) : (seq += 1) {
                _ = self.core.advanceDurable(seq, {}, ignoreRelease);
            }
        }
    };
    const shared = try testing.allocator.create(Shared);
    defer testing.allocator.destroy(shared);
    shared.* = .{ .core = &core };
    var threads: [9]std.Thread = undefined;
    threads[0] = try std.Thread.spawn(.{}, Shared.drainer, .{shared});
    for (threads[1..], 1..) |*t, id| t.* = try std.Thread.spawn(.{}, Shared.claimer, .{ shared, @as(u32, @intCast(id)) });
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u64, 0), shared.dups.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), shared.drift.load(.monotonic));
    try testing.expectEqual(total, shared.claimed.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), core.freeExtents());
    try testing.expectEqual(@as(u64, 0), core.pendingCount());
    for (&shared.owners) |*o| try testing.expect(o.load(.monotonic) != 0);
}

test "heap admission: splits need the whole reserve, compactions the floor; promises retract the delta; refusal names claimable and the floor" {
    // reserve 20 → floor 10; 25 free, 3 promised → claimable 22.
    const split = admitLeaf(25, 3, 20, 4, 0);
    try testing.expectEqual(@as(u64, 22), split.no_space.free);
    try testing.expectEqual(@as(u64, 20), split.no_space.reserve);
    const compaction = admitLeaf(25, 3, 20, 1, 0);
    try testing.expectEqual(@as(u64, 1), compaction.admitted.delta);
    try testing.expect(!compaction.admitted.is_split);
    // Exactly at the floor is admitted; one below refuses.
    try testing.expectEqual(@as(u64, 1), admitLeaf(14, 3, 20, 1, 0).admitted.delta);
    try testing.expectEqual(@as(u64, 10), admitLeaf(13, 3, 20, 1, 0).no_space.free);
    // A split with the whole reserve clear: 30 − 3 − 4 = 23 ≥ 20.
    try testing.expectEqual(@as(u64, 4), admitLeaf(30, 3, 20, 4, 0).admitted.delta);
    // Already promised 4 of 4: nothing more, the basis just moves.
    try testing.expectEqual(@as(u64, 0), admitLeaf(0, 0, 20, 4, 4).admitted.delta);
    // Growth floor: free − promised − min_split ≥ reserve.
    try testing.expect(heapGrowthFloorClear(25, 1, 4, 20));
    try testing.expect(!heapGrowthFloorClear(24, 1, 4, 20));
}

test "space audit: progress resets the rung; grant and space standstills never advance it; 8 wedged cycles fail" {
    var audit: SpaceAudit = .{};
    const base = CycleObservations{ .pending_before = 3, .pending_after = 3, .tail_before = 10, .tail = 10, .deferred_for_grant = 0, .deferred_for_space = 0, .growth_floor_clear = true };
    // Space standstill: counted, latched once, never terminal, rung untouched.
    var obs = base;
    obs.deferred_for_space = 2;
    var out = audit.tick(obs);
    try testing.expectEqual(CycleVerdict.space_standstill, out.verdict);
    try testing.expectEqual(PostureTransition.entered_heap_full, out.posture);
    try testing.expectEqual(@as(u64, 0), out.stalled_cycles);
    out = audit.tick(obs);
    try testing.expectEqual(PostureTransition.none, out.posture);
    try testing.expectEqual(@as(u64, 2), audit.heap_full_cycles);
    // A pass that deferred nothing with the floor clear leaves the posture.
    obs = base;
    obs.deferred_for_grant = 1;
    out = audit.tick(obs);
    try testing.expectEqual(CycleVerdict.manager_dependency, out.verdict);
    try testing.expectEqual(PostureTransition.left_heap_full, out.posture);
    // A latched posture stays while the floor is not clear.
    obs = base;
    obs.deferred_for_space = 1;
    _ = audit.tick(obs);
    obs = base;
    obs.growth_floor_clear = false;
    out = audit.tick(obs);
    try testing.expectEqual(PostureTransition.none, out.posture);
    try testing.expect(audit.heap_full);
    // The wedge: that tick was rung 1; seven more reach the terminal.
    try testing.expectEqual(CycleVerdict.wedge_rung, out.verdict);
    try testing.expectEqual(@as(u64, 1), out.stalled_cycles);
    var n: u64 = 2;
    while (n < pending_free_force_cycles) : (n += 1) {
        out = audit.tick(base);
        try testing.expectEqual(CycleVerdict.wedge_rung, out.verdict);
        try testing.expectEqual(n, out.stalled_cycles);
    }
    out = audit.tick(base);
    try testing.expectEqual(CycleVerdict.wedge_failed, out.verdict);
    try testing.expectEqual(pending_free_force_cycles, out.stalled_cycles);
    try testing.expect(audit.failed);
    // Progress by any of the three signals resets the rung.
    var fresh: SpaceAudit = .{};
    _ = fresh.tick(base);
    var tail_moved = base;
    tail_moved.tail = 11;
    try testing.expectEqual(CycleVerdict.progress, fresh.tick(tail_moved).verdict);
    try testing.expectEqual(@as(u64, 0), fresh.stalled_cycles);
    _ = fresh.tick(base);
    var released = base;
    released.pending_after = 2;
    try testing.expectEqual(CycleVerdict.progress, fresh.tick(released).verdict);
    var empty = base;
    empty.pending_after = 0;
    try testing.expectEqual(CycleVerdict.progress, fresh.tick(empty).verdict);
}
