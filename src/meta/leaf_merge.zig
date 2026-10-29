//! Leaf merge, interior recursion and root collapse (charter §5.3 "Leaf
//! merge": `C` = fold capacity; mergeable iff `f(L)+f(R) ≤ ¾C ∧ min ≤ ¼C`
//! (the split's constants read backwards; hysteresis structural); triggers:
//! the flush pass's O(1) prefilter, the heap-full sweep (bounded per cycle
//! to one tick period, cursor-resumed, levels walked leaves-up), the D4
//! defrag arm; interior recursion + root collapse are the ONLY way height
//! decreases; survey `docs/porting/kv_tree_cache_commit_survey.md` §2.12,
//! §7 row 8; `docs/porting/kv_node.md` §5.4; the reference's `tree.rs`
//! §4.6a).
//!
//! The merge is the §4.6 three-step replacement over TWO frozen sources
//! (`smo.zig`'s protocol verbatim: build with no locks → barrier → admit →
//! parent, then both children in ascending ADDRESS order (the commit
//! path's leaf order, never key order — so a multi-leaf transaction
//! holding one sibling and waiting on the other can never form a cycle
//! with this SMO) → reserve inside → move both deltas → publish → flip →
//! retire → release → write → park). The parent's records are `Put(R.max
//! → succ)` + `Delete(L.max)` in ONE snapshot swap: the Put REPLACES the
//! right's separator by per-key LWW, the tombstone retires the left's, and
//! `nextLive` routes every key of the union to the successor. The entry is
//! `[Put, Delete, alloc(succ), free(L), free(R)]` — flips first (the
//! parent's floor pins at `res.start`), the last free the entry's highest
//! position (the coverage gate for BOTH retirements).
//!
//! Space: a merge is −1 extent net, admitted at the COMPACTION FLOOR
//! (`reserve/2`), so a full volume's deletes always have room to fold;
//! the merged extents return 1–2 barriered cycles later and recovery is a
//! geometric wave (measured 8 → 16 → 32 → 64 → 128 merges per sweep, 313
//! merges in 14 sweeps, free 11 → 325 on a 24 MiB / 64 KiB volume).

