//! The ENOSPC / promise law (charter §5.3 "Space": "a user commit PROMISES
//! its projected SMO extents at admission (`heap_promised`) and refuses
//! growth `ENOSPC` while compactions/deletes are admitted down to the
//! compaction floor `reserve/2`; a cycle that defers a node for NoSpace is
//! the counted non-terminal space class; the FAILED terminal is the wedge
//! class alone"; survey `docs/porting/kv_tree_cache_commit_survey.md` §2.13,
//! §3.5, §7 row 14). PURE over `alloc_core`'s arithmetic: no device, no
//! lock, no clock — the conveyor pass calls `admitHeapLocked` inside its
//! union leaf-lock window and hands it views of the locked leaves.
//!
//! WHY a user commit never claims an extent: its records land in leaf
//! overlays and the checkpoint's flush pass claims for the SMOs they force.
//! Before this law the reserve was consumed by ACKED user growth, the pass
//! ran out of heap mid-flush, the deferred nodes pinned the tail, and the
//! wedged-tail audit fail-stopped a FULL volume (the 2026-09-09 sweep's P1).
//! Every acked record must be FLUSHABLE: per leaf a member's records land
//! on, project the flush — fits in place (no SMO, nothing to promise) or
//! needs an SMO whose extents are PROMISED now against `claimable = free −
//! heap_promised`, at the whole reserve for a SPLIT (net growth) or at the
//! compaction floor `reserve/2` for a net-zero COMPACTION; a member that
//! cannot be promised refuses `NoSpace` ALONE, before anything is reserved
//! or applied.
//!
//! `heap_full` is a POSTURE, never terminal (§3.5): a full heap is a
//! capacity fact, not corruption — reads, deletes and in-place appends keep
//! committing, deletes' compactions are admitted at the floor and RETURN
//! extents (the geometric recovery wave 8 → 16 → 32 → 64 → 128 merges), so
//! latching FAILED would turn a recoverable state into EIO-until-remount.
//! Only the wedge class — parked retirements with no release and no tail
//! advance for `pending_free_force_cycles` barriered cycles — is terminal.
//! `HeapFullPosture` here is THE posture word (one per volume); the wedge
//! rung is `alloc_core.SpaceAudit`'s, and it keeps the space class out of
//! the rung without a posture of its own.

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../core/counters.zig");
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");
const alloc_core = @import("alloc_core.zig");
const alloc_ext = @import("alloc_ext.zig");
const record = @import("record.zig");
const node = @import("node.zig");
const tx = @import("tx.zig");

const assert = std.debug.assert;

pub const NodeLayout = node.NodeLayout;
pub const RecordKind = record.RecordKind;