const std = @import("std");
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const clock = @import("../core/clock.zig");
const key = @import("key.zig");
const rec = @import("record.zig");
const node = @import("node.zig");
const node_cache = @import("node_cache.zig");
const snapshot = @import("node_snapshot.zig");
const jc = @import("journal_core.zig");
const alloc_core = @import("alloc_core.zig");
const heap_law = @import("heap_law.zig");
const checkpoint = @import("checkpoint.zig");
const tree = @import("tree.zig");
const smo = @import("smo.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;

pub const KvTree = tree.KvTree;
pub const CachedNode = node_cache.CachedNode;
pub const OwnedRec = node_cache.OwnedRec;
pub const SmoContext = smo.SmoContext;
pub const MaintenanceOutcome = smo.MaintenanceOutcome;
pub const Error = smo.Error;

// ---------------------------------------------------------------------------
// Observability (survey §5.5 / §5.7 names).
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// §4.6a sibling merges (two nodes → one); moves on delete-heavy loads.
    pub var meta_kv_node_merges: counters.ShardedCounter = .{};
    /// The level-≥1 face of `meta_kv_node_merges` (cross-parent shrinkage).
    pub var meta_kv_interior_merges: counters.ShardedCounter = .{};
    /// Height decrements.
    pub var meta_kv_root_collapses: counters.ShardedCounter = .{};
    /// Bounded sweep calls / whole laps.
    pub var meta_kv_merge_sweeps: counters.ShardedCounter = .{};
    pub var meta_kv_merge_laps: counters.ShardedCounter = .{};
    /// `ns ÷ projections` = ns per leaf projected.
    pub var meta_kv_merge_sweep_ns: counters.ShardedCounter = .{};
    pub var meta_kv_merge_sweep_projections: counters.ShardedCounter = .{};
    /// EXACT under the tail its lap counted with: underfull leaves of the
    /// last completed lap (per tree here; the volume lap is the
    /// checkpoint's aggregation).
    pub var meta_kv_merge_candidates: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// Derivations.
// ---------------------------------------------------------------------------

/// The sweep's per-cycle WORK BUDGET: one checkpoint tick period — the
/// same law the threshold drain follows ("bounded by one cadence period;
/// at least one item per pass; leftovers re-arm"). The projection walk is
/// O(resident nodes) per lap (≈ 2,048 × the bench's ns/leaf on the shipped
/// 512 MiB / 256 KiB geometry) and runs INSIDE the cycle a full-cache
/// volume's ring reclamation depends on, so a cycle may spend at most one
/// tick on it and the lap resumes from its cursor next cycle. Never a
/// constant of its own: `checkpoint.zig` owns the ONE derivation; the tie
/// test below restates it.
pub const mergeSweepBudgetMs = checkpoint.mergeSweepBudgetMs;

// ---------------------------------------------------------------------------
// The law.
// ---------------------------------------------------------------------------

/// `f(N)` of the candidate law: the K1 fold's byte upper bound over the
/// node's serialized log + frozen delta + open delta, with tombstones
/// below `durable_tail` credited as elided (`compactFold`'s rule).
pub fn foldUpperAt(t: *const KvTree, n: *CachedNode, durable_tail: u64) usize {
    const snap = n.snapshotLoad();
    defer snap.release();
    var none: [0]snapshot.Extra = .{};
    return snap.value.foldBytesUpperWith(&none, t.cache.cfg.layout.mergePairCapacity(), durable_tail).bytes;
}

pub fn foldUpper(t: *const KvTree, n: *CachedNode) usize {
    return foldUpperAt(t, n, t.durableTail());
}

/// The pair law on two fold estimates: the pair fits a fresh split part
/// AND one of the two is underfull (`node.mergeable`).
pub fn pairMergeable(layout: node.NodeLayout, a: usize, b: usize) bool {
    return node.mergeable(layout, a, b);
}

/// The candidate predicate — ONE source of truth for the sweep's walk, its
/// count phase, the census and the bench: a live non-root node of this
/// tree whose fold projects at or under the underfull bound `¼C`. The tail
/// decides which tombstones project as elided, so a count is comparable to
/// another only under the SAME tail (a later tail can only ADD candidates).
pub fn isMergeCandidateAt(t: *KvTree, n: *CachedNode, durable_tail: u64) bool {
    return !n.state.isSuperseded() and t.owns(n) and !t.isRoot(n) and foldUpperAt(t, n, durable_tail) <= t.cache.cfg.layout.mergeCandidateCapacity();
}

pub fn isMergeCandidate(t: *KvTree, n: *CachedNode) bool {
    return isMergeCandidateAt(t, n, t.durableTail());
}

/// D4's per-leaf occupancy (`ops_jobs_lifecycle.md` §3.1): the serialized
/// bset log's bytes past the header page (`indexed` — every frame ever
/// appended) against the ON-DISK length of the one frame a compaction
/// writes for the K1 fold's upper bound (`live`, page-rounded like the
/// log it replaces — a leaf already one frame long reads 0 dead, so the
/// nudge never rewrites what it cannot shrink: the first live row kicked
/// 7,742 rewrites of one-page leaves); the dead share is `1 − live/indexed`.
pub const LeafOccupancy = struct {
    log_bytes: u64,
    fold_frame_bytes: u64,

    pub fn deadPermille(self: LeafOccupancy) u64 {
        if (self.log_bytes == 0 or self.fold_frame_bytes >= self.log_bytes) return 0;
        return (self.log_bytes - self.fold_frame_bytes) * 1000 / self.log_bytes; // permille: the gauge unit the D4 ratios share
    }

    /// The bytes a compaction reclaims (the nudge's ranking key).
    pub fn deadBytes(self: LeafOccupancy) u64 {
        return self.log_bytes -| self.fold_frame_bytes;
    }
};

pub fn leafOccupancy(t: *KvTree, n: *CachedNode) LeafOccupancy {
    const log_bytes: u64 = blk: {
        const d = n.lockRead(t.cache.io);
        defer n.unlockRead(t.cache.io);
        break :blk d.tailOffset() -| node.NODE_PAGE;
    };
    return .{ .log_bytes = log_bytes, .fold_frame_bytes = t.cache.cfg.layout.appendFrameLen(foldUpper(t, n)) };
}

/// One resident node of a level: its `min_key` (the sweep's resume
/// cursor) and address.
pub const ResidentNode = struct {
    min: key.KeyBuf,
    addr: u64,
};

const ResidentCollector = struct {
    t: *const KvTree,
    level: u8,
    from: []const u8,
    out: *std.ArrayList(ResidentNode),
    failed: bool = false,

    fn visit(self: *ResidentCollector, n: *CachedNode) void {
        if (!self.t.owns(n) or n.level != self.level or n.state.isSuperseded()) return;
        if (key.lessThan(n.min_key, self.from)) return;
        self.out.append(self.t.gpa, .{ .min = key.KeyBuf.fromSlice(n.min_key), .addr = n.addr }) catch {
            self.failed = true;
        };
    }
};

fn residentLess(_: void, a: ResidentNode, b: ResidentNode) bool {
    return key.lessThan(a.min.slice(), b.min.slice());
}

/// This tree's RESIDENT nodes at `level` in key order from `from`
/// (inclusive on `min_key`) — the sweep's per-level census surface; no
/// device reads. Caller frees.
pub fn residentNodesAt(t: *const KvTree, gpa: Allocator, level: u8, from: []const u8) Allocator.Error![]ResidentNode {
    var out: std.ArrayList(ResidentNode) = .empty;
    errdefer out.deinit(gpa);
    var c: ResidentCollector = .{ .t = t, .level = level, .from = from, .out = &out };
    t.cache.forEachNode(&c, ResidentCollector.visit);
    if (c.failed) return error.OutOfMemory;
    std.mem.sort(ResidentNode, out.items, {}, residentLess);
    return out.toOwnedSlice(gpa);
}

/// The projection-only census of this tree's resident leaves under
/// `durable_tail`: how many are candidates right now (the sweep's count
/// phase without the cursor).
pub fn mergeCandidateCensusAt(t: *KvTree, durable_tail: u64) Allocator.Error!u64 {
    const leaves = try residentNodesAt(t, t.gpa, 0, "");
    defer t.gpa.free(leaves);
    var n: u64 = 0;
    for (leaves) |l| {
        const c = t.cache.tryGet(l.addr) orelse continue;
        defer c.release();
        n += @intFromBool(isMergeCandidateAt(t, c, durable_tail));
    }
    return n;
}

pub fn mergeCandidateCensus(t: *KvTree) Allocator.Error!u64 {
    return mergeCandidateCensusAt(t, t.durableTail());
}

// ---------------------------------------------------------------------------
// The bounded, cursor-resumed sweep.
// ---------------------------------------------------------------------------

/// Where a lap stands between bounded calls.
pub const SweepPhase = union(enum) {
    /// Walking the nodes at this level (leaves up).
    merge: u8,
    /// Every level walked and the collapse chain run: counting the leaves
    /// that remain underfull.
    count,
};

/// The per-tree sweep cursor — the lap's resumable state (touched only by
/// the serialized SMO task).
pub const SweepCursor = struct {
    phase: SweepPhase = .{ .merge = 0 },
    /// The `min_key` the walk resumes at within the current phase.
    next_min: key.KeyBuf = .{},
    /// Underfull leaves counted so far in this lap's count phase.
    lap_candidates: u64 = 0,
    /// The compaction floor refused a merge earlier in this lap.
    space_refused: bool = false,
};

/// Why a sweep call stopped short of its deadline: a merge admission the
/// caller's checkpoint cycle remedies.
pub const SweepRefusal = union(enum) {
    journal_reserve: struct { needed: u64 },
    pending_free_full: struct { pending: u64 },
};

/// What one `mergeUnderfull` call did.
pub const MergeSweep = struct {
    outcome: MaintenanceOutcome = .{},
    /// The EXACT count of underfull leaves standing after the lap's merges
    /// when `lap_complete`; the running count otherwise.
    candidates: u64 = 0,
    /// A merge was refused at the compaction floor during this lap:
    /// merging stopped, counting continued — the backlog stands and the
    /// next barriered cycle returns extents.
    space_refused: bool = false,
    /// The lap ended in this call (every level walked, the collapse chain
    /// run, the count phase finished, the cursor reset).
    lap_complete: bool = false,
    /// A merge refused for a reason a cycle remedies: the lap stopped at
    /// that node with its merges so far REPORTED, the cursor parked there.
    refusal: ?SweepRefusal = null,
    /// Nodes projected by this call.
    projections: u64 = 0,
    /// This call's wall time.
    elapsed_ns: u64 = 0,
};

fn refusalOf(ctx: *const SmoContext) ?SweepRefusal {
    return switch (ctx.last_refusal) {
        .none => null,
        .journal_reserve => |r| .{ .journal_reserve = .{ .needed = r.needed } },
        .pending_free_full => |r| .{ .pending_free_full = .{ .pending = r.pending } },
    };
}

/// The progress law: the deadline is consulted only after this call
/// projected at least one node, so every call makes progress.
fn expired(projected: u64, deadline: ?u64) bool {
    const d = deadline orelse return false;
    return projected >= 1 and clock.monotonicNs() >= d;
}

/// One BOUNDED, CURSOR-RESUMED sweep call over `t`. A LAP walks the
/// resident nodes level by level from the leaves up, each level in key
/// order — every node projected once, every underfull one handed to
/// `tryMergeNode` — then runs the root-collapse chain, then a COUNT phase
/// over the leaves that publishes the EXACT candidate population of the
/// post-merge tree. The interior levels are walked in their own right, not
/// only through the climb: the separator tombstones a leaf merge mints sit
/// ABOVE the durable tail until the next barriered cycle covers them, so a
/// parent that lost most of its children projects underfull only on a
/// LATER lap. `deadline` (monotonic ns; null = one whole lap) bounds ONE
/// call: past it the walk saves its cursor and returns `lap_complete ==
/// false`; the next call resumes there. A compaction-floor refusal stops
/// the MERGING for this lap and skips to the collapse chain and the count,
/// so the published count is exact mid-wave too; any other refusal parks
/// the cursor AT the refused node.
pub fn mergeUnderfull(t: *KvTree, ctx: *SmoContext, forced_retirement: bool, deadline: ?u64) Error!MergeSweep {
    const gpa = t.gpa;
    const started = clock.monotonicNs();
    var sweep: MergeSweep = .{};
    var cursor = t.merge_cursor;
    outer: while (true) {
        switch (cursor.phase) {
            .merge => |level| {
                if (level >= try t.rootLevel()) {
                    // Every level below the root walked (or the root
                    // collapsed under us): the collapse chain, then the
                    // count. A collapse refused parks the lap HERE (the
                    // chain re-runs on resume — a collapse is idempotent).
                    collapseRootChain(t, ctx, &sweep.outcome, forced_retirement) catch |e| switch (e) {
                        error.JournalReserveExhausted, error.PendingFreeFull => {
                            sweep.refusal = refusalOf(ctx);
                            break :outer;
                        },
                        else => return e,
                    };
                    cursor.phase = .count;
                    cursor.next_min = .{};
                    continue;
                }
                const nodes = try residentNodesAt(t, gpa, level, cursor.next_min.slice());
                defer gpa.free(nodes);
                var level_done = true;
                for (nodes) |rn| {
                    if (expired(sweep.projections, deadline)) {
                        cursor.next_min = rn.min;
                        level_done = false;
                        break;
                    }
                    // Park the cursor AT this node before touching it: a
                    // refusal propagating out of the merge leaves the walk
                    // resumable exactly here.
                    cursor.next_min = rn.min;
                    t.merge_cursor = cursor;
                    const n = t.cache.tryGet(rn.addr) orelse continue; // evicted/merged since the census
                    defer n.release();
                    sweep.projections += 1;
                    if (!isMergeCandidate(t, n)) continue;
                    if (cursor.space_refused) continue; // counting only: the floor refused this lap
                    tryMergeNode(t, ctx, n, &sweep.outcome, forced_retirement) catch |e| switch (e) {
                        error.NoSpace => {
                            cursor.space_refused = true;
                            sweep.space_refused = true;
                        },
                        error.JournalReserveExhausted, error.PendingFreeFull => {
                            sweep.refusal = refusalOf(ctx);
                            level_done = false;
                            break;
                        },
                        else => return e,
                    };
                }
                if (!level_done) break :outer;
                // Nothing more can merge this lap after a floor refusal:
                // straight to the collapse chain (no claim) and the count.
                cursor.phase = .{ .merge = if (cursor.space_refused) std.math.maxInt(u8) else level + 1 };
                cursor.next_min = .{};
            },
            .count => {
                const nodes = try residentNodesAt(t, gpa, 0, cursor.next_min.slice());
                defer gpa.free(nodes);
                var done = true;
                for (nodes) |rn| {
                    if (expired(sweep.projections, deadline)) {
                        cursor.next_min = rn.min;
                        done = false;
                        break;
                    }
                    const n = t.cache.tryGet(rn.addr) orelse continue;
                    defer n.release();
                    sweep.projections += 1;
                    if (isMergeCandidate(t, n)) cursor.lap_candidates += 1;
                }
                if (done) {
                    sweep.candidates = cursor.lap_candidates;
                    sweep.lap_complete = true;
                    sweep.space_refused = sweep.space_refused or cursor.space_refused;
                    cursor = .{};
                }
                break :outer;
            },
        }
    }
    if (!sweep.lap_complete) {
        sweep.candidates = cursor.lap_candidates;
        sweep.space_refused = sweep.space_refused or cursor.space_refused;
    }
    t.merge_cursor = cursor;
    sweep.elapsed_ns = clock.monotonicNs() -| started;
    stats.meta_kv_merge_sweeps.inc();
    stats.meta_kv_merge_sweep_ns.add(sweep.elapsed_ns);
    stats.meta_kv_merge_sweep_projections.add(sweep.projections);
    if (sweep.lap_complete) {
        stats.meta_kv_merge_laps.inc();
        stats.meta_kv_merge_candidates.set(sweep.candidates);
    }
    return sweep;
}

/// Merge `n` with a sibling while the pair law admits — packing greedily
/// rightwards (a successor keeps merging with ITS right sibling), then
/// leftwards for a rightmost child — and climb: the parent lost a child
/// per merge, so a one-child root collapses and an underfull non-root
/// parent is itself a candidate at its level (bounded by the height). A
/// merge refusal propagates untouched for the caller's posture.
pub fn tryMergeNode(t: *KvTree, ctx: *SmoContext, n: *CachedNode, out: *MaintenanceOutcome, forced_retirement: bool) Error!void {
    const layout = t.cache.cfg.layout;
    var cur = n.retain();
    defer cur.release();
    while (true) {
        if (t.isRoot(cur) or cur.state.isSuperseded() or !t.owns(cur)) return;
        var merged_here = false;
        while (try mergeWithSibling(t, ctx, cur, out, forced_retirement)) |succ| {
            cur.release();
            cur = succ;
            merged_here = true;
        }
        if (!merged_here or t.isRoot(cur)) return;
        const parent = try t.resolveParent(cur);
        if (t.isRoot(parent)) {
            parent.release();
            return collapseRootChain(t, ctx, out, forced_retirement);
        }
        if (foldUpper(t, parent) > layout.mergeCandidateCapacity()) {
            parent.release();
            return;
        }
        cur.release();
        cur = parent;
    }
}

/// A sibling the parent routes to, verified against the pointer (a stale
/// route on the serialized SMO task is a protocol bug, surfaced loud).
fn siblingOf(t: *KvTree, sep: []const u8, ptr: []const u8, level: u8) Error!*CachedNode {
    const iv = key.InteriorValue.decode(ptr) catch return error.Corrupt;
    const s = try t.cache.get(iv.childAddr());
    errdefer s.release();
    if (s.node_seq != iv.childSeq() or s.level != level or s.state.isSuperseded()) {
        log.err("merge sibling probe: separator names 0x{x}@{d} but the object is seq {d} level {d} superseded={}", .{ iv.childAddr(), iv.childSeq(), s.node_seq, s.level, s.state.isSuperseded() });
        return error.Corrupt;
    }
    if (!std.mem.eql(u8, s.max_key, sep)) {
        log.err("merge sibling probe: separator ≠ child 0x{x} max_key", .{iv.childAddr()});
        return error.Corrupt;
    }
    return s;
}

/// Find `n`'s merge partner under its parent — the right sibling first
/// (`nextLive(successor(n.max))`), the left one for a rightmost child
/// (`prevLive(n.min)`) — and run the merge when the pair law admits.
/// `null` = nothing mergeable here. The returned successor is the
/// caller's reference.
fn mergeWithSibling(t: *KvTree, ctx: *SmoContext, n: *CachedNode, out: *MaintenanceOutcome, forced_retirement: bool) Error!?*CachedNode {
    const gpa = t.cache.gpa;
    const layout = t.cache.cfg.layout;
    const f_node = foldUpper(t, n);
    const parent = try t.resolveParent(n);
    defer parent.release();
    const psnap = parent.snapshotLoad();
    defer psnap.release();
    if (key.lessThan(n.max_key, parent.max_key)) {
        const right_min = key.keySuccessor(n.max_key);
        var next = try psnap.value.nextLive(gpa, right_min.slice(), null);
        if (next) |*live| {
            defer live.deinit();
            const right = try siblingOf(t, live.key, live.value, n.level);
            defer right.release();
            if (std.mem.eql(u8, right.min_key, right_min.slice()) and pairMergeable(layout, f_node, foldUpper(t, right))) {
                return smoMerge(t, ctx, parent, n, right, out, forced_retirement);
            }
        }
    }
    if (!std.mem.eql(u8, n.min_key, parent.min_key)) {
        var prev = try psnap.value.prevLive(gpa, n.min_key);
        if (prev) |*live| {
            defer live.deinit();
            const left = try siblingOf(t, live.key, live.value, n.level);
            defer left.release();
            const succ = key.keySuccessor(left.max_key);
            if (std.mem.eql(u8, succ.slice(), n.min_key) and pairMergeable(layout, foldUpper(t, left), f_node)) {
                return smoMerge(t, ctx, parent, left, n, out, forced_retirement);
            }
        }
    }
    return null;
}

/// Both siblings' open deltas as ONE owned slice (left's first). Once the
/// combined allocation exists nothing below can fail, so the halves are
/// freed with no cleanup left armed.
fn takeBothOverlays(gpa: Allocator, left: *node_cache.NodeDirty, right: *node_cache.NodeDirty) Allocator.Error![]OwnedRec {
    const lo = try left.takeOverlay(gpa);
    errdefer {
        for (lo) |*r| r.deinit();
        gpa.free(lo);
    }
    const ro = try right.takeOverlay(gpa);
    errdefer {
        for (ro) |*r| r.deinit();
        gpa.free(ro);
    }
    const both = try gpa.alloc(OwnedRec, lo.len + ro.len);
    @memcpy(both[0..lo.len], lo);
    @memcpy(both[lo.len..], ro);
    gpa.free(lo);
    gpa.free(ro);
    return both;
}

/// Collapse the root while it is an interior node with exactly one live
/// child; chains across levels.
fn collapseRootChain(t: *KvTree, ctx: *SmoContext, out: *MaintenanceOutcome, forced_retirement: bool) Error!void {
    while (true) {
        const root = try t.cache.get(t.rootPtr().addr);
        defer root.release();
        if (!try smoRootCollapse(t, ctx, root, out, forced_retirement)) return;
    }
}

// ---------------------------------------------------------------------------
// The merge SMO.
// ---------------------------------------------------------------------------

/// Fold two ADJACENT siblings under `parent` into ONE successor spanning
/// `[left.min, right.max]` (module doc). Returns the successor (the
/// caller's reference), or `null` when the candidate went stale (the
/// exact fold no longer fits the pair capacity; a superseded/racing node)
/// — a clean decline. Admission refusals are typed: `NoSpace` at the
/// compaction floor, `PendingFreeFull` under the FIFO valve (two
/// retirements ride one entry), `JournalReserveExhausted`.
pub fn smoMerge(t: *KvTree, ctx: *SmoContext, parent: *CachedNode, left: *CachedNode, right: *CachedNode, out: *MaintenanceOutcome, forced_retirement: bool) Error!?*CachedNode {
    const cache = t.cache;
    const gpa = cache.gpa;
    const io = cache.io;
    const layout = cache.cfg.layout;
    const durable_tail = t.durableTail();

    // ---- Step 0: admission, side-effect free.
    const adjacent = std.mem.eql(u8, key.keySuccessor(left.max_key).slice(), right.min_key);
    if (left.level != right.level or !t.owns(left) or !t.owns(right) or left.state.isSuperseded() or right.state.isSuperseded() or !adjacent or t.isRoot(left) or t.isRoot(right) or parent.level != left.level + 1) return null;
    // The merge's claim is the §4.7 compaction class — net −1 allocated,
    // admitted down to `reserve/2`, never the flush pass's own half.
    const claimable = ctx.alloc.freeExtents() -| cache.heapPromised();
    const floor = alloc_core.compactionFloorExtents(ctx.alloc.reserveExtents());
    if (claimable -| 1 < floor) return error.NoSpace;
    if (ctx.journal != null and !forced_retirement and !ctx.pendingHasRoomFor(2)) return ctx.refusePendingFull();

    // ---- Step 1: build the successor from both frozen sources, no locks.
    var fl = (try smo.freezeForSmo(cache, left)) orelse return null;
    defer fl.release();
    var fr = (try smo.freezeForSmo(cache, right)) orelse return null;
    defer fr.release();
    var src_l = try smo.Source.load(cache, left.addr, durable_tail, t.forest_slot);
    defer src_l.deinit(gpa);
    var src_r = try smo.Source.load(cache, right.addr, durable_tail, t.forest_slot);
    defer src_r.deinit(gpa);
    const extra_l = try smo.foldSourceExtra(cache, left, &src_l.loaded);
    defer {
        rec.deinitRecords(gpa, extra_l);
        gpa.free(extra_l);
    }
    const extra_r = try smo.foldSourceExtra(cache, right, &src_r.loaded);
    defer {
        rec.deinitRecords(gpa, extra_r);
        gpa.free(extra_r);
    }
    const refs_l = try smo.refsOf(gpa, extra_l);
    defer gpa.free(refs_l);
    const refs_r = try smo.refsOf(gpa, extra_r);
    defer gpa.free(refs_r);
    // The two key spaces are disjoint, so the cross-node view order is
    // immaterial to the per-key fold.
    var folded = try node.foldNodeSources(gpa, &.{ &src_l.loaded, &src_r.loaded }, &.{ refs_l, refs_r }, durable_tail, @min(left.cut_floor.load(.monotonic), right.cut_floor.load(.monotonic)));
    defer folded.deinit(gpa);
    if (folded.encodedBytes() > layout.mergePairCapacity()) {
        // Stale candidate (records landed since the estimate): a clean
        // decline — the frozen deltas append at the next flush.
        return null;
    }
    const extent = try ctx.claimInternal();
    var published = false;
    errdefer if (!published) ctx.releaseUnpublished(extent);
    const dst = cache.extentAddr(extent);
    const dst_seq = try t.seq.mint();
    const all_refs = try smo.refsOf(gpa, folded.records.items);
    defer gpa.free(all_refs);
    const successor = try smo.writeSuccessor(cache, .{
        .node_addr = dst,
        .node_seq = dst_seq,
        .tree_id = t.tree_id,
        .level = left.level,
        .min_key = left.min_key,
        .max_key = right.max_key,
    }, all_refs, folded.horizon, left.level > 0, t.forest_slot);
    errdefer successor.release();
    // The successor is one fresh image — the flush pass's image class.
    ctx.images_written += 1;
    smo.testSmoBuildPause(t, left.level, false, true, left.min_key, right.max_key);

    // ---- Journaling before any lock: successor durable, then the entry
    // admitted from the checkpoint-task reserve.
    const old_l = cache.addrExtent(left.addr);
    const old_r = cache.addrExtent(right.addr);
    var prep: ?smo.PreparedEntry = null;
    defer if (prep) |*p| p.deinit();
    if (ctx.journal) |j| {
        try j.barrier();
        var recs = smo.SmoEntry.init(gpa);
        {
            // Ours until `prepare`, which takes them on every outcome.
            errdefer recs.deinit();
            try recs.putInterior(t, left.level + 1, right.max_key, dst, dst_seq);
            try recs.deleteInterior(t, left.level + 1, left.max_key);
            try recs.allocRecord(extent);
            const tag = j.retire_seq.load(.acquire);
            try recs.freeRecord(old_l, tag);
            try recs.freeRecord(old_r, tag);
        }
        prep = try smo.PreparedEntry.prepare(ctx, j, recs);
    }

    // ---- Step 2: parent, then both children in ascending address order;
    // reserve inside; move both deltas; publish → flip → retire; release.
    var registered: ?jc.InflightRegistry.Registered = null;
    // §BX.9: an error past the reservation leaves it a landed PAD, once the
    // locks went back.
    errdefer if (registered) |r| ctx.journal.?.padAbandoned(r.res);
    {
        const pd = parent.lockWrite(io);
        errdefer parent.unlockWrite(io);
        const first: *CachedNode = if (left.addr < right.addr) left else right;
        const second: *CachedNode = if (left.addr < right.addr) right else left;
        _ = first.lockWrite(io);
        errdefer first.unlockWrite(io);
        _ = second.lockWrite(io);
        errdefer second.unlockWrite(io);
        if (prep) |*p| registered = try smo.reserveInWindow(ctx.journal.?, p);
        // The bounded second merge over BOTH deltas, exact per-record floors.
        const leftovers = try takeBothOverlays(gpa, &left.dirty, &right.dirty);
        const taken = gpa.alloc(bool, leftovers.len) catch |e| {
            for (leftovers) |*r| r.deinit();
            gpa.free(leftovers);
            return e;
        };
        @memset(taken, false);
        // Every leftover not moved into the successor dies here (an error
        // path); the moved ones were consumed by the successor's apply.
        defer {
            for (leftovers, taken) |*r, tk| if (!tk) r.deinit();
            gpa.free(taken);
            gpa.free(leftovers);
        }
        for ([_]*CachedNode{ left, right }) |n| {
            const outcome = n.state.supersede() catch {
                log.err("double supersede of node 0x{x} (SMO serialization violated)", .{n.addr});
                return error.Corrupt;
            };
            assert(outcome.was_freezing);
        }
        try smo.applyLeftovers(cache, successor, leftovers, taken);
        for (taken) |tk| assert(tk);
        // Route flip before retire: successor published, the parent's two
        // records in ONE snapshot swap, both old objects retired last.
        t.stamp(successor);
        try cache.publish(successor);
        published = true;
        const put_seq = if (prep) |*p| p.recs.seqAt(0) else try t.seq.mint();
        const del_seq = if (prep) |*p| p.recs.seqAt(1) else try t.seq.mint();
        var flips: [2]OwnedRec = undefined;
        flips[0] = try OwnedRec.fromRecordRef(gpa, .{ .key = right.max_key, .seq = put_seq, .kind = .put, .value = &key.encodeInteriorValue(dst, dst_seq) });
        flips[1] = OwnedRec.fromRecordRef(gpa, .{ .key = left.max_key, .seq = del_seq, .kind = .delete, .value = &.{} }) catch |e| {
            flips[0].deinit();
            return e;
        };
        parent.applyLockedStructural(pd, &flips, put_seq) catch |e| return smo.mapApply(e);
        try cache.retire(left);
        try cache.retire(right);
        second.unlockWrite(io);
        first.unlockWrite(io);
        parent.unlockWrite(io);
    }
    _ = left.state.endFreeze();
    _ = right.state.endFreeze();
    fl.armed = false;
    fr.armed = false;

    // ---- Step 3: after release — entry bytes, both retirements gated on
    // the entry's last record's position, parent maintenance, counters.
    const gate: u64 = if (registered) |r| smo.writeEntryAfterRelease(ctx.journal.?, r, &prep.?, "merge") else try t.seq.mint();
    registered = null;
    try ctx.freePending(old_l, gate, forced_retirement);
    try ctx.freePending(old_r, gate, forced_retirement);
    smo.requeueIfOverThreshold(t, parent);
    stats.meta_kv_node_merges.inc();
    out.merges += 1;
    t.noteNodes(-1);
    if (left.level > 0) {
        stats.meta_kv_interior_merges.inc();
        out.interior_merges += 1;
    }
    return successor;
}

// ---------------------------------------------------------------------------
// Root collapse.
// ---------------------------------------------------------------------------

/// A root interior with exactly ONE live child makes that child the root —
/// a ROOT-SWAP SMO: no pointer record (its durable form is the next ledger
/// record's roots), the dying floor at `res.start` keeps every later
/// record in the replay window until then, the entry is `[free(old_root)]`
/// so a mount from the old ledger replays through the old root — still
/// routing to the child — under the coverage gate. `false` = not a
/// collapse shape (a leaf root, ≥ 2 children). The ONLY way height
/// decreases.
pub fn smoRootCollapse(t: *KvTree, ctx: *SmoContext, root: *CachedNode, out: *MaintenanceOutcome, forced_retirement: bool) Error!bool {
    const cache = t.cache;
    const gpa = cache.gpa;
    const io = cache.io;
    if (!t.isRoot(root) or root.level == 0 or root.state.isSuperseded()) return false;
    const snap = root.snapshotLoad();
    defer snap.release();
    var first = (try snap.value.nextLive(gpa, root.min_key, null)) orelse {
        log.err("root 0x{x} (level {d}) routes to no child", .{ root.addr, root.level });
        return error.Corrupt;
    };
    defer first.deinit();
    var second = try snap.value.nextLive(gpa, key.keySuccessor(first.key).slice(), null);
    if (second) |*s| {
        s.deinit();
        return false; // two or more children
    }
    const iv = key.InteriorValue.decode(first.value) catch return error.Corrupt;
    const child = try cache.get(iv.childAddr());
    defer child.release();
    if (child.node_seq != iv.childSeq() or child.state.isSuperseded() or child.level + 1 != root.level) {
        log.err("root collapse: the sole separator names 0x{x}@{d} level {d} but the object is seq {d} level {d} superseded={}", .{ iv.childAddr(), iv.childSeq(), root.level - 1, child.node_seq, child.level, child.state.isSuperseded() });
        return error.Corrupt;
    }
    if (!(child.min_key.len == 0 and std.mem.eql(u8, child.max_key, &key.KEY_SPACE_MAX) and std.mem.eql(u8, first.key, &key.KEY_SPACE_MAX))) {
        log.err("root collapse: the sole child 0x{x} does not span the whole key space under separator KEY_SPACE_MAX (K2)", .{iv.childAddr()});
        return error.Corrupt;
    }
    if (ctx.journal != null and !forced_retirement and !ctx.pendingHasRoomFor(1)) return ctx.refusePendingFull();
    const old_extent = cache.addrExtent(root.addr);
    var prep: ?smo.PreparedEntry = null;
    defer if (prep) |*p| p.deinit();
    if (ctx.journal) |j| {
        var recs = smo.SmoEntry.init(gpa);
        recs.freeRecord(old_extent, j.retire_seq.load(.acquire)) catch |e| {
            recs.deinit();
            return e;
        };
        prep = try smo.PreparedEntry.prepare(ctx, j, recs);
    }
    var registered: ?jc.InflightRegistry.Registered = null;
    // §BX.9: as in the merge.
    errdefer if (registered) |r| ctx.journal.?.padAbandoned(r.res);
    {
        const rd = root.lockWrite(io);
        errdefer root.unlockWrite(io);
        if (prep) |*p| registered = try smo.reserveInWindow(ctx.journal.?, p);
        // A root swap has no journaled pointer record and no parent floor
        // — clamp the next tail to this swap's position.
        const swap_floor: u64 = if (registered) |r| r.res.start else 0;
        cache.noteDyingFloor(swap_floor);
        t.setRootFloor(swap_floor);
        // The old root's open delta (separator flips whose only live target
        // IS the child) dies with the object: its records are journaled,
        // and replay routes them into the old image or drops them as
        // unroutable under a shorter mounted structure — both sound. The
        // charge/promise it held returns here.
        const dropped = try rd.takeOverlay(gpa);
        for (dropped) |*r| r.deinit();
        gpa.free(dropped);
        _ = root.state.supersede() catch {
            log.err("double supersede of root 0x{x} (SMO serialization violated)", .{root.addr});
            return error.Corrupt;
        };
        child.pin();
        try t.storeRoot(.{ .addr = child.addr, .seq = child.node_seq });
        try cache.retire(root);
        root.unlockWrite(io);
    }
    const gate: u64 = if (registered) |r| smo.writeEntryAfterRelease(ctx.journal.?, r, &prep.?, "root-collapse") else try t.seq.mint();
    registered = null;
    try ctx.freePending(old_extent, gate, forced_retirement);
    stats.meta_kv_root_collapses.inc();
    out.root_collapses += 1;
    t.noteNodes(-1);
    return true;
}

// ---------------------------------------------------------------------------
// The flush-pass prefilter.
// ---------------------------------------------------------------------------

/// After a leaf's flush visit: an O(1) sound-but-incomplete prefilter on
/// the leaf now covering `min_key` (the node itself after an append, its
/// successor after an SMO) — its SERIALIZED log bytes plus its frozen and
/// open deltas at-or-under the underfull bound prove `f ≤ ¼C` (folding
/// only shrinks, frames only add), so only a certain candidate pays the
/// O(node) exact fold + sibling probe. A tombstone-heavy full log is
/// missed here by design and caught when it compacts (its successor's log
/// IS its fold) or by the sweeps. Opportunistic: a floor or reserve
/// refusal is a deferral the heap-full sweep retries, never this visit's
/// error; a certain candidate under a user space retry yields (its claim
/// is at the floor the retry needs).
pub fn mergeAfterFlush(t: *KvTree, ctx: *SmoContext, min_key: []const u8) Error!void {
    const cache = t.cache;
    const layout = cache.cfg.layout;
    const cur = try t.descend(min_key, 0);
    defer cur.release();
    if (t.isRoot(cur) or cur.state.isSuperseded()) return;
    const bound = blk: {
        const d = cur.lockRead(cache.io);
        defer cur.unlockRead(cache.io);
        break :blk (d.tailOffset() -| node.NODE_PAGE) + d.frozenBytes() + d.overlayBytes();
    };
    if (bound > layout.mergeCandidateCapacity()) return;
    if (ctx.space_retry) |h| if (h.inflight()) {
        heap_law.SpaceRetryHold.noteMergeYield();
        return;
    };
    var o: MaintenanceOutcome = .{};
    tryMergeNode(t, ctx, cur, &o, true) catch |e| switch (e) {
        error.NoSpace, error.JournalReserveExhausted => log.debug("flush-pass merge deferred on tree {d} (the heap-full sweep retries): {s}", .{ t.tree_id, @errorName(e) }),
        else => return e,
    };
}

// ---------------------------------------------------------------------------
// Tests — row 8 of the survey (`kv_leaf_merge_tests`) over `smo.TestVolume`.
// ---------------------------------------------------------------------------

const testing = std.testing;
const TestVolume = smo.TestVolume;
const KeyValue = tree.KeyValue;

fn ikey(i: u64) [key.INODE_KEY_LEN]u8 {
    return key.inodeKey(i);
}

fn expectLookup(t: *KvTree, k: []const u8, want: ?[]const u8) !void {
    const got = try t.lookup(testing.allocator, k);
    defer if (got) |g| testing.allocator.free(g);
    if (want) |w| {
        try testing.expectEqualSlices(u8, w, got orelse return error.TestExpectedLive);
    } else {
        try testing.expectEqual(@as(?[]u8, null), got);
    }
}

/// Flush + cover + sweep to a fixpoint: no merge and no collapse left to
/// run. Every sweep is an unbounded lap; the FIFO valve is answered the way
/// the D4 arm answers it (cover, resume). Returns the outcome and the pass
/// count (the terminal no-op pass included).
fn mergeToFixpoint(vol: *TestVolume, t: *KvTree) !struct { out: MaintenanceOutcome, passes: u32 } {
    var total: MaintenanceOutcome = .{};
    var pass: u32 = 1;
    while (pass <= 64) : (pass += 1) {
        try vol.flushDirty(t);
        vol.coverEverything();
        var pass_out: MaintenanceOutcome = .{};
        const sweep = while (true) {
            const s = mergeUnderfull(t, &vol.ctx, false, null) catch |e| switch (e) {
                error.PendingFreeFull => {
                    vol.coverEverything();
                    continue;
                },
                else => return e,
            };
            pass_out.add(s.outcome);
            if (s.lap_complete) break s;
        };
        total.add(pass_out);
        try testing.expect(!sweep.space_refused);
        if (pass_out.merges + pass_out.root_collapses == 0) return .{ .out = total, .passes = pass };
    }
    return error.NoFixpoint;
}

/// The K2 structural walk: every interior node's live separators, in
/// order, name children that partition the parent's range gap-free and
/// every child's `node_seq` matches the pointer. Returns `(leaves, height)`.
fn k2Walk(t: *KvTree) !struct { leaves: u64, height: u8 } {
    const Walker = struct {
        fn walk(tr: *KvTree, n: *CachedNode, leaves: *u64) !void {
            const gpa = testing.allocator;
            if (n.level == 0) {
                leaves.* += 1;
                return;
            }
            const snap = n.snapshotLoad();
            defer snap.release();
            var expected_min = key.KeyBuf.fromSlice(n.min_key);
            var cursor = key.KeyBuf.fromSlice(n.min_key);
            var last_max: ?key.KeyBuf = null;
            var children: u32 = 0;
            while (true) {
                var live = (try snap.value.nextLive(gpa, cursor.slice(), null)) orelse break;
                defer live.deinit();
                const iv = try key.InteriorValue.decode(live.value);
                const child = try tr.cache.get(iv.childAddr());
                defer child.release();
                try testing.expectEqual(iv.childSeq(), child.node_seq);
                try testing.expectEqual(n.level, child.level + 1);
                try testing.expectEqualSlices(u8, expected_min.slice(), child.min_key);
                try testing.expectEqualSlices(u8, live.key, child.max_key);
                expected_min = key.keySuccessor(child.max_key);
                cursor = expected_min;
                last_max = key.KeyBuf.fromSlice(child.max_key);
                children += 1;
                try walk(tr, child, leaves);
            }
            try testing.expect(children >= 1);
            try testing.expectEqualSlices(u8, n.max_key, last_max.?.slice());
        }
    };
    const root = try t.cache.get(t.rootPtr().addr);
    defer root.release();
    try testing.expectEqual(t.rootPtr().seq, root.node_seq);
    try testing.expectEqual(@as(usize, 0), root.min_key.len);
    try testing.expectEqualSlices(u8, &key.KEY_SPACE_MAX, root.max_key);
    var leaves: u64 = 0;
    try Walker.walk(t, root, &leaves);
    return .{ .leaves = leaves, .height = root.level };
}

/// §4.6a (d) as a checkable law — the tree is at its MERGE FIXED POINT: at
/// every level, no two adjacent nodes under ONE parent are jointly
/// mergeable, and no two adjacent nodes under DIFFERENT parents are jointly
/// mergeable unless their parents are themselves NOT jointly mergeable.
/// Returns `(stranded_pairs, heavy_boundaries)`.
fn mergeFixedPoint(t: *KvTree) !struct { stranded: u64, heavy: u64 } {
    const gpa = testing.allocator;
    const layout = t.cache.cfg.layout;
    const Level = struct { n: *CachedNode, parent: u64, fold: usize };
    const root = try t.cache.get(t.rootPtr().addr);
    var level: std.ArrayList(Level) = .empty;
    defer {
        for (level.items) |l| l.n.release();
        level.deinit(gpa);
    }
    try level.append(gpa, .{ .n = root, .parent = std.math.maxInt(u64), .fold = foldUpper(t, root) });
    var stranded: u64 = 0;
    var heavy: u64 = 0;
    while (level.items[0].n.level > 0) {
        var next: std.ArrayList(Level) = .empty;
        errdefer {
            for (next.items) |l| l.n.release();
            next.deinit(gpa);
        }
        for (level.items) |l| {
            const snap = l.n.snapshotLoad();
            defer snap.release();
            var cursor = key.KeyBuf.fromSlice(l.n.min_key);
            while (true) {
                var live = (try snap.value.nextLive(gpa, cursor.slice(), null)) orelse break;
                defer live.deinit();
                const iv = try key.InteriorValue.decode(live.value);
                const child = try t.cache.get(iv.childAddr());
                cursor = key.keySuccessor(child.max_key);
                try next.append(gpa, .{ .n = child, .parent = l.n.addr, .fold = foldUpper(t, child) });
            }
        }
        var i: usize = 1;
        while (i < next.items.len) : (i += 1) {
            const a = next.items[i - 1];
            const b = next.items[i];
            if (!pairMergeable(layout, a.fold, b.fold)) continue;
            try testing.expect(a.parent != b.parent); // siblings jointly mergeable: not at the fixed point
            var fpa: ?usize = null;
            var fpb: ?usize = null;
            for (level.items) |l| {
                if (l.n.addr == a.parent) fpa = l.fold;
                if (l.n.addr == b.parent) fpb = l.fold;
            }
            try testing.expect(!pairMergeable(layout, fpa.?, fpb.?)); // the interior merge the law owes did not run
            stranded += 1;
        }
        i = 1;
        while (i < level.items.len) : (i += 1) heavy += @intFromBool(!pairMergeable(layout, level.items[i - 1].fold, level.items[i].fold));
        for (level.items) |l| l.n.release();
        level.deinit(gpa);
        level = next;
    }
    try testing.expect(stranded <= heavy);
    return .{ .stranded = stranded, .heavy = heavy };
}

/// A deterministic pseudo-random permutation of `0..n` (Fisher–Yates over
/// an LCG) — spreads inserts/deletes across every leaf.
fn permutation(gpa: Allocator, n: u64, seed: u64) ![]u64 {
    const p = try gpa.alloc(u64, @intCast(n));
    for (p, 0..) |*x, i| x.* = i;
    var s = seed | 1;
    var i = p.len;
    while (i > 1) : (i -= 1) {
        s = s *% 6364136223846793005 +% 1442695040888963407;
        const j: usize = @intCast((s >> 33) % i);
        std.mem.swap(u64, &p[i - 1], &p[j]);
    }
    return p;
}

/// The children of an interior node in key order (the caller releases).
fn childrenOf(t: *KvTree, n: *CachedNode, out: *std.ArrayList(*CachedNode)) !void {
    const gpa = testing.allocator;
    const snap = n.snapshotLoad();
    defer snap.release();
    var cursor = key.KeyBuf.fromSlice(n.min_key);
    while (true) {
        var live = (try snap.value.nextLive(gpa, cursor.slice(), null)) orelse break;
        defer live.deinit();
        const iv = try key.InteriorValue.decode(live.value);
        const child = try t.cache.get(iv.childAddr());
        cursor = key.keySuccessor(child.max_key);
        try out.append(gpa, child);
    }
}

fn releaseAll(list: *std.ArrayList(*CachedNode)) void {
    for (list.items) |n| n.release();
    list.deinit(testing.allocator);
}

test "merge_bounds_derive_from_the_split_fill_target: ¾C pair cap, ¼C candidate bound, structural hysteresis, the floor is reserve/2" {
    for ([_]usize{ 64 * 1024, node.DEFAULT_NODE_SIZE, 1024 * 1024 }) |ns| {
        const layout = try node.NodeLayout.new(ns, null);
        const cap = layout.foldCapacity();
        try testing.expectEqual(layout.splitPartCapacity(), layout.mergePairCapacity());
        try testing.expectEqual(cap * 3 / 4, layout.mergePairCapacity());
        try testing.expectEqual(layout.splitPartCapacity() - cap / 2, layout.mergeCandidateCapacity());
        try testing.expectEqual(cap / 4, layout.mergeCandidateCapacity());
        try testing.expect(cap / 2 + 1 + layout.mergeCandidateCapacity() > layout.mergePairCapacity());
        try testing.expect(cap - layout.mergePairCapacity() >= layout.mergeCandidateCapacity());
        try testing.expect(!pairMergeable(layout, cap / 2 + 1, layout.mergeCandidateCapacity()));
        try testing.expect(pairMergeable(layout, layout.mergeCandidateCapacity(), layout.mergeCandidateCapacity() * 2));
    }
    for ([_]u64{ 64, 4096, 65_536 }) |total| {
        const reserve = alloc_core.compactionReserveExtents(total);
        try testing.expectEqual(reserve / 2, alloc_core.compactionFloorExtents(reserve));
    }
}

test "merge_sweep_budget_derives_from_the_checkpoint_tick_period: the budget IS the tick period; strict mode reads 100 ms" {
    try testing.expectEqual(@as(u64, 50), mergeSweepBudgetMs(50));
    try testing.expectEqual(@as(u64, 100), mergeSweepBudgetMs(0));
    try testing.expectEqual(@as(u64, 2000), mergeSweepBudgetMs(2000));
    for ([_]u64{ 0, 1, 50, 100, 250, 1000, 5000, 60_000 }) |ms| try testing.expectEqual(checkpoint.checkpointTickPeriodMs(ms), mergeSweepBudgetMs(ms));
    try testing.expectEqual(checkpoint.STRICT_TICK_PERIOD_MS, checkpoint.checkpointTickPeriodMs(0));
}

test "stale_writer_revalidates_and_retries_after_a_merge: two underfull leaves merge; the resolved leaf is superseded; applyAt answers stale and counts; both leaves' kept keys serve" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [128]u8 = undefined;
    var n: u64 = 0;
    while (try t.rootLevel() < 1) : (n += 1) {
        try t.insert(&ikey(n), tree.testValue(n, 64, &buf));
        if (n % 64 == 63) _ = try vol.drain(t);
        try testing.expect(n < 100_000);
    }
    try vol.flushDirty(t);
    const stale_leaf = try t.resolveLeaf(&ikey(5));
    defer stale_leaf.release();
    try testing.expectEqual(@as(u8, 0), stale_leaf.level);
    for (0..n) |i| if (i % 20 != 0) try t.delete(&ikey(i));
    const fx = try mergeToFixpoint(&vol, t);
    try testing.expect(fx.out.merges >= 1);
    try testing.expect(stale_leaf.state.isSuperseded());
    const r0 = tree.stats.meta_kv_commit_smo_retries.load();
    try testing.expectEqual(tree.ApplyOutcome.stale, try t.applyAt(stale_leaf, &ikey(5), .put, tree.testValue(4242, 64, &buf)));
    try testing.expectEqual(r0 + 1, tree.stats.meta_kv_commit_smo_retries.load());
    try t.insert(&ikey(5), tree.testValue(4242, 64, &buf));
    try expectLookup(t, &ikey(5), tree.testValue(4242, 64, &buf));
    var i: u64 = 0;
    while (i < n) : (i += 20) try expectLookup(t, &ikey(i), tree.testValue(i, 64, &buf));
    // A merge's successor is an image the context counts.
    try testing.expect(vol.ctx.imagesWritten() >= 1);
}

test "fold_equivalence_holds_across_a_merge_storm: three 90 %-delete rounds with reinserts; lookup and range equal a BTreeMap-shaped oracle; the K2 walk holds; leaves shrink" {
    const gpa = testing.allocator;
    const KEYS: u64 = 12_000;
    var vol = try TestVolume.init(gpa, .{ .extents = 512 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var shadow = std.AutoHashMap(u64, u64).init(gpa);
    defer shadow.deinit();
    var buf: [128]u8 = undefined;
    const perm = try permutation(gpa, KEYS, 7);
    defer gpa.free(perm);
    for (perm, 0..) |i, k| {
        try t.insert(&ikey(i), tree.testValue(i, 80, &buf));
        try shadow.put(i, i);
        if (k % 256 == 255) _ = try vol.drain(t);
    }
    try vol.flushDirty(t);
    try testing.expect(try t.rootLevel() >= 1);
    const w0 = try k2Walk(t);
    var total: MaintenanceOutcome = .{};
    for (0..3) |round| {
        var live: std.ArrayList(u64) = .empty;
        defer live.deinit(gpa);
        var it = shadow.keyIterator();
        while (it.next()) |k| try live.append(gpa, k.*);
        std.mem.sort(u64, live.items, {}, std.sort.asc(u64));
        const order = try permutation(gpa, live.items.len, 11 + round);
        defer gpa.free(order);
        for (order, 0..) |idx, k| {
            if (k % 10 == 0) continue;
            const i = live.items[@intCast(idx)];
            try t.delete(&ikey(i));
            _ = shadow.remove(i);
        }
        var i: u64 = round;
        while (i < KEYS) : (i += 3) {
            if (!shadow.contains(i) and i % 7 == 0) {
                const v = i * 1_000_003 + round;
                try t.insert(&ikey(i), tree.testValue(v, 80, &buf));
                try shadow.put(i, v);
            }
        }
        const fx = try mergeToFixpoint(&vol, t);
        total.add(fx.out);
        for (0..KEYS) |k| {
            if (shadow.get(k)) |v| try expectLookup(t, &ikey(k), tree.testValue(v, 80, &buf)) else try expectLookup(t, &ikey(k), null);
        }
        var all: std.ArrayList(KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &all);
        try t.range(gpa, &ikey(0), &ikey(KEYS), std.math.maxInt(usize), &all);
        try testing.expectEqual(shadow.count(), all.items.len);
        var prev: ?u64 = null;
        for (all.items) |kv| {
            const k = try key.decodeInodeKey(kv.key);
            if (prev) |p| try testing.expect(p < k);
            prev = k;
            try testing.expectEqualSlices(u8, tree.testValue(shadow.get(k).?, 80, &buf), kv.value);
        }
        _ = try k2Walk(t);
    }
    try testing.expect(total.merges > 0);
    const w1 = try k2Walk(t);
    try testing.expect(w1.leaves < w0.leaves);
}

test "height_three_tree_collapses_to_a_root_leaf_gap_free_with_readers_mid_walk: 98 % then 100 %-but-five deletes collapse height 2 → 0 under concurrent readers; regrowth splits again" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .extents = 2048 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    const collapses0 = stats.meta_kv_root_collapses.load();
    // 32-byte keys ⇒ 974 leaves per full interior (the smo.zig depth-3 shape).
    const n = try vol.growToRootLevel(t, 2, 32, 4096, 60_000);
    try vol.flushDirty(t);
    const w0 = try k2Walk(t);
    try testing.expectEqual(@as(u8, 2), w0.height);
    const vbuf = try gpa.alloc(u8, 4096);
    defer gpa.free(vbuf);
    // Concurrent latch-free readers over five keys that survive both
    // phases: they may restart (a root swap lowers the height mid-walk)
    // but never error and never see a wrong value.
    const Reader = struct {
        fn run(tr: *KvTree, stop: *std.atomic.Value(bool), failed: *std.atomic.Value(bool), served: *std.atomic.Value(u64), start: usize) void {
            var want: [4096]u8 = undefined;
            var i = start;
            while (!stop.load(.acquire)) : (i += 3) {
                const k: u64 = @as(u64, @intCast(i % 5)) * 50;
                var kb: [32]u8 = undefined;
                const got = tr.lookup(std.heap.page_allocator, smo.wideKey(k, 32, &kb)) catch {
                    failed.store(true, .release);
                    return;
                };
                defer if (got) |g| std.heap.page_allocator.free(g);
                if (got == null or !std.mem.eql(u8, got.?, tree.testValue(k, 4096, &want))) failed.store(true, .release);
                _ = served.fetchAdd(1, .monotonic);
                std.Thread.yield() catch {};
            }
        }
    };
    var stop: std.atomic.Value(bool) = .init(false);
    var failed: std.atomic.Value(bool) = .init(false);
    var served: std.atomic.Value(u64) = .init(0);
    var readers: [4]std.Thread = undefined;
    for (&readers, 0..) |*r, i| r.* = try std.Thread.spawn(.{}, Reader.run, .{ t, &stop, &failed, &served, i });
    // Phase A: keep every 50th key.
    for (0..n) |i| {
        if (i % 50 == 0) continue;
        var kb: [32]u8 = undefined;
        try t.delete(smo.wideKey(i, 32, &kb));
    }
    const a = try mergeToFixpoint(&vol, t);
    try testing.expect(a.out.merges > 0);
    const wa = try k2Walk(t);
    try testing.expect(wa.leaves < w0.leaves / 4);
    try testing.expect(wa.height < w0.height);
    // Phase B: down to the five kept keys (0, 50, 100, 150, 200).
    var i: u64 = 250;
    while (i < n) : (i += 50) {
        var kb: [32]u8 = undefined;
        try t.delete(smo.wideKey(i, 32, &kb));
    }
    const b = try mergeToFixpoint(&vol, t);
    stop.store(true, .release);
    for (readers) |r| r.join();
    try testing.expect(!failed.load(.acquire));
    try testing.expect(served.load(.monotonic) > 0);
    const wb = try k2Walk(t);
    try testing.expectEqual(@as(u64, 1), wb.leaves);
    try testing.expectEqual(@as(u8, 0), wb.height);
    try testing.expect(stats.meta_kv_root_collapses.load() - collapses0 >= 2);
    try testing.expect(a.out.root_collapses + b.out.root_collapses >= 2);
    for (0..5) |k| {
        var kb: [32]u8 = undefined;
        try expectLookup(t, smo.wideKey(@as(u64, k) * 50, 32, &kb), tree.testValue(@as(u64, k) * 50, 4096, vbuf));
    }
    // Still a tree: growth after the collapse splits again.
    for (100_000..100_200) |j| {
        var kb: [32]u8 = undefined;
        try t.insert(smo.wideKey(j, 32, &kb), tree.testValue(j, 4096, vbuf));
        if (j % 8 == 7) _ = try vol.drain(t);
    }
    try vol.flushDirty(t);
    const wc = try k2Walk(t);
    try testing.expect(wc.leaves > 1 and wc.height >= 1);
}

test "cross_parent_underfull_leaves_shrink_through_interior_merges_to_a_fixed_point: ≥ parents−1 interior merges, no stranded pair, height collapses, passes ≤ 3·h₀+1" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .extents = 4096 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    const im0 = stats.meta_kv_interior_merges.load();
    // Grow until the root is level 2 with ≥ 4 level-1 interiors (32-byte
    // keys ⇒ 974 leaves per full interior, ascending inserts ⇒ ~2,400
    // leaves; 6 KiB values: ~9 records per full leaf, two survivors sit
    // under the ¼C bound, three do not pair with three).
    const VAL: usize = 6 * 1024;
    const vbuf = try gpa.alloc(u8, VAL);
    defer gpa.free(vbuf);
    var n: u64 = 0;
    while (true) : (n += 1) {
        var kb: [32]u8 = undefined;
        try t.insert(smo.wideKey(n, 32, &kb), tree.testValue(n, VAL, vbuf));
        if (n % 8 == 7) {
            _ = try vol.drain(t);
            vol.coverEverything();
        }
        if (try t.rootLevel() == 2) {
            const root = try t.cache.get(t.rootPtr().addr);
            defer root.release();
            var kids: std.ArrayList(*CachedNode) = .empty;
            defer releaseAll(&kids);
            try childrenOf(t, root, &kids);
            if (kids.items.len >= 4) break;
        }
        try testing.expect(n < 80_000);
    }
    n += 1;
    try vol.flushDirty(t);
    const w0 = try k2Walk(t);
    try testing.expectEqual(@as(u8, 2), w0.height);
    // Per level-1 interior keep the first record of its leftmost and of its
    // rightmost leaf; delete everything else.
    var keep = std.AutoHashMap(u64, void).init(gpa);
    defer keep.deinit();
    const parents0 = blk: {
        const root = try t.cache.get(t.rootPtr().addr);
        defer root.release();
        var parents: std.ArrayList(*CachedNode) = .empty;
        defer releaseAll(&parents);
        try childrenOf(t, root, &parents);
        for (parents.items) |p| {
            var leaves: std.ArrayList(*CachedNode) = .empty;
            defer releaseAll(&leaves);
            try childrenOf(t, p, &leaves);
            for ([_]*CachedNode{ leaves.items[0], leaves.items[leaves.items.len - 1] }) |leaf| {
                var first: std.ArrayList(KeyValue) = .empty;
                defer tree.deinitKeyValues(gpa, &first);
                try t.range(gpa, leaf.min_key, leaf.max_key, 1, &first);
                try testing.expectEqual(@as(usize, 1), first.items.len);
                try keep.put(std.mem.readInt(u64, first.items[0].key[24..][0..8], .big), {});
            }
        }
        break :blk parents.items.len;
    };
    for (0..n) |i| {
        if (keep.contains(i)) continue;
        var kb: [32]u8 = undefined;
        try t.delete(smo.wideKey(i, 32, &kb));
    }
    const fx = try mergeToFixpoint(&vol, t);
    try testing.expect(fx.out.interior_merges >= parents0 - 1);
    try testing.expect(stats.meta_kv_interior_merges.load() - im0 >= parents0 - 1);
    const bound: u32 = 3 * @as(u32, w0.height) + 1;
    try testing.expect(fx.passes <= bound);
    const fp = try mergeFixedPoint(t);
    try testing.expectEqual(@as(u64, 0), fp.stranded);
    try testing.expectEqual(@as(u64, 0), fp.heavy);
    const w1 = try k2Walk(t);
    try testing.expect(w1.leaves < w0.leaves / 8);
    try testing.expectEqual(@as(u8, if (w1.leaves > 1) 1 else 0), w1.height);
    try testing.expect(w1.height < w0.height);
    var kit = keep.keyIterator();
    while (kit.next()) |k| {
        var kb: [32]u8 = undefined;
        try expectLookup(t, smo.wideKey(k.*, 32, &kb), tree.testValue(k.*, VAL, vbuf));
    }
    var all: std.ArrayList(KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &all);
    var lo: [32]u8 = undefined;
    var hi: [32]u8 = undefined;
    try t.range(gpa, smo.wideKey(0, 32, &lo), smo.wideKey(n, 32, &hi), std.math.maxInt(usize), &all);
    try testing.expectEqual(keep.count(), all.items.len);
}