pub const stats = struct {
    /// `meta_kv_enospc_refusals`: user commits refused `NoSpace` at heap
    /// admission (after the one retry).
    pub var meta_kv_enospc_refusals: counters.ShardedCounter = .{};
    /// `meta_kv_merge_yields`: opportunistic merges that stood down for a
    /// user commit's space retry.
    pub var meta_kv_merge_yields: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// The typed refusal and its errno.
// ---------------------------------------------------------------------------

/// The ENOSPC surface: `{free: claimable, reserve: floor}` at the refusal.
pub const NoSpace = struct {
    free: u64,
    reserve: u64,

    pub fn format(self: NoSpace, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("metadata heap full: {d} claimable extent(s) against a floor of {d}", .{ self.free, self.reserve });
    }
};

/// The heap law's one error class. Retryable exactly once behind the
/// space-retry cycles; a surviving refusal is terminal FOR THE OP (never
/// for the volume).
pub const HeapError = error{NoSpace};

/// `NoSpace` is the ONE table's `NoSpaceLeft` (ENOSPC).
pub fn coreError(e: HeapError) errors.Error {
    return switch (e) {
        error.NoSpace => error.NoSpaceLeft,
    };
}

// ---------------------------------------------------------------------------
// The promise ledgers: per volume (`heap_promised`) and per leaf.
// ---------------------------------------------------------------------------

/// `heap_promised`: Σ extents promised to pending SMOs on one volume — the
/// admission's `claimable − promised` term; 0 at quiesce (every promise is
/// consumed by its SMO, released at an in-place append, or retracted with
/// its refused member). Atomic because the SMO task releases while the
/// conveyor pass promises; `.acq_rel` on every RMW so a release is visible
/// to the next admission's load (the pairing is RMW ↔ `load(.acquire)`).
/// Mirrors `alloc_ext.meta_kv_heap_promised` by delta (Σ over volumes).
pub const HeapLedger = struct {
    own: counters.Gauge = .{},
    /// The volume's live ledger when bound (the node cache's
    /// `heap_promised` gauge, which the SMO task's releases move too);
    /// `own` stands in for the pure tests.
    gauge: ?*counters.Gauge = null,

    /// Bound to the volume's gauge: `load` reads what every promise and
    /// release on the volume moved.
    pub fn bound(gauge: *counters.Gauge) HeapLedger {
        return .{ .gauge = gauge };
    }

    pub fn load(self: *const HeapLedger) u64 {
        return (self.gauge orelse &self.own).load();
    }

    fn add(self: *HeapLedger, n: u64) void {
        if (n == 0) return;
        (self.gauge orelse &self.own).add(n);
        alloc_ext.meta_kv_heap_promised.add(n);
    }

    fn sub(self: *HeapLedger, n: u64) void {
        if (n == 0) return;
        const g = self.gauge orelse &self.own;
        assert(g.load() >= n);
        g.sub(n);
        alloc_ext.meta_kv_heap_promised.sub(n);
    }
};

/// One leaf's promise, kept under the leaf's write lock (the node cache's
/// dirty half embeds it): extents PROMISED to the leaf's next SMO (0 = the
/// pending bytes append in place), the fold estimate the promise was
/// computed for at the last exact walk, and the bytes admitted since —
/// the cheap re-check that skips the O(node) walk per commit on a hot
/// leaf between two flushes.
pub const LeafPromise = struct {
    promised: u64 = 0,
    basis: usize = 0,
    added: usize = 0,

    /// Promise `extents` more for a fold of `basis` bytes (the caller
    /// checked the budget against the ledger under the leaf's lock).
    pub fn promise(self: *LeafPromise, ledger: *HeapLedger, extents: u64, basis: usize) void {
        self.promised += extents;
        self.basis = basis;
        self.added = 0;
        ledger.add(extents);
    }

    /// A member admitted under the existing promise (inside the growth
    /// window): its bytes count toward the next re-walk.
    pub fn noteGrowth(self: *LeafPromise, bytes: usize) void {
        self.added += bytes;
    }

    /// Re-base on a fresh exact walk without changing the promise.
    pub fn setBasis(self: *LeafPromise, basis: usize) void {
        self.basis = basis;
        self.added = 0;
    }

    /// Return `extents` of a promise made for a member that was then
    /// REFUSED (a later leaf of the same member did not clear the floor —
    /// §4.7 P1, Issue 17): the member's records never land, so the SMO they
    /// projected is not owed. WHY it matters: a promise on a leaf with
    /// nothing pending is never consumed — no flush has an SMO to run there
    /// — and stands as a permanent claimable deficit (`heap_promised = 1`
    /// for 32 cycles was the mid-wave ENOSPC fixpoint: `claimable = free −
    /// 1 = floor`, and neither a delete's compaction nor a sweep's merge
    /// could be admitted again). The basis stays (an over-estimate only
    /// triggers an earlier re-walk); a promise back at 0 clears it.
    pub fn retract(self: *LeafPromise, ledger: *HeapLedger, extents: u64) void {
        const take = @min(extents, self.promised);
        self.promised -= take;
        ledger.sub(take);
        if (self.promised == 0) {
            self.basis = 0;
            self.added = 0;
        }
    }

    /// Undo `noteGrowth` for a refused member's bytes (never applied).
    pub fn retractGrowth(self: *LeafPromise, bytes: usize) void {
        self.added -|= bytes;
    }

    /// Return the whole promise: the SMO consumed it (its claims drew the
    /// budget the promise held), an in-place append left the remainder
    /// fitting, or the leaf object dies (Drop-owned — a dying leaf never
    /// strands ledger budget).
    pub fn release(self: *LeafPromise, ledger: *HeapLedger) void {
        ledger.sub(self.promised);
        self.* = .{};
    }
};

// ---------------------------------------------------------------------------
// The heap-full posture word.
// ---------------------------------------------------------------------------

/// What the posture's log lines report at a transition.
pub const SpaceSnapshot = struct {
    free: u64,
    promised: u64,
    reserve: u64,
};

/// The §4.7 `meta_kv_heap_full` posture word (0/1 per volume): growth
/// needing a new leaf is refused `ENOSPC` while set; reads, deletes and
/// in-place appends keep committing. Loud ONCE per transition. Three sites
/// enter it (a refused user commit, a cycle whose flush pass deferred for
/// space, a threshold maintenance refusal); two leave it (a cycle that
/// deferred nothing with the growth floor clear, an admitted split). The
/// `.acq_rel` swaps order the posture against the admission that caused
/// the transition.
pub const HeapFullPosture = struct {
    full: std.atomic.Value(bool) = .init(false),

    pub fn isFull(self: *const HeapFullPosture) bool {
        return self.full.load(.acquire);
    }

    /// Latch; `true` iff this call made the transition.
    pub fn enter(self: *HeapFullPosture, volume: []const u8, why: []const u8, snap: SpaceSnapshot) bool {
        if (self.full.swap(true, .acq_rel)) return false;
        alloc_ext.meta_kv_heap_full.set(1);
        log.warn("meta volume {s}: metadata heap FULL — {s} (free={d} claimable, {d} promised to pending SMOs, reserve={d}); growth needing a new leaf answers ENOSPC, reads and deletes continue, the volume is NOT failed (§4.7 space standstill class)", .{ volume, why, snap.free, snap.promised, snap.reserve });
        return true;
    }

    /// Clear; `true` iff this call made the transition.
    pub fn leave(self: *HeapFullPosture, volume: []const u8, why: []const u8, snap: SpaceSnapshot) bool {
        if (!self.full.swap(false, .acq_rel)) return false;
        alloc_ext.meta_kv_heap_full.set(0);
        log.info("meta volume {s}: metadata heap no longer full — {s} (free={d} claimable, {d} promised, reserve={d})", .{ volume, why, snap.free, snap.promised, snap.reserve });
        return true;
    }

    /// The checkpoint cycle's half: a flush pass that deferred
    /// `deferred_for_space` nodes is the space class (`meta_kv_heap_full_
    /// cycles` per cycle, latched once); a pass that deferred nothing clears
    /// the latch once the growth floor is clear again — a full volume whose
    /// deletes only free record space stays latched (honest: no new leaf
    /// can be minted). The wedge rung is `alloc_core.SpaceAudit`'s.
    pub fn cycleFlushPass(self: *HeapFullPosture, volume: []const u8, deferred_for_space: u64, growth_floor_clear: bool, snap: SpaceSnapshot) void {
        if (deferred_for_space > 0) {
            alloc_ext.meta_kv_heap_full_cycles.inc();
            _ = self.enter(volume, "the flush pass deferred node(s) whose SMO could not claim an extent", snap);
        } else if (growth_floor_clear) {
            _ = self.leave(volume, "a flush pass deferred nothing and the growth floor is clear", snap);
        }
    }
};

/// Whether the growth floor is clear right now: the SMALLEST split (a fold
/// one byte over capacity — two parts, plus the rounding and cascade
/// extents: 4) could be promised above the reserve — the same arithmetic
/// the admission runs, so the posture word and the refusals agree.
pub fn heapGrowthFloorClear(layout: NodeLayout, free: u64, promised: u64, reserve: u64) bool {
    return alloc_core.heapGrowthFloorClear(free, promised, minSplitExtents(layout), reserve);
}

/// `smo_extents_for_parts(fold_capacity + 1, 2)`: two parts + 2.
pub fn minSplitExtents(layout: NodeLayout) u64 {
    return layout.smoExtentsForParts(layout.foldCapacity() + 1, 2);
}

// ---------------------------------------------------------------------------
// admitHeapLocked.
// ---------------------------------------------------------------------------

/// What the admission sees of ONE locked leaf — the node write guard's
/// view, with the leaf's promise ledger borrowed for the lock window.
pub const LeafView = struct {
    addr: u64,
    /// Node-relative unwritten-tail offset.
    tail_offset: usize,
    /// On-disk length of the in-flight frozen frame (0 = none).
    frozen_frame_len: usize,
    /// Encoded bytes in the open delta.
    overlay_bytes: usize,
    /// A root leaf's split also mints a new root (+1 extent).
    is_root: bool,
    promise: *LeafPromise,

    /// Where the leaf's log would end after appending everything pending
    /// plus `extra` encoded record bytes: the frozen frame (if any) and ONE
    /// append frame over the open delta + `extra`. `> node_size` ⇒ the
    /// flush needs an SMO. Conservative: a frozen and an open delta cost
    /// two frames' headers where one flush might merge them.
    pub fn projectedLogEnd(self: LeafView, layout: NodeLayout, extra: usize) usize {
        return self.tail_offset + self.frozen_frame_len + layout.appendFrameLen(self.overlay_bytes + extra);
    }
};

/// One batch member for the admission: its records and, per record, the
/// index of its resolved leaf in the union `lock_set`.
pub const Member = struct {
    recs: []const tx.Rec,
    leaf_of: []const usize,
};

/// A record about to land on a leaf — the exact walk's pending input.
pub const PendingRecord = struct {
    key: []const u8,
    kind: RecordKind,
    /// `record.RecordRef.encodedLen`: header + key + value.
    encoded_len: usize,
};

/// `(fold bytes, greedy ¾-fill parts)` — what the leaf snapshot's
/// `fold_bytes_upper_with(pending, split_part_capacity, tail 0)` answers.
/// Tail 0: the admission never credits tombstone elision (the conservative
/// posture the law landed with).
pub const FoldUpper = struct { bytes: usize, parts: usize };

pub const Refused = struct {
    member: usize,
    why: NoSpace,
};

/// Decision 0003 §BD: the lock-free admission's room — a volume with at
/// least twice the compaction reserve plus `SPLIT_EXTENTS_PER_TX` extents
/// per transaction the conveyor can hold in flight (`caps.txs`): every leaf
/// an in-flight transaction could push past its node splits at writeback
/// from extents that exist, so no promise is owed and no lock is taken;
/// near the reserve every admission takes the locked law.
pub fn fastPathRoom(free_extents: u64, reserve_extents: u64, txs_in_flight_cap: u64) bool {
    return free_extents >= 2 * reserve_extents + SPLIT_EXTENTS_PER_TX * txs_in_flight_cap;
}

/// A transaction's records touch two leaves (a create: the inode's and
/// the dentry's) and a leaf's split takes two extents.
pub const SPLIT_EXTENTS_PER_TX: u64 = 4;

/// The encoded (bset) length of a staged record.
pub fn encodedLen(r: tx.Rec) usize {
    return record.RECORD_HEADER_LEN + r.key.len + r.value.len;
}

/// Under the union locks before the reservation. `budget` answers
/// `freeExtents()` / `reserveExtents()` (the extent allocator); `walker`
/// answers `foldUpper(leaf_index, pending) FoldUpper` (the leaf snapshots'
/// exact walk). Appends the members refused `NoSpace` to `refused`,
/// ascending — side-effect-free on the refusal counters (the caller
/// finalizes: a refusal may earn one retry behind the space-retry cycles);
/// promises granted to survivors stay on their leaves across the retry
/// (the re-projection finds them already covered).
///
/// FAST PATH (every pass on a volume with room): per leaf the WHOLE
/// batch's bytes projected at once — a leaf that absorbs them in place
/// needs no per-member accounting; stack-buffered for the usual union
/// widths so the hot path gains no allocation per batch. Else per member,
/// per leaf in record order: in place ⇒ nothing; already promised and
/// within the growth window ⇒ `noteGrowth`; else the exact walk, `need =
/// smo_extents_for_parts(fold, parts)` (+1 for a root leaf's split),
/// `alloc_core.admitLeaf` — admitted ⇒ promise, refused ⇒ the member's
/// earlier promises are retracted. An admitted split leaves `heap_full`.
pub fn admitHeapLocked(
    gpa: Allocator,
    layout: NodeLayout,
    budget: anytype,
    walker: anytype,
    ledger: *HeapLedger,
    posture: *HeapFullPosture,
    volume: []const u8,
    members: []const Member,
    lock_set: []const LeafView,
    refused: *std.ArrayList(Refused),
) Allocator.Error!void {
    const node_size = layout.nodeSize();
    // FAST PATH.
    var totals_buf: [32]usize = @splat(0);
    const totals: []usize = if (lock_set.len <= totals_buf.len) totals_buf[0..lock_set.len] else blk: {
        const t = try gpa.alloc(usize, lock_set.len);
        @memset(t, 0);
        break :blk t;
    };
    defer if (lock_set.len > totals_buf.len) gpa.free(totals);
    for (members) |m| {
        assert(m.recs.len == m.leaf_of.len);
        for (m.recs, m.leaf_of) |r, gi| totals[gi] += encodedLen(r);
    }
    var any_overflow = false;
    for (totals, 0..) |t, gi| {
        if (t > 0 and lock_set[gi].projectedLogEnd(layout, t) > node_size) any_overflow = true;
    }
    if (!any_overflow) return;

    const reserve = budget.reserveExtents();
    // Per locked leaf: bytes this batch's ADMITTED members land on it.
    const pending = try gpa.alloc(usize, lock_set.len);
    defer gpa.free(pending);
    @memset(pending, 0);
    const LeafBytes = struct { gi: usize, bytes: usize };
    const Taken = struct { gi: usize, extents: u64, grew: usize };
    var member_leaves: std.ArrayList(LeafBytes) = .empty;
    defer member_leaves.deinit(gpa);
    var taken: std.ArrayList(Taken) = .empty;
    defer taken.deinit(gpa);
    var extra: std.ArrayList(PendingRecord) = .empty;
    defer extra.deinit(gpa);

    for (members, 0..) |m, qi| {
        member_leaves.clearRetainingCapacity();
        taken.clearRetainingCapacity();
        for (m.recs, m.leaf_of) |r, gi| {
            const bytes = encodedLen(r);
            for (member_leaves.items) |*lb| {
                if (lb.gi == gi) {
                    lb.bytes += bytes;
                    break;
                }
            } else try member_leaves.append(gpa, .{ .gi = gi, .bytes = bytes });
        }
        var verdict: ?NoSpace = null;
        var admitted_split = false;
        for (member_leaves.items) |lb| {
            const gi = lb.gi;
            const leaf = lock_set[gi];
            if (leaf.projectedLogEnd(layout, pending[gi] + lb.bytes) <= node_size) continue; // in-place append at the flush
            const p = leaf.promise;
            // Cheap re-check on an already-promised leaf — never the O(node)
            // walk per commit on a hot leaf between two flushes: a SPLIT
            // promise carries a cascade extent, so growth below the layout's
            // window since the last exact walk cannot need more; a
            // COMPACTION promise (1) covers growth while the walk's fold plus
            // every byte since stays within one node (no shadowing
            // credited — an upper bound).
            if (p.promised > 0) {
                const added = p.added + lb.bytes;
                const bound = p.basis + added;
                const covered = if (p.promised == 1) bound <= layout.foldCapacity() else added < layout.splitGrowthWindow();
                if (covered) {
                    p.noteGrowth(lb.bytes);
                    try taken.append(gpa, .{ .gi = gi, .extents = 0, .grew = lb.bytes });
                    continue;
                }
            }
            try pendingRecords(gpa, members, qi, gi, refused.items, &extra);
            const fold = walker.foldUpper(gi, extra.items);
            var need = layout.smoExtentsForParts(fold.bytes, fold.parts);
            if (need > 1 and leaf.is_root) need += 1;
            switch (alloc_core.admitLeaf(budget.freeExtents(), ledger.load(), reserve, need, p.promised)) {
                .admitted => |a| {
                    if (a.delta == 0) {
                        // Covered by an earlier member/attempt: the exact
                        // estimate becomes the new basis.
                        p.setBasis(fold.bytes);
                        continue;
                    }
                    p.promise(ledger, a.delta, fold.bytes);
                    try taken.append(gpa, .{ .gi = gi, .extents = a.delta, .grew = 0 });
                    admitted_split = admitted_split or a.is_split;
                },
                .no_space => |ns| {
                    log.debug("heap admission refused: leaf {x} projects a fold of {d} B (capacity {d}) needing {d} extent(s), {d} promised; claimable {d} < floor {d} + {d}", .{ leaf.addr, fold.bytes, layout.foldCapacity(), need, p.promised, ns.free, ns.reserve, need - p.promised });
                    verdict = .{ .free = ns.free, .reserve = ns.reserve };
                    break;
                },
            }
        }
        if (verdict) |why| {
            // The member never lands: give back every promise its earlier
            // leaves drew and every growth they noted.
            for (taken.items) |t| {
                if (t.extents > 0) lock_set[t.gi].promise.retract(ledger, t.extents);
                if (t.grew > 0) lock_set[t.gi].promise.retractGrowth(t.grew);
            }
            try refused.append(gpa, .{ .member = qi, .why = why });
        } else {
            for (member_leaves.items) |lb| pending[lb.gi] += lb.bytes;
            if (admitted_split) {
                // A leaf was minted-in-budget for a member that LANDS: the
                // growth floor is clear.
                _ = posture.leave(volume, "a growth commit was admitted a new leaf", .{ .free = budget.freeExtents(), .promised = ledger.load(), .reserve = reserve });
            }
        }
    }
}

/// The batch's records landing on leaf `gi`, members `0..=upto`, in apply
/// order (refused members were never applied and are skipped).
fn pendingRecords(gpa: Allocator, members: []const Member, upto: usize, gi: usize, refused: []const Refused, out: *std.ArrayList(PendingRecord)) Allocator.Error!void {
    out.clearRetainingCapacity();
    for (members[0 .. upto + 1], 0..) |m, qi| {
        var skip = false;
        for (refused) |r| {
            if (r.member == qi) skip = true;
        }
        if (skip) continue;
        for (m.recs, m.leaf_of) |r, g| {
            if (g != gi) continue;
            try out.append(gpa, .{ .key = r.key, .kind = r.kind, .encoded_len = encodedLen(r) });
        }
    }
}

// ---------------------------------------------------------------------------
// The space retry: ONE retry behind the cycles that return budget.
// ---------------------------------------------------------------------------

/// Barriered cycles the retry runs before re-admitting. WHY two:
/// reclamation lags one cycle by design (§4.6 pt 2) — the first cycle's
/// SMOs park their old extents at gates past the tail that cycle covers,
/// the second cycle's tail passes them. The loop stops early once nothing
/// is pending.
pub const SPACE_RETRY_CYCLES: u32 = 2;

pub const SpaceRetryObs = struct {
    /// Whether this pass already spent its one retry.
    cycled_for_space: bool,
    /// `heap_promised` right now.
    heap_promised: u64,
    /// Retirements parked on the tail right now.
    pending_free: u64,
    /// How long the pass has been parked in space retries.
    elapsed_ns: u64,
    /// The volume's commit watchdog threshold (the committer's D1.b park
    /// bound, 30 s shipped) — a retry still parked past it is answered
    /// rather than wedging the committer behind a cycle that will not come.
    wall_bound_ns: u64,
};

pub const SpaceRetryDecision = enum {
    /// Begin the hold, run the return-room cycles, re-admit.
    retry_after_cycles,
    /// Fan the refusals out: `enospc_refusals++`, `enter_heap_full`.
    final_refusal,
};

/// Budget the checkpoint can RETURN — promises held by leaves the flush
/// pass has not reached yet (turned into claims at their SMO or released at
/// their append) and retirements parked on the tail — earns the refused
/// members ONE retry behind the cycles that return it: on a full volume a
/// burst of deletes would otherwise trip over its own outstanding
/// compaction promises between two cadence ticks. A refusal that survives,
/// finds nothing to return, or has parked past the wall bound is final.
pub fn decideSpaceRetry(obs: SpaceRetryObs) SpaceRetryDecision {
    if (obs.cycled_for_space) return .final_refusal;
    if (obs.elapsed_ns >= obs.wall_bound_ns) return .final_refusal;
    const returnable = obs.heap_promised > 0 or obs.pending_free > 0;
    return if (returnable) .retry_after_cycles else .final_refusal;
}

/// Run another return-room cycle? Stops at `SPACE_RETRY_CYCLES` or as soon
/// as nothing is pending (the second cycle's only job was to pass the
/// first's gates).
pub fn continueSpaceRetryCycles(cycles_run: u32, pending_free: u64) bool {
    return cycles_run < SPACE_RETRY_CYCLES and pending_free > 0;
}

/// The hold the retry takes from the first return-room cycle to the
/// re-admission that follows them: while one is held the opportunistic
/// merges YIELD (`inflight()`). WHY: the heap-full sweep claims at the same
/// compaction floor the refused member needs and runs at the end of every
/// cycle, after the barrier — without the hold it re-claimed every extent
/// the two cycles returned and the retry met the floor again while
/// `pending_free` grew (a full volume refusing its deletes while its
/// recovery churned).
pub const SpaceRetryHold = struct {
    retries: std.atomic.Value(u32) = .init(0),

    /// `.acq_rel` pairs with `inflight`'s acquire load: a sweep that reads
    /// the count after a begin sees it.
    pub fn begin(self: *SpaceRetryHold) void {
        _ = self.retries.fetchAdd(1, .acq_rel);
    }

    pub fn end(self: *SpaceRetryHold) void {
        const prev = self.retries.fetchSub(1, .acq_rel);
        assert(prev > 0);
    }

    /// The opportunistic merges' yield predicate.
    pub fn inflight(self: *const SpaceRetryHold) bool {
        return self.retries.load(.acquire) > 0;
    }

    /// One opportunistic merge stood down for a space retry.
    pub fn noteMergeYield() void {
        stats.meta_kv_merge_yields.inc();
    }
};

/// Finalize a surviving refusal: count it and latch the posture. Answers
/// the typed error the committer surfaces as `ENOSPC`.
pub fn finalizeRefusal(posture: *HeapFullPosture, volume: []const u8, why: NoSpace, promised: u64) HeapError {
    stats.meta_kv_enospc_refusals.inc();
    _ = posture.enter(volume, "a user commit was refused NoSpace", .{ .free = why.free, .promised = promised, .reserve = why.reserve });
    return error.NoSpace;
}

// ---------------------------------------------------------------------------
// Tests — the heap-law cases of `kv_leaf_merge_tests.rs` as pure cases.
// ---------------------------------------------------------------------------

const testing = std.testing;
const key = @import("key.zig");

const layout64 = NodeLayout.new(64 * 1024, null) catch unreachable;

/// A fake extent budget: the counts the allocator would answer.
const Budget = struct {
    free: u64,
    reserve: u64,

    fn freeExtents(self: *const Budget) u64 {
        return self.free;
    }
    fn reserveExtents(self: *const Budget) u64 {
        return self.reserve;
    }
};

/// A fake exact walk: each leaf's resident fold plus every pending byte
/// (no shadowing credited — the upper bound), packed greedily into ¾-fill
/// parts by bytes.
const Walker = struct {
    resident: []const usize,
    calls: u32 = 0,

    fn foldUpper(self: *Walker, leaf: usize, pending: []const PendingRecord) FoldUpper {
        self.calls += 1;
        var bytes = self.resident[leaf];
        for (pending) |p| bytes += p.encoded_len;
        const cap = layout64.splitPartCapacity();
        return .{ .bytes = bytes, .parts = @max(1, (bytes + cap - 1) / cap) };
    }
};

fn rec(k: []const u8, value_len: usize) tx.Rec {
    return .{ .tree_id = key.TREE_INODES, .key = k, .kind = .put, .value = zeros[0..value_len] };
}

const zeros: [65536]u8 = @splat(0);

/// A leaf whose log has `pages` free 4 KiB pages: an append frame of up to
/// `pages × 4096 − 64` record bytes fits in place; more needs an SMO.
/// Frames are page-granular, so "one record from overflow" is `pages = 0`
/// — the log ends at `node_size` and ANY append projects past it.
fn leafWithPages(addr: u64, pages: usize, is_root: bool, p: *LeafPromise) LeafView {
    return .{
        .addr = addr,
        .tail_offset = layout64.nodeSize() - pages * node.NODE_PAGE,
        .frozen_frame_len = 0,
        .overlay_bytes = 0,
        .is_root = is_root,
        .promise = p,
    };
}

fn leafFull(addr: u64, is_root: bool, p: *LeafPromise) LeafView {
    return leafWithPages(addr, 0, is_root, p);
}

fn admit(budget: *const Budget, walker: *Walker, ledger: *HeapLedger, posture: *HeapFullPosture, members: []const Member, lock_set: []const LeafView, refused: *std.ArrayList(Refused)) !void {
    try admitHeapLocked(testing.allocator, layout64, budget, walker, ledger, posture, "test-vol", members, lock_set, refused);
}

test "fast path: a batch every leaf absorbs in place promises nothing, walks nothing, allocates nothing" {
    var p0: LeafPromise = .{};
    var p1: LeafPromise = .{};
    const leaves = [_]LeafView{ leafWithPages(0x1000, 1, false, &p0), leafWithPages(0x2000, 1, true, &p1) };
    const k = key.inodeKey(5);
    const recs = [_]tx.Rec{ rec(&k, 100), rec(&k, 100), rec(&k, 100) };
    const members = [_]Member{
        .{ .recs = recs[0..2], .leaf_of = &.{ 0, 1 } },
        .{ .recs = recs[2..3], .leaf_of = &.{1} },
    };
    const budget: Budget = .{ .free = 0, .reserve = 8 };
    var walker: Walker = .{ .resident = &.{ 0, 0 } };
    var ledger: HeapLedger = .{};
    var posture: HeapFullPosture = .{};
    var refused: std.ArrayList(Refused) = .empty;
    defer refused.deinit(testing.allocator);
    try admitHeapLocked(testing.failing_allocator, layout64, &budget, &walker, &ledger, &posture, "test-vol", &members, &leaves, &refused);
    try testing.expectEqual(@as(usize, 0), refused.items.len);
    try testing.expectEqual(@as(u32, 0), walker.calls);
    try testing.expectEqual(@as(u64, 0), ledger.load());
    try testing.expectEqual(@as(u64, 0), p0.promised + p1.promised);
}

test "a refused multi-leaf member strands no heap promise (§4.7 P1, Issue 17): I is promised, D is refused at the floor, the member is refused whole, heap_promised returns to 0, and a single-leaf compaction is still admissible" {
    const reserve: u64 = 8;
    const floor = alloc_core.compactionFloorExtents(reserve);
    try testing.expectEqual(@as(u64, 4), floor);
    // Leaves I (the tail inode leaf) and D (the dentry leaf), each one
    // record from overflow; folds small so the SMO is a COMPACTION (need 1).
    var pi: LeafPromise = .{};
    var pd: LeafPromise = .{};
    const leaves = [_]LeafView{ leafFull(0x1000, false, &pi), leafFull(0x2000, false, &pd) };
    const ik = key.inodeKey(800);
    const dk = key.dentryKey(1, 0x1234, 0);
    const inode_put = tx.Rec{ .tree_id = key.TREE_INODES, .key = &ik, .kind = .put, .value = zeros[0..record.INODE_VALUE_LEN] };
    const dentry_put = tx.Rec{ .tree_id = key.TREE_DENTRIES, .key = &dk, .kind = .put, .value = zeros[0..200] };
    // `create` stages the inode Put first: I then D.
    const create = [_]tx.Rec{ inode_put, dentry_put };
    const members = [_]Member{.{ .recs = &create, .leaf_of = &.{ 0, 1 } }};
    // Exactly one claimable extent above the compaction floor.
    const budget: Budget = .{ .free = floor + 1, .reserve = reserve };
    var walker: Walker = .{ .resident = &.{ 20_000, 20_000 } };
    var ledger: HeapLedger = .{};
    var posture: HeapFullPosture = .{};
    var refused: std.ArrayList(Refused) = .empty;
    defer refused.deinit(testing.allocator);

    try admit(&budget, &walker, &ledger, &posture, &members, &leaves, &refused);
    try testing.expectEqual(@as(usize, 1), refused.items.len);
    try testing.expectEqual(@as(usize, 0), refused.items[0].member);
    // D's refusal saw I's promise: claimable = floor + 1 − 1 = floor; floor − 1 < floor.
    try testing.expectEqual(floor, refused.items[0].why.free);
    try testing.expectEqual(floor, refused.items[0].why.reserve);
    // The law: the refused member's promise on I is returned to the ledger.
    try testing.expectEqual(@as(u64, 0), ledger.load());
    try testing.expectEqual(@as(u64, 0), pi.promised);
    try testing.expectEqual(@as(usize, 0), pi.basis);
    try testing.expectEqual(@as(u64, 0), pd.promised);
    try testing.expectEqual(@as(u32, 2), walker.calls);

    // The consequence the law protects: with the one extent still
    // claimable, a single-leaf compaction (I's next inode shadow) is
    // admitted at floor + 1, exactly as before the refusal.
    refused.clearRetainingCapacity();
    const shadow = [_]tx.Rec{inode_put};
    const single = [_]Member{.{ .recs = &shadow, .leaf_of = &.{0} }};
    try admit(&budget, &walker, &ledger, &posture, &single, &leaves, &refused);
    try testing.expectEqual(@as(usize, 0), refused.items.len);
    try testing.expectEqual(@as(u64, 1), ledger.load());
    try testing.expectEqual(@as(u64, 1), pi.promised);
    // …and the SMO consuming it brings the ledger back to 0 (quiesce law).
    pi.release(&ledger);
    try testing.expectEqual(@as(u64, 0), ledger.load());
}

test "a compaction is admitted at the compaction floor + 1 and refused at the floor; a split needs the whole reserve; a root leaf's split mints a new root (+1)" {
    const reserve: u64 = 8;
    const floor = alloc_core.compactionFloorExtents(reserve);
    var p: LeafPromise = .{};
    const k = key.inodeKey(1);
    const one = [_]tx.Rec{rec(&k, 200)};
    const members = [_]Member{.{ .recs = &one, .leaf_of = &.{0} }};
    var ledger: HeapLedger = .{};
    var posture: HeapFullPosture = .{};
    var refused: std.ArrayList(Refused) = .empty;
    defer refused.deinit(testing.allocator);

    // ---- Compaction (fold ≤ C): refused one below the floor, nothing changes.
    {
        const leaves = [_]LeafView{leafFull(0x1000, false, &p)};
        var walker: Walker = .{ .resident = &.{10_000} };
        const at_floor: Budget = .{ .free = floor, .reserve = reserve };
        try admit(&at_floor, &walker, &ledger, &posture, &members, &leaves, &refused);
        try testing.expectEqual(@as(usize, 1), refused.items.len);
        try testing.expectEqual(NoSpace{ .free = floor, .reserve = floor }, refused.items[0].why);
        try testing.expectEqual(@as(u64, 0), ledger.load());
        refused.clearRetainingCapacity();
        // One extent returned: admitted at the floor + 1.
        const above: Budget = .{ .free = floor + 1, .reserve = reserve };
        try admit(&above, &walker, &ledger, &posture, &members, &leaves, &refused);
        try testing.expectEqual(@as(usize, 0), refused.items.len);
        try testing.expectEqual(@as(u64, 1), p.promised);
        try testing.expectEqual(@as(u64, 1), ledger.load());
        p.release(&ledger);
    }
    // ---- Split (fold > C): two parts + 2 = 4 extents, at the WHOLE reserve.
    {
        const leaves = [_]LeafView{leafFull(0x1000, false, &p)};
        var walker: Walker = .{ .resident = &.{layout64.foldCapacity()} };
        const need: u64 = 4;
        try testing.expectEqual(need, minSplitExtents(layout64));
        const short: Budget = .{ .free = reserve + need - 1, .reserve = reserve };
        try admit(&short, &walker, &ledger, &posture, &members, &leaves, &refused);
        try testing.expectEqual(@as(usize, 1), refused.items.len);
        try testing.expectEqual(reserve, refused.items[0].why.reserve);
        refused.clearRetainingCapacity();
        const enough: Budget = .{ .free = reserve + need, .reserve = reserve };
        try admit(&enough, &walker, &ledger, &posture, &members, &leaves, &refused);
        try testing.expectEqual(@as(usize, 0), refused.items.len);
        try testing.expectEqual(need, p.promised);
        try testing.expectEqual(layout64.foldCapacity() + encodedLen(one[0]), p.basis);
        p.release(&ledger);
    }
    // ---- A ROOT leaf's split: +1 for the new root.
    {
        const leaves = [_]LeafView{leafFull(0x1000, true, &p)};
        var walker: Walker = .{ .resident = &.{layout64.foldCapacity()} };
        const need: u64 = 5;
        const short: Budget = .{ .free = reserve + need - 1, .reserve = reserve };
        try admit(&short, &walker, &ledger, &posture, &members, &leaves, &refused);
        try testing.expectEqual(@as(usize, 1), refused.items.len);
        refused.clearRetainingCapacity();
        const enough: Budget = .{ .free = reserve + need, .reserve = reserve };
        try admit(&enough, &walker, &ledger, &posture, &members, &leaves, &refused);
        try testing.expectEqual(@as(usize, 0), refused.items.len);
        try testing.expectEqual(need, p.promised);
        p.release(&ledger);
        try testing.expectEqual(@as(u64, 0), ledger.load());
    }
}

test "an admitted split leaves the heap-full posture; a promised leaf absorbs growth inside its window without a re-walk and re-walks past it" {
    const reserve: u64 = 8;
    var p: LeafPromise = .{};
    const k = key.inodeKey(1);
    const one = [_]tx.Rec{rec(&k, 200)};
    const members = [_]Member{.{ .recs = &one, .leaf_of = &.{0} }};
    const leaves = [_]LeafView{leafFull(0x1000, false, &p)};
    var walker: Walker = .{ .resident = &.{layout64.foldCapacity()} };
    var ledger: HeapLedger = .{};
    var posture: HeapFullPosture = .{};
    var refused: std.ArrayList(Refused) = .empty;
    defer refused.deinit(testing.allocator);
    try testing.expect(posture.enter("test-vol", "test", .{ .free = 0, .promised = 0, .reserve = reserve }));
    try testing.expect(posture.isFull());
    const budget: Budget = .{ .free = 100, .reserve = reserve };
    try admit(&budget, &walker, &ledger, &posture, &members, &leaves, &refused);
    try testing.expectEqual(@as(usize, 0), refused.items.len);
    try testing.expect(!posture.isFull());
    try testing.expectEqual(@as(u64, 4), p.promised);
    try testing.expectEqual(@as(u32, 1), walker.calls);
    // Growth below the split window rides the cascade extent: no walk.
    try admit(&budget, &walker, &ledger, &posture, &members, &leaves, &refused);
    try testing.expectEqual(@as(u32, 1), walker.calls);
    try testing.expectEqual(encodedLen(one[0]), p.added);
    try testing.expectEqual(@as(u64, 4), p.promised);
    // Past the window: the exact walk runs again; the fold now holds every
    // byte since, so the promise grows by the parts it needs.
    const big = [_]tx.Rec{rec(&k, layout64.splitGrowthWindow())};
    const grow = [_]Member{.{ .recs = &big, .leaf_of = &.{0} }};
    try admit(&budget, &walker, &ledger, &posture, &grow, &leaves, &refused);
    try testing.expectEqual(@as(u32, 2), walker.calls);
    try testing.expectEqual(@as(usize, 0), p.added);
    try testing.expect(p.promised >= 4);
    try testing.expectEqual(p.promised, ledger.load());
    p.release(&ledger);
    try testing.expectEqual(@as(u64, 0), ledger.load());
}

test "a compaction promise (1) covers growth while the fold stays within one node, then re-walks; the re-walk's delta 0 only re-bases" {
    const reserve: u64 = 8;
    var p: LeafPromise = .{};
    const k = key.inodeKey(1);
    const small = [_]tx.Rec{rec(&k, 100)};
    const members = [_]Member{.{ .recs = &small, .leaf_of = &.{0} }};
    const leaves = [_]LeafView{leafFull(0x1000, false, &p)};
    var walker: Walker = .{ .resident = &.{1000} };
    var ledger: HeapLedger = .{};
    var posture: HeapFullPosture = .{};
    var refused: std.ArrayList(Refused) = .empty;
    defer refused.deinit(testing.allocator);
    const budget: Budget = .{ .free = 100, .reserve = reserve };
    try admit(&budget, &walker, &ledger, &posture, &members, &leaves, &refused);
    try testing.expectEqual(@as(u64, 1), p.promised);
    try testing.expectEqual(@as(usize, 1000 + encodedLen(small[0])), p.basis);
    try testing.expectEqual(@as(u32, 1), walker.calls);
    try admit(&budget, &walker, &ledger, &posture, &members, &leaves, &refused);
    try testing.expectEqual(@as(u32, 1), walker.calls);
    try testing.expectEqual(encodedLen(small[0]), p.added);
    // A record that pushes basis + added past the fold capacity re-walks
    // (the upper bound credited no shadowing); the exact walk, over a
    // resident fold the shadows collapsed, still projects a compaction, so
    // need 1 − promised 1 = 0: re-base on the exact estimate, no new
    // promise.
    const huge = [_]tx.Rec{rec(&k, layout64.foldCapacity() - 1000)};
    const grow = [_]Member{.{ .recs = &huge, .leaf_of = &.{0} }};
    walker.resident = &.{0};
    try admit(&budget, &walker, &ledger, &posture, &grow, &leaves, &refused);
    try testing.expectEqual(@as(u32, 2), walker.calls);
    try testing.expectEqual(@as(usize, 0), refused.items.len);
    try testing.expectEqual(@as(u64, 1), p.promised);
    try testing.expectEqual(encodedLen(huge[0]), p.basis);
    try testing.expectEqual(@as(usize, 0), p.added);
    try testing.expectEqual(@as(u64, 1), ledger.load());
    p.release(&ledger);
}

test "a batch: a refused member's records are excluded from later members' walks; survivors keep their promises across the caller's retry" {
    const reserve: u64 = 8;
    const floor = alloc_core.compactionFloorExtents(reserve);
    var pa: LeafPromise = .{};
    var pb: LeafPromise = .{};
    const leaves = [_]LeafView{ leafFull(0x1000, false, &pa), leafFull(0x2000, false, &pb) };
    const k = key.inodeKey(1);
    const r = [_]tx.Rec{ rec(&k, 300), rec(&k, 300), rec(&k, 300) };
    // m0: leaf A only (compaction); m1: A and B (B refused ⇒ m1 refused);
    // m2: leaf A — admitted under A's promise (growth window).
    const members = [_]Member{
        .{ .recs = r[0..1], .leaf_of = &.{0} },
        .{ .recs = r[1..3], .leaf_of = &.{ 0, 1 } },
        .{ .recs = r[0..1], .leaf_of = &.{0} },
    };
    const budget: Budget = .{ .free = floor + 1, .reserve = reserve };
    var walker: Walker = .{ .resident = &.{ 5000, 5000 } };
    var ledger: HeapLedger = .{};
    var posture: HeapFullPosture = .{};
    var refused: std.ArrayList(Refused) = .empty;
    defer refused.deinit(testing.allocator);
    try admit(&budget, &walker, &ledger, &posture, &members, &leaves, &refused);
    try testing.expectEqual(@as(usize, 1), refused.items.len);
    try testing.expectEqual(@as(usize, 1), refused.items[0].member);
    try testing.expectEqual(@as(u64, 1), pa.promised);
    try testing.expectEqual(@as(u64, 0), pb.promised);
    try testing.expectEqual(@as(u64, 1), ledger.load());
    // m1's noted growth on A was retracted; m0's and m2's remain.
    try testing.expectEqual(encodedLen(r[0]), pa.added);
    pa.release(&ledger);
}

test "full-heap contract: every admitted member is flushable (in place or promised ≥ need); the ledger is 0 once the SMOs consume; the wedge audit never fires on a full heap" {
    const reserve: u64 = 8;
    var ps: [3]LeafPromise = @splat(.{});
    const leaves = [_]LeafView{ leafFull(0x1000, false, &ps[0]), leafFull(0x2000, false, &ps[1]), leafFull(0x3000, false, &ps[2]) };
    const k = key.inodeKey(1);
    const r = [_]tx.Rec{ rec(&k, 300), rec(&k, 300), rec(&k, 300) };
    const members = [_]Member{
        .{ .recs = r[0..1], .leaf_of = &.{0} },
        .{ .recs = r[1..2], .leaf_of = &.{1} },
        .{ .recs = r[2..3], .leaf_of = &.{2} },
    };
    // Room for two compactions above the floor; the third is refused.
    const floor = alloc_core.compactionFloorExtents(reserve);
    const budget: Budget = .{ .free = floor + 2, .reserve = reserve };
    var walker: Walker = .{ .resident = &.{ 100, 100, 100 } };
    var ledger: HeapLedger = .{};
    var posture: HeapFullPosture = .{};
    var refused: std.ArrayList(Refused) = .empty;
    defer refused.deinit(testing.allocator);
    try admit(&budget, &walker, &ledger, &posture, &members, &leaves, &refused);
    try testing.expectEqual(@as(usize, 1), refused.items.len);
    try testing.expectEqual(@as(usize, 2), refused.items[0].member);
    for (ps[0..2]) |p| try testing.expectEqual(@as(u64, 1), p.promised);
    try testing.expectEqual(@as(u64, 0), ps[2].promised);
    try testing.expectEqual(@as(u64, 2), ledger.load());
    // Finalize: counted, the posture latches once, the error maps to ENOSPC.
    const before = stats.meta_kv_enospc_refusals.load();
    const e = finalizeRefusal(&posture, "test-vol", refused.items[0].why, ledger.load());
    try testing.expectEqual(HeapError.NoSpace, e);
    try testing.expectEqual(std.os.linux.E.NOSPC, errors.toErrno(coreError(e)));
    try testing.expectEqual(@as(u16, 28), @backingInt(errors.toErrno(coreError(e))));
    try testing.expectEqual(before + 1, stats.meta_kv_enospc_refusals.load());
    try testing.expect(posture.isFull());
    try testing.expectEqual(@as(u64, 1), alloc_ext.meta_kv_heap_full.load());
    // The SMOs consume the promises: 0 at quiesce.
    for (&ps) |*p| p.release(&ledger);
    try testing.expectEqual(@as(u64, 0), ledger.load());
    // A full heap is the SPACE class for as many cycles as it lasts — never
    // the wedge terminal.
    var audit: alloc_core.SpaceAudit = .{};
    var i: u32 = 0;
    while (i < 3 * alloc_core.pending_free_force_cycles) : (i += 1) {
        const out = audit.tick(.{ .pending_before = 5, .pending_after = 5, .tail_before = 100, .tail = 100, .deferred_for_grant = 0, .deferred_for_space = 2 });
        try testing.expectEqual(alloc_core.CycleVerdict.space_standstill, out.verdict);
        try testing.expect(!audit.failed);
    }
    _ = posture.leave("test-vol", "test", .{ .free = 0, .promised = 0, .reserve = reserve });
}

test "ONE posture, one wedge rung: a deferring cycle counts heap_full_cycles exactly once (the posture's path) and latches the gauge while the audit answers the space class; clear cycles leave the posture and climb the rung to the terminal" {
    var posture: HeapFullPosture = .{};
    var audit: alloc_core.SpaceAudit = .{};
    const snap: SpaceSnapshot = .{ .free = 3, .promised = 0, .reserve = 8 };
    const standstill = alloc_core.CycleObservations{ .pending_before = 1, .pending_after = 1, .tail_before = 3, .tail = 3, .deferred_for_grant = 0, .deferred_for_space = 1 };
    const cycles0 = alloc_ext.meta_kv_heap_full_cycles.load();
    // The cycle's C5 then C12, as the checkpointer runs them.
    posture.cycleFlushPass("v", standstill.deferred_for_space, false, snap);
    var out = audit.tick(standstill);
    try testing.expectEqual(alloc_core.CycleVerdict.space_standstill, out.verdict);
    try testing.expect(posture.isFull());
    try testing.expectEqual(@as(u64, 1), alloc_ext.meta_kv_heap_full.load());
    try testing.expectEqual(cycles0 + 1, alloc_ext.meta_kv_heap_full_cycles.load());
    try testing.expect(!audit.failed);
    // Cycles that defer nothing with the floor clear: the posture leaves at
    // the first, the counter stands, and the rung climbs.
    var wedged = standstill;
    wedged.deferred_for_space = 0;
    var i: u64 = 1;
    while (i < alloc_core.pending_free_force_cycles) : (i += 1) {
        posture.cycleFlushPass("v", 0, true, snap);
        out = audit.tick(wedged);
        try testing.expectEqual(alloc_core.CycleVerdict.wedge_rung, out.verdict);
        try testing.expectEqual(i, out.stalled_cycles);
        try testing.expect(!audit.failed);
    }
    try testing.expect(!posture.isFull());
    try testing.expectEqual(@as(u64, 0), alloc_ext.meta_kv_heap_full.load());
    try testing.expectEqual(cycles0 + 1, alloc_ext.meta_kv_heap_full_cycles.load());
    posture.cycleFlushPass("v", 0, true, snap);
    out = audit.tick(wedged);
    try testing.expectEqual(alloc_core.CycleVerdict.wedge_failed, out.verdict);
    try testing.expect(audit.failed);
}

test "the posture word transitions loud once per edge; the cycle half counts heap_full_cycles and clears only with the growth floor clear" {
    var posture: HeapFullPosture = .{};
    const snap: SpaceSnapshot = .{ .free = 3, .promised = 1, .reserve = 8 };
    try testing.expect(!posture.isFull());
    try testing.expect(!posture.leave("v", "idle", snap));
    try testing.expect(posture.enter("v", "refused", snap));
    try testing.expect(!posture.enter("v", "refused again", snap));
    try testing.expectEqual(@as(u64, 1), alloc_ext.meta_kv_heap_full.load());
    const cycles = alloc_ext.meta_kv_heap_full_cycles.load();
    // A pass that deferred nothing with the floor NOT clear stays latched.
    posture.cycleFlushPass("v", 0, false, snap);
    try testing.expect(posture.isFull());
    try testing.expectEqual(cycles, alloc_ext.meta_kv_heap_full_cycles.load());
    // A deferring pass counts a cycle (already latched: no transition).
    posture.cycleFlushPass("v", 3, false, snap);
    try testing.expectEqual(cycles + 1, alloc_ext.meta_kv_heap_full_cycles.load());
    // Floor clear and nothing deferred: leaves.
    posture.cycleFlushPass("v", 0, true, snap);
    try testing.expect(!posture.isFull());
    try testing.expectEqual(@as(u64, 0), alloc_ext.meta_kv_heap_full.load());
    try testing.expect(!posture.leave("v", "again", snap));
}

test "growth floor tie: clear iff free − promised − 4 ≥ reserve (the smallest split is two parts + 2)" {
    try testing.expectEqual(@as(u64, 4), minSplitExtents(layout64));
    const l256 = NodeLayout.new(256 * 1024, null) catch unreachable;
    try testing.expectEqual(@as(u64, 4), minSplitExtents(l256));
    const reserve: u64 = 8;
    try testing.expect(heapGrowthFloorClear(layout64, 12, 0, reserve));
    try testing.expect(!heapGrowthFloorClear(layout64, 11, 0, reserve));
    try testing.expect(!heapGrowthFloorClear(layout64, 12, 1, reserve));
    try testing.expect(heapGrowthFloorClear(layout64, 13, 1, reserve));
    try testing.expect(!heapGrowthFloorClear(layout64, 0, 100, reserve));
    // Agrees with the admission's own verdict for the smallest split.
    const v = alloc_core.admitLeaf(12, 0, reserve, 4, 0);
    try testing.expect(v == .admitted);
    const w = alloc_core.admitLeaf(11, 0, reserve, 4, 0);
    try testing.expect(w == .no_space);
}

test "space retry: one retry when budget is returnable, final otherwise or once spent or past the wall bound; the cycle loop stops at 2 or when nothing is pending; the hold yields merges" {
    const bound: u64 = 30 * std.time.ns_per_s;
    try testing.expectEqual(SpaceRetryDecision.retry_after_cycles, decideSpaceRetry(.{ .cycled_for_space = false, .heap_promised = 1, .pending_free = 0, .elapsed_ns = 0, .wall_bound_ns = bound }));
    try testing.expectEqual(SpaceRetryDecision.retry_after_cycles, decideSpaceRetry(.{ .cycled_for_space = false, .heap_promised = 0, .pending_free = 7, .elapsed_ns = 0, .wall_bound_ns = bound }));
    try testing.expectEqual(SpaceRetryDecision.final_refusal, decideSpaceRetry(.{ .cycled_for_space = false, .heap_promised = 0, .pending_free = 0, .elapsed_ns = 0, .wall_bound_ns = bound }));
    try testing.expectEqual(SpaceRetryDecision.final_refusal, decideSpaceRetry(.{ .cycled_for_space = true, .heap_promised = 5, .pending_free = 5, .elapsed_ns = 0, .wall_bound_ns = bound }));
    try testing.expectEqual(SpaceRetryDecision.final_refusal, decideSpaceRetry(.{ .cycled_for_space = false, .heap_promised = 5, .pending_free = 5, .elapsed_ns = bound, .wall_bound_ns = bound }));
    try testing.expectEqual(@as(u32, 2), SPACE_RETRY_CYCLES);
    try testing.expect(continueSpaceRetryCycles(0, 3));
    try testing.expect(continueSpaceRetryCycles(1, 3));
    try testing.expect(!continueSpaceRetryCycles(2, 3));
    try testing.expect(!continueSpaceRetryCycles(1, 0));

    var hold: SpaceRetryHold = .{};
    try testing.expect(!hold.inflight());
    hold.begin();
    try testing.expect(hold.inflight());
    const yields = stats.meta_kv_merge_yields.load();
    SpaceRetryHold.noteMergeYield();
    try testing.expectEqual(yields + 1, stats.meta_kv_merge_yields.load());
    hold.begin();
    hold.end();
    try testing.expect(hold.inflight());
    hold.end();
    try testing.expect(!hold.inflight());
}

test "leaf promise ledger: promise/retract/release keep the volume ledger and the gauge exact; retract never underflows; release clears the basis" {
    var ledger: HeapLedger = .{};
    var a: LeafPromise = .{};
    var b: LeafPromise = .{};
    const g0 = alloc_ext.meta_kv_heap_promised.load();
    a.promise(&ledger, 3, 1000);
    b.promise(&ledger, 1, 500);
    try testing.expectEqual(@as(u64, 4), ledger.load());
    try testing.expectEqual(g0 + 4, alloc_ext.meta_kv_heap_promised.load());
    a.noteGrowth(40);
    a.retractGrowth(100);
    try testing.expectEqual(@as(usize, 0), a.added);
    a.retract(&ledger, 10);
    try testing.expectEqual(@as(u64, 0), a.promised);
    try testing.expectEqual(@as(usize, 0), a.basis);
    try testing.expectEqual(@as(u64, 1), ledger.load());
    b.setBasis(700);
    try testing.expectEqual(@as(usize, 700), b.basis);
    b.release(&ledger);
    try testing.expectEqual(@as(u64, 0), ledger.load());
    try testing.expectEqual(g0, alloc_ext.meta_kv_heap_promised.load());
    try testing.expectEqual(@as(usize, 0), b.basis);
    // Releasing an empty promise is a no-op.
    b.release(&ledger);
    try testing.expectEqual(@as(u64, 0), ledger.load());
}

test "a wide union (> 32 leaves) takes the heap-buffered fast path and still admits in place" {
    var ps: [40]LeafPromise = @splat(.{});
    var leaves: [40]LeafView = undefined;
    for (&leaves, 0..) |*l, i| l.* = leafWithPages(0x1000 * (i + 1), 1, false, &ps[i]);
    const k = key.inodeKey(1);
    var recs: [40]tx.Rec = undefined;
    var leaf_of: [40]usize = undefined;
    for (&recs, 0..) |*r, i| {
        r.* = rec(&k, 50);
        leaf_of[i] = i;
    }
    const members = [_]Member{.{ .recs = &recs, .leaf_of = &leaf_of }};
    const budget: Budget = .{ .free = 0, .reserve = 8 };
    var resident: [40]usize = @splat(0);
    var walker: Walker = .{ .resident = &resident };
    var ledger: HeapLedger = .{};
    var posture: HeapFullPosture = .{};
    var refused: std.ArrayList(Refused) = .empty;
    defer refused.deinit(testing.allocator);
    try admit(&budget, &walker, &ledger, &posture, &members, &leaves, &refused);
    try testing.expectEqual(@as(usize, 0), refused.items.len);
    try testing.expectEqual(@as(u32, 0), walker.calls);
}

test "NoSpace formats its surface" {
    var buf: [128]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try (NoSpace{ .free = 4, .reserve = 4 }).format(&w);
    try testing.expectEqualStrings("metadata heap full: 4 claimable extent(s) against a floor of 4", w.buffered());
}