test "merge_sweep_is_bounded_per_call_and_resumes_from_its_cursor: an expired deadline runs exactly the progress minimum; bounded laps reach the unbounded fixpoint; the published count is the census" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .extents = 512 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [128]u8 = undefined;
    const perm = try permutation(gpa, 6000, 3);
    defer gpa.free(perm);
    for (perm, 0..) |i, k| {
        try t.insert(&ikey(i), tree.testValue(i, 80, &buf));
        if (k % 256 == 255) _ = try vol.drain(t);
    }
    for (0..6000) |i| if (i % 10 != 0) try t.delete(&ikey(i));
    try vol.flushDirty(t);
    vol.coverEverything();
    const w0 = try k2Walk(t);
    try testing.expect(w0.leaves >= 4);
    var calls: u32 = 0;
    var merges: u64 = 0;
    while (true) {
        const sweep = try mergeUnderfull(t, &vol.ctx, false, clock.monotonicNs());
        calls += 1;
        merges += sweep.outcome.merges + sweep.outcome.root_collapses;
        try testing.expect(sweep.projections >= 1);
        if (sweep.lap_complete) break;
        try testing.expect(sweep.projections <= 2);
        try testing.expect(calls < 10_000);
    }
    try testing.expect(calls > 2);
    try testing.expect(merges > 0);
    _ = try mergeToFixpoint(&vol, t);
    _ = try mergeFixedPoint(t);
    const last = try mergeUnderfull(t, &vol.ctx, false, null);
    try testing.expect(last.lap_complete);
    try testing.expectEqual(@as(u64, 0), last.outcome.merges + last.outcome.root_collapses);
    try testing.expectEqual(try mergeCandidateCensus(t), last.candidates);
    try testing.expectEqual(last.candidates, stats.meta_kv_merge_candidates.load());
}

test "a_merge_is_admitted_at_the_compaction_floor_and_returns_extents: the merge claims down to reserve/2 and refuses NoSpace below it; the sweep reports space_refused and keeps counting" {
    const gpa = testing.allocator;
    // 64 extents, reserve 8 ⇒ floor 4.
    var vol = try TestVolume.init(gpa, .{ .extents = 64, .reserve = 8 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [128]u8 = undefined;
    var n: u64 = 0;
    while (try t.rootLevel() < 1) : (n += 1) {
        try t.insert(&ikey(n), tree.testValue(n, 64, &buf));
        if (n % 64 == 63) _ = try vol.drain(t);
    }
    try vol.flushDirty(t);
    vol.coverEverything();
    for (0..n) |i| if (i % 20 != 0) try t.delete(&ikey(i));
    try vol.flushDirty(t);
    vol.coverEverything();
    // Drain the heap to exactly the floor: claimable − 1 < floor refuses.
    var held: std.ArrayList(u64) = .empty;
    defer {
        for (held.items) |e| vol.alloc.releaseUnpublished(e);
        held.deinit(gpa);
    }
    while (vol.alloc.freeExtents() > alloc_core.compactionFloorExtents(8)) try held.append(gpa, try vol.alloc.claimInternal());
    const sweep = try mergeUnderfull(t, &vol.ctx, false, null);
    try testing.expect(sweep.space_refused);
    try testing.expect(sweep.lap_complete);
    try testing.expectEqual(@as(u64, 0), sweep.outcome.merges);
    try testing.expectEqual(try mergeCandidateCensus(t), sweep.candidates);
    try testing.expect(sweep.candidates >= 2);
    // One extent above the floor admits the merge (net −1: both old extents
    // park, then return on coverage).
    vol.alloc.releaseUnpublished(held.pop().?);
    const free0 = vol.alloc.freeExtents();
    const sweep2 = try mergeUnderfull(t, &vol.ctx, false, null);
    try testing.expect(sweep2.outcome.merges >= 1);
    try testing.expectEqual(free0 - 1, vol.alloc.freeExtents());
    vol.coverEverything();
    try testing.expect(vol.alloc.freeExtents() > free0);
}

test "replay_converges_across_crash_windows_of_a_merge_and_a_root_collapse: the merge entry replays through the final routing; the collapse replays through the old root under the coverage gate" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .journaled = true });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [128]u8 = undefined;
    var n: u64 = 0;
    while (try t.rootLevel() < 1) : (n += 1) {
        try vol.commit(t, &ikey(n), .put, tree.testValue(n, 64, &buf));
        if (n % 64 == 63) _ = try vol.drain(t);
    }
    _ = try vol.checkpoint();
    const led = try vol.checkpoint();
    const root_at_ledger = t.rootPtr();
    for (0..n) |i| if (i % 20 != 0) try vol.commit(t, &ikey(i), .delete, &.{});
    // The merge alone (no climb — the sweep would collapse the root in the
    // same lap): the tombstones sit above the tail, but the pair is
    // underfull on size alone.
    try vol.flushDirty(t);
    var o: MaintenanceOutcome = .{};
    {
        const left = try t.resolveLeaf(&ikey(0));
        defer left.release();
        const succ = (try mergeWithSibling(t, &vol.ctx, left, &o, false)) orelse return error.TestExpectedMerge;
        succ.release();
    }
    try testing.expectEqual(@as(u64, 1), o.merges);
    const merged_root = t.rootPtr();
    try testing.expectEqual(root_at_ledger, merged_root);
    const merge_entry = vol.journal.last_entry.?;
    // Window A: crash after the merge entry, before the collapse.
    {
        var re = try vol.crashReplay(led);
        defer re.deinit();
        try testing.expect(re.outcome.interior_applied >= 2); // Put + Delete
        const t2 = re.treeOf(.native);
        try testing.expectEqual(root_at_ledger, t2.rootPtr());
        var i: u64 = 0;
        while (i < n) : (i += 1) {
            if (i % 20 == 0) try expectLookup(t2, &ikey(i), tree.testValue(i, 64, &buf)) else try expectLookup(t2, &ikey(i), null);
        }
        _ = try k2Walk(t2);
    }
    // The collapse: a level-1 root over ONE leaf.
    const root_node = try vol.cache.get(merged_root.addr);
    defer root_node.release();
    try testing.expect(try smoRootCollapse(t, &vol.ctx, root_node, &o, false));
    try testing.expectEqual(@as(u8, 0), try t.rootLevel());
    const collapse_entry = vol.journal.last_entry.?;
    try testing.expect(collapse_entry.start > merge_entry.start);
    try testing.expectEqual(collapse_entry.start, t.rootFloor());
    // Window B: crash after the collapse — the old ledger still names the
    // level-1 root; replay walks the old structure to the same leaf.
    {
        var re = try vol.crashReplay(led);
        defer re.deinit();
        const t2 = re.treeOf(.native);
        try testing.expectEqual(root_at_ledger, t2.rootPtr());
        try testing.expectEqual(@as(u8, 1), try t2.rootLevel());
        var i: u64 = 0;
        while (i < n) : (i += 1) {
            if (i % 20 == 0) try expectLookup(t2, &ikey(i), tree.testValue(i, 64, &buf)) else try expectLookup(t2, &ikey(i), null);
        }
        // The old root's free is in the window: parked until coverage.
        try testing.expect(vol.alloc.pendingCount() >= 1);
    }
    // A ledger AFTER the collapse names the leaf root; replay is clean.
    const led2 = try vol.checkpoint();
    try testing.expectEqual(t.rootPtr().addr, led2.roots[0].node_addr);
    var re = try vol.crashReplay(led2);
    defer re.deinit();
    try testing.expectEqual(@as(u8, 0), try re.treeOf(.native).rootLevel());
    var i: u64 = 0;
    while (i < n) : (i += 20) try expectLookup(re.treeOf(.native), &ikey(i), tree.testValue(i, 64, &buf));
}

test "merge_after_flush prefilter: a leaf whose flush-step compaction leaves a tiny log merges with its sibling; a tombstone-heavy full log is not a prefilter candidate; a space retry in flight yields and counts" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [128]u8 = undefined;
    var n: u64 = 0;
    while (try t.rootLevel() < 1) : (n += 1) {
        try t.insert(&ikey(n), tree.testValue(n, 64, &buf));
        if (n % 64 == 63) _ = try vol.drain(t);
    }
    try vol.flushDirty(t);
    vol.coverEverything();
    const left0 = try t.resolveLeaf(&ikey(0));
    defer left0.release();
    const half = try key.decodeInodeKey(left0.max_key);
    // Delete 95 % of the LEFT leaf (the right stays full); cover so the
    // tombstones elide at the next fold.
    for (0..half + 1) |i| if (i % 20 != 0) try t.delete(&ikey(i));
    try vol.flushDirty(t);
    vol.coverEverything();
    const m0 = stats.meta_kv_node_merges.load();
    // The tombstone-heavy full log is NOT a prefilter candidate (sound but
    // incomplete): its serialized bytes exceed ¼C.
    try mergeAfterFlush(t, &vol.ctx, left0.min_key);
    try testing.expectEqual(m0, stats.meta_kv_node_merges.load());
    // Churn the survivors, flushing every round, until the NEXT round's
    // frame cannot land; that round then makes the flush step COMPACT the
    // leaf, and the successor's log IS its fold, under ¼C.
    const survivors = half / 20 + 1;
    const round_bytes: usize = @intCast(survivors * (rec.RECORD_HEADER_LEN + key.INODE_KEY_LEN + 64));
    var spins: u64 = 0;
    while (true) : (spins += 1) {
        var i: u64 = 0;
        while (i <= half) : (i += 20) try t.insert(&ikey(i), tree.testValue(i + spins, 64, &buf));
        const leaf = try t.resolveLeaf(&ikey(0));
        defer leaf.release();
        const tail = blk: {
            const d = leaf.lockRead(testing.io);
            defer leaf.unlockRead(testing.io);
            break :blk d.tailOffset();
        };
        if (tail + vol.layout.appendFrameLen(round_bytes) > vol.layout.node_size) break;
        try vol.flushDirty(t);
        try testing.expect(spins < 10_000);
    }
    // Under a space retry the prefilter yields (counted) and merges nothing.
    var hold: heap_law.SpaceRetryHold = .{};
    vol.ctx.space_retry = &hold;
    hold.begin();
    const y0 = heap_law.stats.meta_kv_merge_yields.load();
    const c0 = smo.stats.meta_kv_node_compactions.load();
    try smo.checkpointFlushNode(t, &vol.ctx, (try leafAddr(t, &ikey(0))));
    try testing.expectEqual(c0 + 1, smo.stats.meta_kv_node_compactions.load());
    try testing.expect(heap_law.stats.meta_kv_merge_yields.load() > y0);
    try testing.expectEqual(m0, stats.meta_kv_node_merges.load());
    hold.end();
    // Without the hold: dirty the successor and flush it — the prefilter
    // proves it a candidate and merges the pair.
    try t.insert(&ikey(0), tree.testValue(777, 64, &buf));
    try smo.checkpointFlushNode(t, &vol.ctx, (try leafAddr(t, &ikey(0))));
    try testing.expectEqual(m0 + 1, stats.meta_kv_node_merges.load());
    try expectLookup(t, &ikey(0), tree.testValue(777, 64, &buf));
    var i: u64 = 20;
    while (i <= half) : (i += 20) try expectLookup(t, &ikey(i), tree.testValue(i + spins, 64, &buf));
    i = half + 1;
    while (i < n) : (i += 1) try expectLookup(t, &ikey(i), tree.testValue(i, 64, &buf));
}

fn leafAddr(t: *KvTree, k: []const u8) !u64 {
    const leaf = try t.resolveLeaf(k);
    defer leaf.release();
    return leaf.addr;
}
