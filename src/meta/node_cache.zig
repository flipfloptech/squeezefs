//! The RAM-authoritative node cache (charter §5.3 "Node cache" and
//! "Writeback", §5.1 "Node", §2 lock order 4b; survey
//! `docs/porting/kv_tree_cache_commit_survey.md` §2.8–§2.9, §3.3, §3.8, §4,
//! §5.4, §7.1; the reference's `node_cache.rs` `NodeDirty`/`CachedNode`/
//! `NodeCache`).
//!
//! - `CachedNode` = an arc-swapped immutable `NodeSnapshot` (readers
//!   latch-free: one `ArcSwap.load`, binary search, slice views) + the dirty
//!   delta under the per-node write lock — lock order 4b, RAM mutation
//!   only, NEVER held across device I/O or a ring-space wait. The lifecycle
//!   word (`node_state_core`) decides every race the lock does not.
//! - `applyLocked` is THE RAM-mutation choke point: the partitioning gate
//!   (reader ⇒ refuse; interior ∧ ¬authority ⇒ refuse; the slot-lease
//!   verdict), `markDirty`, the dirty-floor stamp, the D7.a head
//!   materialization, the inversion guard, the bounded tail publish.
//! - `freezeLocked` freezes the open delta under the lock into a 4 KiB-padded
//!   bset image whose records join the snapshot's base index incrementally;
//!   `appendFrozen` writes it OUTSIDE the lock through the `NodeLoader`.
//!   Every error after `beginFreeze` restores the word (the W-A law); a
//!   latched word with no frozen delta is healed loud (RES-22).
//! - `NodeCache`: demand paging, single-flight per address with the
//!   pre-read epoch snapshot, positional torn classification through
//!   `node.verifyNodeExtentScreened`, clock eviction with the strong-count
//!   gate and the pinned classes (dirty / serializing by the lifecycle
//!   CAS, interior and roots unconditionally), the per-volume budget
//!   `max(mem_budget/16, 512 MiB)` registered as a sheddable `mem_budget`
//!   component.
//!
//! ## The map
//!
//! A sharded COPY-ON-WRITE map: each shard is an immutable
//! `AutoHashMap(addr → *CachedNode)` behind an `ArcSwap`; a read is one
//! arc-swap load + one hash probe + one retain — latch-free, never behind a
//! writer. Inserts and removals take the shard's `std.Io.Mutex` (the
//! sanctioned metadata-transaction class), clone the shard, mutate the
//! clone and publish it; readers of the previous shard keep every node it
//! named alive by refcount. Why not a mutex-guarded map: the traversal takes
//! one hit per tree level per op and the charter's law is latch-free hot
//! paths. Why not one COW map: the clone is O(shard) per publish and a
//! publish happens per miss; the shard count is derived from the budget so
//! a shard rewrite stays a ~64-pointer copy against a ~100 µs device read.
//!
//! Byte images travel through a `NodeLoader` the device binding implements;
//! `RamLoader` is the in-RAM volume the contracts run over.

const std = @import("std");
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const mem_budget = @import("../mem/budget.zig");
const key = @import("key.zig");
const rec = @import("record.zig");
const bset = @import("bset.zig");
const node = @import("node.zig");
const node_state_core = @import("node_state_core.zig");
const epoch_core = @import("epoch_core.zig");
const journal_core = @import("journal_core.zig");
const alloc_ext = @import("alloc_ext.zig");
const snapshot = @import("node_snapshot.zig");
const arc_swap = @import("../sync/arc_swap.zig");
const sync = @import("../sync/sync.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
const Io = std.Io;
const MiB = derive.MiB;

pub const Arc = arc_swap.Arc;
pub const ArcSwap = arc_swap.ArcSwap;
pub const Bytes = snapshot.Bytes;
pub const SharedBuf = snapshot.SharedBuf;
pub const OwnedRec = snapshot.OwnedRec;
pub const Head = snapshot.Head;
pub const NodeSnapshot = snapshot.NodeSnapshot;
pub const RecordIndex = snapshot.RecordIndex;
pub const Run = snapshot.Run;
pub const NodeState = node_state_core.NodeState;
pub const NodeLayout = node.NodeLayout;
pub const SeqSpan = journal_core.SeqSpan;
pub const ForestSlot = key.ForestSlot;
pub const RecordRef = rec.RecordRef;
pub const RecordKind = rec.RecordKind;

// ---------------------------------------------------------------------------
// Constants and derivations, each with its WHY.
// ---------------------------------------------------------------------------

/// The shipped node-cache budget: since the derivation sweep the FLOOR of
/// the derived default `max(mem_budget/16, 512 MiB)` — never regress below
/// what every box ran (charter §1.5 "Floors are legitimate only as ...
/// never regress below what shipped"). ~2,048 nodes at 256 KiB ≈ 4–8 M hot
/// records.
pub const DEFAULT_CACHE_BUDGET_BYTES: u64 = 512 * MiB;
/// The budget's share of the R5 memory budget: 1/16, one of the fixed
/// shares the sweep partitioned the budget into (the transport and IPC
/// caps take 1/8 each).
pub const CACHE_BUDGET_SHARE_DIVISOR: u64 = 16;
/// Writeback threshold: freeze/append once a node's open delta exceeds a
/// bset worth — one 4 KiB append page (§4.6 pt 1).
pub const DEFAULT_WRITEBACK_DELTA_BYTES: usize = bset.NODE_PAGE;
/// The `mem_budget` component floor the reference registers for
/// `kv_node_cache` (its production registry row: 64 MiB, weight 1): the
/// working set below which a shed would only reload-thrash.
pub const KV_NODE_CACHE_FLOOR_BYTES: u64 = 64 * MiB;
/// Weight 1: the cache sheds in proportion with the other RAM-authoritative
/// tables, never ahead of the data tiers (weights 2–4).
pub const KV_NODE_CACHE_SHED_WEIGHT: u64 = 1;
/// Concurrent cold loads a cache serves at once: one leader per missing
/// address, bounded by the threads that can issue a miss; `4 ×` the
/// sharded-counter stripe cap covers 4 outstanding loads per stripe thread.
/// A full table is a counted refusal (`error.FlightTableFull`) — the
/// caller's retry class, never an allocation.
pub const MAX_INFLIGHT_LOADS: usize = 4 * counters.max_stripes;
/// Nodes per map shard the shard count targets: a shard rewrite (one clone
/// + ≤ 64 retains ≈ 1 KiB) stays negligible against the ~100 µs device
/// read every publish follows.
pub const SHARD_TARGET_NODES: u64 = 64;
/// Shard-count floor: parallel loaders on a small cache rarely serialize on
/// one shard mutex.
pub const SHARD_COUNT_FLOOR: u64 = 16;
/// Shard-count cap: 4,096 slots ≈ 16 KiB of slots at the largest budgets.
pub const SHARD_COUNT_CAP: u64 = 4096;

/// `clamp(next_pow2(ceil(budget_nodes / 64)), 16, 4096)`.
pub fn shardCount(budget_nodes: u64) usize {
    return @intCast(derive.clamp(derive.nextPow2(derive.ceilDiv(@max(budget_nodes, 1), SHARD_TARGET_NODES)), SHARD_COUNT_FLOOR, SHARD_COUNT_CAP));
}

/// `SQUEEZEFS_META_NODE_CACHE_MB` (MiB, verbatim) > `_PCT` (percent of the
/// R5 budget) > `max(budget/16, 512 MiB)`. Pure; never refuses (the knob
/// gate already did).
pub fn resolveNodeCacheBudget(budget_bytes: u64, mb: ?u64, pct: ?u64) u64 {
    if (mb) |m| return m *| MiB;
    if (pct) |p| return @intCast(@as(u128, budget_bytes) * p / 100);
    return @max(budget_bytes / CACHE_BUDGET_SHARE_DIVISOR, DEFAULT_CACHE_BUDGET_BYTES);
}

/// The registry face of `resolveNodeCacheBudget`.
pub fn nodeCacheBudgetFromEnv(env: *const std.process.Environ.Map, budget_bytes: u64) u64 {
    return resolveNodeCacheBudget(
        budget_bytes,
        knobs.optIntKnob(u64, env, "SQUEEZEFS_META_NODE_CACHE_MB"),
        knobs.optIntKnob(u64, env, "SQUEEZEFS_META_NODE_CACHE_PCT"),
    );
}

// ---------------------------------------------------------------------------
// Observability (survey §5.4 names).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var meta_kv_node_cache_hits: counters.ShardedCounter = .{};
    pub var meta_kv_node_cache_misses: counters.ShardedCounter = .{};
    pub var meta_kv_node_cache_evictions: counters.ShardedCounter = .{};
    /// Extent reads issued (misses plus reader re-reads).
    pub var meta_kv_node_cache_loads: counters.ShardedCounter = .{};
    /// Overlay records shadowed at freeze (designed high under same-key storms).
    pub var meta_kv_node_freeze_shadow_dropped: counters.ShardedCounter = .{};
    /// A flush appended a parked frozen delta and KEPT the open records' floor.
    pub var meta_kv_flush_floor_kept: counters.ShardedCounter = .{};
    /// Reader-side: stale-stamped hits refused / mappings dropped / re-reads.
    pub var meta_kv_revalidate_stale_serves: counters.ShardedCounter = .{};
    pub var meta_kv_revalidate_nodes_dropped: counters.ShardedCounter = .{};
    pub var meta_kv_reader_load_retries: counters.ShardedCounter = .{};
};

/// A reader or a non-authority mutated a node population it does not own,
/// or a foreign append was detected at our tail.
pub var partition_refusals: counters.Tripwire = .init("meta_kv_node_partition_refusals");
/// A leaf of a slot this mount does not lease (the symmetric plane's third
/// gate state).
pub var leaf_lease_refusals: counters.Tripwire = .init("meta_kv_leaf_lease_refusals");
/// FREEZING latched with no frozen delta — a third unknown window (RES-22).
pub var freeze_latched_recovered: counters.Tripwire = .init("kv_freeze_latched_recovered");
/// A revalidation drop pass met un-durable RAM records: revalidation was
/// armed on a mount that writes.
pub var revalidate_dirty_skips: counters.Tripwire = .init("meta_kv_revalidate_dirty_skips");

/// Test seam: how many `freezeLocked` serializations to FAIL right after
/// `beginFreeze` succeeds — the W-A window. Each engaged freeze decrements
/// it. One monotonic load per freeze when unarmed.
pub var test_freeze_encode_fail: std.atomic.Value(u64) = .init(0);

// ---------------------------------------------------------------------------
// The loader: byte images in and out.
// ---------------------------------------------------------------------------

pub const LoaderError = error{Io};

/// The device binding: one extent read, one tail-page append, one whole
/// fresh-image write. All are synchronous from the caller's view (the
/// binding parks the task in its own ring).
pub const NodeLoader = struct {
    ctx: *anyopaque,
    /// Fill `buf` (`node_size` bytes, `NODE_PAGE`-aligned) from `addr`.
    readExtent: *const fn (ctx: *anyopaque, addr: u64, buf: []u8) LoaderError!void,
    /// Write `bytes` (a 4 KiB-padded frame) at `addr + off`.
    appendBset: *const fn (ctx: *anyopaque, addr: u64, off: usize, bytes: []const u8) LoaderError!void,
    /// Land a fresh node image (`node.WrittenNode.image`: header page ‖
    /// base frame) whole at `addr` — an SMO successor, a tree's first root.
    /// Always a FRESH extent (CoW law §5.1), never an in-place rewrite of
    /// a mapped one; the SMO task is its only caller.
    writeImage: *const fn (ctx: *anyopaque, addr: u64, bytes: []const u8) LoaderError!void,
};

/// An in-RAM volume: the contracts' device.
pub const RamLoader = struct {
    bytes: []u8,
    reads: std.atomic.Value(u64) = .init(0),
    appends: std.atomic.Value(u64) = .init(0),
    /// Fresh node images landed through `writeImage`.
    images: std.atomic.Value(u64) = .init(0),
    /// Test seam: every append fails `error.Io`.
    fail_appends: std.atomic.Value(bool) = .init(false),

    pub fn init(gpa: Allocator, len: usize) Allocator.Error!RamLoader {
        const b = try gpa.alloc(u8, len);
        @memset(b, 0);
        return .{ .bytes = b };
    }

    pub fn deinit(self: *RamLoader, gpa: Allocator) void {
        gpa.free(self.bytes);
        self.* = undefined;
    }

    pub fn loader(self: *RamLoader) NodeLoader {
        return .{ .ctx = self, .readExtent = readExtent, .appendBset = appendBset, .writeImage = writeImage };
    }

    fn writeImage(ctx: *anyopaque, addr: u64, bytes: []const u8) LoaderError!void {
        const self: *RamLoader = @ptrCast(@alignCast(ctx));
        _ = self.images.fetchAdd(1, .monotonic);
        self.write(addr, bytes);
    }

    fn readExtent(ctx: *anyopaque, addr: u64, buf: []u8) LoaderError!void {
        const self: *RamLoader = @ptrCast(@alignCast(ctx));
        _ = self.reads.fetchAdd(1, .monotonic);
        @memcpy(buf, self.bytes[@intCast(addr)..][0..buf.len]);
    }

    fn appendBset(ctx: *anyopaque, addr: u64, off: usize, bytes: []const u8) LoaderError!void {
        const self: *RamLoader = @ptrCast(@alignCast(ctx));
        if (self.fail_appends.load(.monotonic)) return error.Io;
        _ = self.appends.fetchAdd(1, .monotonic);
        self.write(addr + off, bytes);
    }

    /// Land an image (a `node.writeNode` output, a hand-made frame).
    pub fn write(self: *RamLoader, offset: u64, data: []const u8) void {
        @memcpy(self.bytes[@intCast(offset)..][0..data.len], data);
    }
};

// ---------------------------------------------------------------------------
// The slot-lease gate (the symmetric plane's third gate state) — a hook
// the lease plane installs; unarmed on every flat volume.
// ---------------------------------------------------------------------------

pub const LeaseVerdict = enum { unarmed, allowed, not_leased, releasing };

pub const LeaseGate = struct {
    ctx: ?*anyopaque,
    /// `(slot, structural)`: the commit verdict, or the structural one for
    /// the SMO task's own moves.
    verdict: *const fn (ctx: ?*anyopaque, slot: ForestSlot, structural: bool) LeaseVerdict,

    pub const unarmed: LeaseGate = .{ .ctx = null, .verdict = alwaysUnarmed };

    fn alwaysUnarmed(_: ?*anyopaque, _: ForestSlot, _: bool) LeaseVerdict {
        return .unarmed;
    }
};

/// A callback with context (the checkpoint kick the budget shed drives).
pub const Hook = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque) void,
};

/// Placement + policy for one volume's node cache.
pub const Config = struct {
    layout: NodeLayout,
    /// Byte offset of heap extent 0.
    heap_base: u64,
    /// Cache budget in bytes; each cached node charges one extent plus its
    /// overlay, heads and memo bytes.
    budget_bytes: u64,
    /// Open-delta size that triggers a writeback enqueue.
    writeback_delta_bytes: usize = DEFAULT_WRITEBACK_DELTA_BYTES,
};

/// The unstamped forest slot (a flat volume's every node).
pub const no_forest_slot: u32 = std.math.maxInt(u32);
/// `dirty_floor` of a node holding no un-durable record.
pub const no_floor: u64 = std.math.maxInt(u64);

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// The choke point's typed refusals.
pub const ApplyError = error{
    /// This mount armed reader revalidation and may not write the volume
    /// it is reading.
    ReaderMayNotMutate,
    /// A structural mutation by an appender that is not the root authority.
    NonAuthorityInterior,
    /// A leaf of a slot this mount does not lease (or mid-handover).
    LeafLeaseRefused,
    /// The node was superseded first: nothing accepted, re-resolve.
    Superseded,
    OutOfMemory,
};

/// `freezeLocked`'s classes: the three protocol refusals (typed apart from
/// corruption — the 2026-08-19 field hunt chased phantom device corruption
/// on a refusal text), the seam, and the encoder's.
pub const FreezeError = error{
    FreezeRefusedNotDirty,
    FreezeRefusedAlreadyFreezing,
    FreezeRefusedSuperseded,
    InjectedFreezeFailure,
    ValueTooLarge,
    Corrupt,
    OutOfMemory,
};

pub const LoadError = node.Error || LoaderError || error{ FlightTableFull, Canceled };
/// `getChild`: the pointer's `child_node_seq` does not match the loaded
/// image — a concurrent SMO; reload the parent and retry (bounded).
pub const ChildError = LoadError || error{ ReloadParent, RetiredExtent };
pub const AppendError = node.Error || LoaderError || error{ ForeignAppend, ForeignFrameOverwrite };

/// `CachedNode.flushFreezeLocked`'s answer.
pub const FlushFreeze = struct { floor: u64, frozen: bool };

/// Traversal restarts are bounded by `node.RETRY_BUDGET`; exhaustion is a
/// routing bug, surfaced typed.
pub const RetryBudget = struct {
    left: u32 = node.RETRY_BUDGET,

    pub fn spend(self: *RetryBudget) error{RetryBudgetExhausted}!void {
        if (self.left == 0) return error.RetryBudgetExhausted;
        self.left -= 1;
    }
};

// ---------------------------------------------------------------------------
// The dirty half.
// ---------------------------------------------------------------------------

/// A frozen-but-unwritten delta: the records (the append / compact input)
/// whose bset image is already merged into the snapshot base.
pub const FrozenDelta = struct {
    records: []rec.Record,
    /// Max record seq — the bset `journal_seq_horizon`.
    horizon: u64,
    /// Encoded padded frame length (the fit check before the append).
    frame_len: usize,

    pub fn deinit(self: *FrozenDelta, gpa: Allocator) void {
        rec.deinitRecords(gpa, self.records);
        gpa.free(self.records);
        self.* = undefined;
    }

    /// Encoded bytes of the records.
    pub fn bytes(self: *const FrozenDelta) usize {
        var n: usize = 0;
        for (self.records) |*r| n += r.ref().encodedLen();
        return n;
    }

    pub fn refs(self: *const FrozenDelta, gpa: Allocator) Allocator.Error![]RecordRef {
        const out = try gpa.alloc(RecordRef, self.records.len);
        for (self.records, out) |*r, *o| o.* = r.ref();
        return out;
    }
};

/// The mutable half of a node, guarded by the per-node write lock (RAM
/// mutation only — the append runs outside, on the serialized
/// writeback/SMO task).
pub const NodeDirty = struct {
    /// Open-delta records, `(key asc, seq asc)` — the AUTHORITATIVE merged
    /// view (freeze, SMO take-over and rollback operate on it).
    overlay: std.ArrayListUnmanaged(OwnedRec) = .empty,
    /// Encoded size of `overlay` (writeback threshold input).
    overlay_bytes: usize = 0,
    /// Bytes of materialized heads riding `overlay` records.
    head_bytes: usize = 0,
    /// Publish mirrors of `overlay`: `merge(snap_stable, snap_tail) ==
    /// overlay` at every publish point; `snap_stable` is shared with the
    /// published snapshots, `snap_tail` holds the ≤ `OVERLAY_TAIL_MAX`
    /// records applied since the last re-merge.
    snap_stable: *Arc(Run),
    snap_tail: std.ArrayListUnmanaged(OwnedRec) = .empty,
    /// Highest record seq ever applied — the run-precedence guard: an
    /// apply at-or-below it (mount replay's original seqs) forces a stable
    /// re-merge, keeping "tail ≥ stable per key" true everywhere else.
    max_applied_seq: u64 = 0,
    frozen: ?FrozenDelta = null,
    /// Node-relative offset of the unwritten tail (advances per append).
    tail_offset: usize,
    /// What this open delta currently charges the budget gauge
    /// (`overlay_bytes + head_bytes` as of the last `resyncCharge`).
    charged: u64 = 0,
    charge: *counters.Gauge,
    /// §4.7 heap admission: extents PROMISED to the SMO this node's pending
    /// bytes will need at the flush (0 = they append in place). Granted
    /// under the node write lock by the commit pass; consumed by the SMO
    /// that replaces the node (`takeOverlay`) or released when an in-place
    /// append leaves the remainder fitting; released on deinit so a dying
    /// object never strands ledger budget.
    promised: u64 = 0,
    /// The fold estimate `promised` was computed for, and the bytes admitted
    /// since — the cheap re-check below the layout's growth window.
    promise_basis: usize = 0,
    promise_added: usize = 0,
    /// The volume's ledger (`NodeCache.heap_promised`); every move mirrors
    /// into the process-wide `alloc_ext.meta_kv_heap_promised` so the
    /// conveyor's `heap_law.HeapLedger` adds and the SMO task's releases
    /// here read as one population.
    heap_promised: *counters.Gauge,

    pub fn overlayBytes(self: *const NodeDirty) usize {
        return self.overlay_bytes;
    }

    pub fn overlayLen(self: *const NodeDirty) usize {
        return self.overlay.items.len;
    }

    /// The not-yet-durable floor of the records still in the OPEN delta —
    /// `min(entry_floor, seq)` over them — null when empty. The flush step
    /// keeps this on the node when it appends a pre-existing frozen delta
    /// the open records are not part of.
    pub fn overlayFloor(self: *const NodeDirty) ?u64 {
        var floor: ?u64 = null;
        for (self.overlay.items) |*r| {
            const f = @min(r.entry_floor, r.seq);
            floor = if (floor) |x| @min(x, f) else f;
        }
        return floor;
    }

    pub fn hasFrozen(self: *const NodeDirty) bool {
        return self.frozen != null;
    }

    /// The frozen-but-unwritten records (the SMO's `extra` fold input);
    /// empty when no freeze is outstanding.
    pub fn frozenRecords(self: *const NodeDirty) []const rec.Record {
        return if (self.frozen) |*f| f.records else &.{};
    }

    pub fn tailOffset(self: *const NodeDirty) usize {
        return self.tail_offset;
    }

    /// Where the log would end after appending everything pending plus
    /// `extra_bytes` more encoded record bytes: the in-flight frozen frame
    /// and one append frame over the open delta + extra. `> node_size` ⇒
    /// the flush needs an SMO (conservative — two frame headers where one
    /// flush might merge them).
    pub fn projectedLogEnd(self: *const NodeDirty, layout: NodeLayout, extra_bytes: usize) usize {
        const frozen: usize = if (self.frozen) |*f| f.frame_len else 0;
        return self.tail_offset + frozen + layout.appendFrameLen(self.overlay_bytes + extra_bytes);
    }

    /// Encoded bytes of the frozen delta awaiting its append (0 = none).
    pub fn frozenBytes(self: *const NodeDirty) usize {
        return if (self.frozen) |*f| f.bytes() else 0;
    }

    pub fn promisedExtents(self: *const NodeDirty) u64 {
        return self.promised;
    }

    pub fn promiseBasis(self: *const NodeDirty) usize {
        return self.promise_basis;
    }

    pub fn promiseAdded(self: *const NodeDirty) usize {
        return self.promise_added;
    }

    /// A member admitted under the existing promise (inside the growth
    /// window): count its bytes toward the next re-walk.
    pub fn notePromiseGrowth(self: *NodeDirty, bytes: usize) void {
        self.promise_added += bytes;
    }

    /// Re-base the promise on a fresh exact walk without changing it.
    pub fn setPromiseBasis(self: *NodeDirty, basis: usize) void {
        self.promise_basis = basis;
        self.promise_added = 0;
    }

    /// Promise `extents` more to this node's next SMO, computed for a fold
    /// of `basis` bytes — the caller checked the budget against the ledger
    /// under this node's write lock.
    pub fn promise(self: *NodeDirty, extents: u64, basis: usize) void {
        self.promised += extents;
        self.promise_basis = basis;
        self.promise_added = 0;
        self.heap_promised.add(extents);
        alloc_ext.meta_kv_heap_promised.add(extents);
    }

    /// Return `extents` of a promise made for a member that was then
    /// REFUSED: the member's records never land, so the SMO they projected
    /// is not owed (a promise left on a node with nothing pending stands as
    /// a permanent claimable deficit). A promise back at 0 clears its basis.
    pub fn retractPromise(self: *NodeDirty, extents: u64) void {
        const take = @min(extents, self.promised);
        self.promised -= take;
        self.heap_promised.sub(take);
        alloc_ext.meta_kv_heap_promised.sub(take);
        if (self.promised == 0) {
            self.promise_basis = 0;
            self.promise_added = 0;
        }
    }

    /// Undo `notePromiseGrowth` for a refused member's bytes.
    pub fn retractPromiseGrowth(self: *NodeDirty, bytes: usize) void {
        self.promise_added -|= bytes;
    }

    /// Return this node's promise to the ledger (the SMO consumed it, or the
    /// pending bytes fit in place after an append).
    pub fn releasePromise(self: *NodeDirty) void {
        if (self.promised > 0) {
            self.heap_promised.sub(self.promised);
            alloc_ext.meta_kv_heap_promised.sub(self.promised);
            self.promised = 0;
        }
        self.promise_basis = 0;
        self.promise_added = 0;
    }

    /// Take the open delta (the "delta that accumulated during the build")
    /// — SMO-only, under the write lock; the records move into the
    /// successors' open deltas (heads ride along — the successor base folds
    /// identically for every moved key). The charge moves with them;
    /// published snapshots keep serving the pre-swap view. Caller owns the
    /// returned slice (`gpa.free` after moving its elements).
    pub fn takeOverlay(self: *NodeDirty, gpa: Allocator) Allocator.Error![]OwnedRec {
        const fresh = try Arc(Run).create(gpa, Run.empty);
        errdefer fresh.release();
        const out = try self.overlay.toOwnedSlice(gpa);
        self.overlay_bytes = 0;
        self.head_bytes = 0;
        self.snap_stable.release();
        self.snap_stable = fresh;
        self.clearTail();
        self.resyncCharge();
        // The SMO taking the delta is the one the promise was for: its
        // claims already drew the budget the promise held.
        self.releasePromise();
        return out;
    }

    fn clearTail(self: *NodeDirty) void {
        for (self.snap_tail.items) |*r| r.deinit();
        self.snap_tail.clearRetainingCapacity();
    }

    /// Re-merge: the stable mirror becomes a clone of the whole overlay and
    /// the tail empties.
    fn remergeStable(self: *NodeDirty, gpa: Allocator) Allocator.Error!void {
        const recs = try gpa.alloc(OwnedRec, self.overlay.items.len);
        errdefer gpa.free(recs);
        for (self.overlay.items, recs) |*src, *dst| dst.* = src.clone();
        const fresh = Arc(Run).create(gpa, .{ .recs = recs }) catch |e| {
            for (recs) |*r| r.deinit();
            return e;
        };
        self.snap_stable.release();
        self.snap_stable = fresh;
        self.clearTail();
    }

    /// Bring the budget gauge in line with this open delta's bytes
    /// (`overlay + heads`). Called at the end of every locked mutation;
    /// deinit subtracts the residue exactly.
    fn resyncCharge(self: *NodeDirty) void {
        const new: u64 = self.overlay_bytes + self.head_bytes;
        if (new > self.charged) self.charge.add(new - self.charged) else if (new < self.charged) self.charge.sub(self.charged - new);
        self.charged = new;
    }

    pub fn deinit(self: *NodeDirty, gpa: Allocator) void {
        for (self.overlay.items) |*r| r.deinit();
        self.overlay.deinit(gpa);
        self.clearTail();
        self.snap_tail.deinit(gpa);
        self.snap_stable.release();
        if (self.frozen) |*f| f.deinit(gpa);
        // The accounting is deinit-owned: whatever this open delta still
        // charges leaves the gauge with it.
        if (self.charged > 0) self.charge.sub(self.charged);
        self.releasePromise();
        self.* = undefined;
    }
};

// ---------------------------------------------------------------------------
// The cached node.
// ---------------------------------------------------------------------------

pub const CachedNode = struct {
    cache: *NodeCache,
    /// References held: the map's shard snapshots plus every caller that
    /// resolved it. The eviction sweep reclaims only what dropping the map
    /// reference would free.
    refs: std.atomic.Value(u32),
    /// The revalidation epoch this object was LOADED under, stamped from
    /// the pre-device-read snapshot by `publishStamped`; `unarmed_epoch`
    /// on every write mount, which is what makes the hit-path compare free.
    epoch_stamp: std.atomic.Value(u64),
    addr: u64,
    node_seq: u64,
    tree_id: u8,
    level: u8,
    min_key: []u8,
    max_key: []u8,
    state: NodeState,
    snapshot: ArcSwap(NodeSnapshot),
    /// The 4b leaf write lock (interior locks belong to the serialized
    /// SMO task).
    lock: sync.RwLock,
    dirty: NodeDirty,
    /// Clock second-chance bit (set on access).
    ref_bit: std.atomic.Value(bool),
    /// Interior nodes and tree roots never evict.
    pinned: std.atomic.Value(bool),
    /// §4.6 pt 2 `oldest_dirty_seq`: the smallest entry-start floor applied
    /// to this node that is not yet in a durable-covered bset (`no_floor`
    /// = none). The checkpoint task swaps it out per flush pass and restores
    /// it if the pass fails; the tail rule takes the min over these.
    dirty_floor: std.atomic.Value(u64),
    /// The slot tree this node belongs to on a forest volume (RAM-only).
    forest_slot: std.atomic.Value(u32),

    /// Build from a verified extent (`loaded` borrows `buf`; both are
    /// consumed). Born clean with one reference, the caller's.
    pub fn fromLoaded(cache: *NodeCache, loaded: node.LoadedNode, buf: *SharedBuf, pinned: bool) Allocator.Error!*CachedNode {
        const gpa = cache.gpa;
        var ld = loaded;
        defer ld.deinit(gpa);
        errdefer buf.release();
        const sources = try gpa.alloc(Bytes, ld.bset_ranges.len);
        var made: usize = 0;
        var sources_ours = true;
        errdefer if (sources_ours) {
            for (sources[0..made]) |s| s.deinit();
            gpa.free(sources);
        };
        for (ld.bset_ranges, 0..) |r, i| {
            sources[i] = .{ .buf = buf.retain(), .off = @intCast(r.start), .len = @intCast(r.end - r.start) };
            made += 1;
        }
        // The index owns the sources from here (on every outcome).
        sources_ours = false;
        const index = RecordIndex.build(gpa, sources) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            // The ranges were verified by the load walk; our own count is bounded.
            error.Corrupt, error.ChecksumMismatch, error.TooManySources => unreachable,
        };
        const base = Arc(RecordIndex).create(gpa, index) catch |e| {
            var idx = index;
            idx.deinit(gpa);
            return e;
        };
        errdefer base.release();
        const stable = try Arc(Run).create(gpa, Run.empty);
        errdefer stable.release();
        const snap = try Arc(NodeSnapshot).create(gpa, NodeSnapshot.init(base, stable, &.{}, &cache.cached_bytes));
        errdefer snap.release();
        const min_key = try gpa.dupe(u8, ld.header.min_key);
        errdefer gpa.free(min_key);
        const max_key = try gpa.dupe(u8, ld.header.max_key);
        errdefer gpa.free(max_key);
        const dirty_stable = try Arc(Run).create(gpa, Run.empty);
        errdefer dirty_stable.release();
        const self = try gpa.create(CachedNode);
        self.* = .{
            .cache = cache,
            .refs = .init(1),
            .epoch_stamp = .init(epoch_core.unarmed_epoch),
            .addr = ld.header.node_addr,
            .node_seq = ld.header.node_seq,
            .tree_id = ld.header.tree_id,
            .level = ld.header.level,
            .min_key = min_key,
            .max_key = max_key,
            .state = .clean,
            .snapshot = .init(snap),
            .lock = .init,
            .dirty = .{
                .snap_stable = dirty_stable,
                .tail_offset = ld.tail_offset,
                .charge = &cache.cached_bytes,
                .heap_promised = &cache.heap_promised,
            },
            .ref_bit = .init(true),
            .pinned = .init(pinned),
            .dirty_floor = .init(no_floor),
            .forest_slot = .init(no_forest_slot),
        };
        if (pinned) cache.pinned_nodes.add(1);
        // The extent buffer's own reference: the sources hold theirs.
        buf.release();
        return self;
    }

    pub fn retain(self: *CachedNode) *CachedNode {
        _ = self.refs.fetchAdd(1, .monotonic);
        return self;
    }

    /// acq_rel: the last release sees every holder's accesses before the
    /// object is torn down.
    pub fn release(self: *CachedNode) void {
        if (self.refs.fetchSub(1, .acq_rel) != 1) return;
        const gpa = self.cache.gpa;
        if (self.pinned.load(.monotonic)) self.cache.pinned_nodes.sub(1);
        self.snapshot.deinit();
        self.dirty.deinit(gpa);
        gpa.free(self.min_key);
        gpa.free(self.max_key);
        gpa.destroy(self);
    }

    pub fn refCount(self: *const CachedNode) u32 {
        return self.refs.load(.monotonic);
    }

    /// Latch-free read entry: the current immutable snapshot (a reference
    /// the caller releases).
    pub fn snapshotLoad(self: *CachedNode) *Arc(NodeSnapshot) {
        return self.snapshot.load();
    }

    // ---- the 4b lock faces (RAM-only critical sections) ----

    /// The write lock; committers are never cancelled.
    pub fn lockWrite(self: *CachedNode, io: Io) *NodeDirty {
        self.lock.lockUncancelable(io);
        return &self.dirty;
    }

    pub fn unlockWrite(self: *CachedNode, io: Io) void {
        self.lock.unlock(io);
    }

    pub fn lockRead(self: *CachedNode, io: Io) *const NodeDirty {
        self.lock.lockSharedUncancelable(io);
        return &self.dirty;
    }

    pub fn unlockRead(self: *CachedNode, io: Io) void {
        self.lock.unlockShared(io);
    }

    pub fn tryLockRead(self: *CachedNode, io: Io) ?*const NodeDirty {
        if (!self.lock.tryLockShared(io)) return null;
        return &self.dirty;
    }

    // ---- identity / bits ----

    /// Pin (tree roots after the tree open; interior nodes at load).
    pub fn pin(self: *CachedNode) void {
        // acq_rel swap paired with `isPinned`'s acquire load.
        if (!self.pinned.swap(true, .acq_rel)) self.cache.pinned_nodes.add(1);
    }

    pub fn isPinned(self: *const CachedNode) bool {
        return self.pinned.load(.acquire);
    }

    fn touch(self: *CachedNode) void {
        self.ref_bit.store(true, .monotonic);
    }

    /// Probation: enter the clock with NO second chance — the node earns
    /// its ref bit on a second touch, so a giant streaming map's
    /// demand-paged leaves evict first.
    fn beginProbation(self: *CachedNode) void {
        self.ref_bit.store(false, .monotonic);
    }

    pub fn epochStamp(self: *const CachedNode) u64 {
        return self.epoch_stamp.load(.monotonic);
    }

    pub fn stampForestSlot(self: *CachedNode, slot: ForestSlot) void {
        self.forest_slot.store(slot, .monotonic);
    }

    pub fn forestSlot(self: *const CachedNode) ?ForestSlot {
        const s = self.forest_slot.load(.monotonic);
        return if (s == no_forest_slot) null else s;
    }

    pub fn dirtyFloor(self: *const CachedNode) u64 {
        return self.dirty_floor.load(.acquire);
    }

    /// Checkpoint flush pass: take the floor (leaving `no_floor`) — records
    /// applied after this re-lower it and belong to the next cycle. Call
    /// under the node write lock, in the same window as the freeze.
    pub fn takeDirtyFloor(self: *CachedNode) u64 {
        // acq_rel swap paired with `applyLocked`'s acq_rel fetchMin.
        return self.dirty_floor.swap(no_floor, .acq_rel);
    }

    /// Restore a floor after a failed flush: the records are still not
    /// durable-covered, so the tail must keep respecting them.
    pub fn restoreDirtyFloor(self: *CachedNode, floor: u64) void {
        _ = self.dirty_floor.fetchMin(floor, .acq_rel);
    }

    // ---- the choke point ----

    /// Apply records to the open delta UNDER THE HELD WRITE LOCK and swap a
    /// new snapshot. Records must carry seqs assigned inside this lock
    /// window. `floor` is the §4.6 pt 2 dirty-floor contribution, rounded
    /// DOWN to the records' journal-entry start (a mid-entry tail drops the
    /// entry's ≥-tail acked records at replay). The records' elements are
    /// consumed on every outcome; the slice stays the caller's.
    pub fn applyLocked(self: *CachedNode, dirty: *NodeDirty, records: []OwnedRec, floor: u64) ApplyError!void {
        return self.applyLockedClass(dirty, records, floor, false);
    }

    /// `applyLocked` for the serialized SMO task's OWN moves (leftover
    /// overlay into a successor, a parent's pointer flips): reads the
    /// STRUCTURAL lease verdict — a slot mid-handover is admitted (the
    /// departing holder's flush IS the release's work), as is an unleased
    /// slot on the manager.
    pub fn applyLockedStructural(self: *CachedNode, dirty: *NodeDirty, records: []OwnedRec, floor: u64) ApplyError!void {
        return self.applyLockedClass(dirty, records, floor, true);
    }

    fn consume(records: []OwnedRec) void {
        for (records) |*r| r.deinit();
    }

    fn applyLockedClass(self: *CachedNode, dirty: *NodeDirty, records: []OwnedRec, floor: u64, structural: bool) ApplyError!void {
        const gpa = self.cache.gpa;
        // The partitioning gate — the ONE place every RAM mutation passes:
        // an armed READER mutates nothing (its cache is a projection of
        // somebody else's tree); interior nodes have ONE cacher and ONE
        // mutator, the root authority (the RAM face of lock order 4b).
        // Leaves are deliberately NOT arbitrated here: which appender owns
        // which key range is the slot map's job, and a gate that pretended
        // otherwise would be mistaken for cross-writer custody. One
        // monotonic load; on a solo volume the word is 0.
        const gate = self.cache.env.gate.load();
        if (gate.reader) {
            consume(records);
            partition_refusals.fire("a mount that armed reader revalidation tried to mutate a node");
            return error.ReaderMayNotMutate;
        }
        if (self.level > 0 and !gate.isAuthority()) {
            consume(records);
            partition_refusals.fire("a non-authority appender tried a structural mutation of an interior node");
            return error.NonAuthorityInterior;
        }
        // The THIRD gate state: "appender for my own leaves". A slot tree
        // has exactly one writer-cacher — its lessee — so on an ARMED mount a
        // leaf of a slot this mount does not lease, or one mid-handover, is
        // refused here under the node's write lock. Flat volumes never reach
        // the hook: the stamp is unset.
        if (self.forestSlot()) |slot| {
            switch (self.cache.lease.verdict(self.cache.lease.ctx, slot, structural)) {
                .unarmed, .allowed => {},
                .not_leased, .releasing => {
                    consume(records);
                    leaf_lease_refusals.fire("a leaf of a slot this mount does not lease (or mid-handover) was mutated");
                    return error.LeafLeaseRefused;
                },
            }
        }
        self.state.markDirty() catch {
            consume(records);
            return error.Superseded;
        };
        // Every apply lowers the not-yet-durable floor; the tail rule reads
        // it back. acq_rel fetchMin paired with `takeDirtyFloor`'s swap.
        if (records.len != 0) _ = self.dirty_floor.fetchMin(floor, .acq_rel);
        // Reserve the inserts up front so a refusal below cannot leave a
        // record half-applied.
        try dirty.overlay.ensureUnusedCapacity(gpa, records.len);
        try dirty.snap_tail.ensureUnusedCapacity(gpa, records.len);
        var applied: usize = 0;
        errdefer consume(records[applied..]);
        for (records) |*moved| {
            var r = moved.*;
            moved.* = undefined;
            // Stamp (or tighten) the record's own floor contribution so an
            // SMO leftover transfer can rebuild successor floors exactly.
            r.entry_floor = @min(r.entry_floor, floor);
            const items = dirty.overlay.items;
            const pos = std.sort.partitionPoint(OwnedRec, items, KeySeq{ .k = r.key.slice(), .seq = r.seq }, keySeqLessEq);
            const gend = std.sort.partitionPoint(OwnedRec, items, KeyOnly{ .k = r.key.slice() }, keyLessEq);
            const key_newest = pos == gend;
            const prev_is_same_key = pos > 0 and std.mem.eql(u8, items[pos - 1].key.slice(), r.key.slice());
            // D7.a head materialization: fold-forward against the folded
            // outcome of everything older than `r` for this key. Put and
            // Delete shadow everything below, so their heads are
            // self-defining; the fold-below is fetched only for Delta. A
            // corrupt base/delta surfaces at read time exactly as before —
            // the apply never changes semantics over it (no head).
            if (r.folded == null) {
                r.folded = switch (r.kind) {
                    .put => .{ .live = .{ .value = r.value.clone(), .decoded = null, .materialized = false } },
                    .delete => .tombstone,
                    .delta => blk: {
                        const prev: ?Head = if (prev_is_same_key)
                            (if (items[pos - 1].folded) |h| h.clone() else self.foldBelowLocked(items[groupStart(items, pos, r.key.slice())..pos], r.key.slice()) catch null)
                        else
                            self.foldBelowLocked(&.{}, r.key.slice()) catch null;
                        const p = prev orelse break :blk null;
                        defer p.deinit();
                        const prev_view = p.asFolded();
                        const next = rec.foldForward(gpa, &prev_view, r.kind, r.value.slice()) catch break :blk null;
                        break :blk Head.fromFolded(gpa, next, r.value) catch blk2: {
                            var n = next;
                            n.deinit(gpa);
                            break :blk2 null;
                        };
                    },
                };
            }
            // Single-head-per-key: the superseded newest below loses its
            // head (never read again — lookups take the group's newest).
            if (prev_is_same_key) {
                dirty.head_bytes -= items[pos - 1].headBytes();
                items[pos - 1].clearHead();
            }
            // Per-key inversion guard: records above `r` folded without it
            // — their heads are stale. Invalidate; reads fall back to the
            // from-scratch fold (byte-identical by the algebra theorem).
            if (!key_newest) {
                for (items[pos..gend]) |*above| {
                    dirty.head_bytes -= above.headBytes();
                    above.clearHead();
                }
            }
            dirty.head_bytes += r.headBytes();
            dirty.overlay_bytes += r.recordRef().encodedLen();
            // Run-precedence guard: mount replay applies ORIGINAL seqs,
            // which may sort below records already published. Fold order
            // requires "tail ≥ stable per key", so a non-monotonic apply
            // re-merges the whole overlay into a fresh stable run instead
            // of riding the tail (replay-only cost, bounded by the window).
            const monotonic = r.seq > dirty.max_applied_seq;
            dirty.max_applied_seq = @max(dirty.max_applied_seq, r.seq);
            if (monotonic) {
                const tpos = std.sort.partitionPoint(OwnedRec, dirty.snap_tail.items, KeySeq{ .k = r.key.slice(), .seq = r.seq }, keySeqLessEq);
                dirty.snap_tail.insertAssumeCapacity(tpos, r.clone());
                dirty.overlay.insertAssumeCapacity(pos, r);
                applied += 1;
            } else {
                dirty.overlay.insertAssumeCapacity(pos, r);
                applied += 1;
                try dirty.remergeStable(gpa);
            }
        }
        // Publish: share the stable run, clone only the small tail. Tail
        // overflow re-merges into a fresh stable run, amortizing the full
        // clone over OVERLAY_TAIL_MAX applies.
        if (dirty.snap_tail.items.len > snapshot.OVERLAY_TAIL_MAX) try dirty.remergeStable(gpa);
        assert(dirty.snap_stable.value.recs.len + dirty.snap_tail.items.len == dirty.overlay.items.len);
        dirty.resyncCharge();
        try self.publishSnapshot(dirty, null);
    }

    const KeySeq = struct { k: []const u8, seq: u64 };

    /// `(r.key, r.seq) <= (ctx.k, ctx.seq)`.
    fn keySeqLessEq(ctx: KeySeq, r: OwnedRec) bool {
        return switch (key.order(r.key.slice(), ctx.k)) {
            .lt => true,
            .gt => false,
            .eq => r.seq <= ctx.seq,
        };
    }

    const KeyOnly = struct { k: []const u8 };

    fn keyLessEq(ctx: KeyOnly, r: OwnedRec) bool {
        return key.order(r.key.slice(), ctx.k) != .gt;
    }

    fn groupStart(items: []const OwnedRec, end: usize, k: []const u8) usize {
        var i = end;
        while (i > 0 and std.mem.eql(u8, items[i - 1].key.slice(), k)) i -= 1;
        return i;
    }

    /// Writer-side fold of "everything older than the record being applied"
    /// for one key: the given lock-held overlay group below it, newest
    /// first, then the current base group. With no overlay records below,
    /// the published snapshot's memo may already carry the base fold —
    /// probed without touching the read counters. Cold by construction.
    fn foldBelowLocked(self: *CachedNode, overlay_below: []const OwnedRec, k: []const u8) rec.FoldError!Head {
        const gpa = self.cache.gpa;
        const cur = self.snapshot.load();
        defer cur.release();
        if (overlay_below.len == 0) {
            if (cur.value.memo.probe(k)) |cell| {
                // The memo already pays for the buffer; the head borrows it.
                return .{ .live = .{ .value = cell.value.clone(), .decoded = null, .materialized = false } };
            }
        }
        const bg = cur.value.base.value.groupBounds(k);
        const n = overlay_below.len + (bg.hi - bg.lo);
        const refs = try gpa.alloc(RecordRef, n);
        defer gpa.free(refs);
        var i: usize = 0;
        var o = overlay_below.len;
        while (o > 0) : (i += 1) {
            o -= 1;
            refs[i] = overlay_below[o].recordRef();
        }
        for (bg.lo..bg.hi) |b| {
            refs[i] = cur.value.base.value.recordRef(b);
            i += 1;
        }
        const folded = try rec.foldNewestFirst(gpa, refs);
        switch (folded) {
            .absent => return .absent,
            .tombstone => return .tombstone,
            .put => |p| {
                // Either arm owns a fresh buffer (folded or copied) — one
                // copy beats threading provenance through the fold, and the
                // head then serves refcounted for its whole life.
                const owned: []u8 = if (p.owned) @constCast(p.value) else try gpa.dupe(u8, p.value);
                errdefer gpa.free(owned);
                return .{ .live = .{ .value = try Bytes.fromOwned(gpa, owned), .decoded = null, .materialized = true } };
            },
        }
    }

    /// Publish a snapshot of the mirrors over `base` (null = the current
    /// base), with a fresh memo.
    fn publishSnapshot(self: *CachedNode, dirty: *NodeDirty, base_override: ?*Arc(RecordIndex)) Allocator.Error!void {
        const gpa = self.cache.gpa;
        const base = base_override orelse blk: {
            const cur = self.snapshot.load();
            defer cur.release();
            break :blk cur.value.base.retain();
        };
        errdefer base.release();
        const tail = try gpa.alloc(OwnedRec, dirty.snap_tail.items.len);
        var cloned: usize = 0;
        errdefer {
            for (tail[0..cloned]) |*r| r.deinit();
            gpa.free(tail);
        }
        for (dirty.snap_tail.items, tail) |*src, *dst| {
            dst.* = src.clone();
            cloned += 1;
        }
        const stable = dirty.snap_stable.retain();
        errdefer stable.release();
        const fresh = try Arc(NodeSnapshot).create(gpa, NodeSnapshot.init(base, stable, tail, &self.cache.cached_bytes));
        self.snapshot.store(fresh);
    }

    /// §4.4 pt 4 seq-conditional rollback, removal half: under the held
    /// write lock, remove every open-overlay record of `k` whose seq is
    /// inside the failing tx's STAMPED span and swap a fresh snapshot.
    /// Records that already left the overlay (a freeze raced the failed
    /// write) are the caller's compensation problem — detected via
    /// `newestSeqOf`. Returns how many records were removed.
    pub fn removeOverlayRecordsLocked(self: *CachedNode, dirty: *NodeDirty, k: []const u8, span: SeqSpan) Allocator.Error!usize {
        const gpa = self.cache.gpa;
        var removed: usize = 0;
        var i: usize = 0;
        while (i < dirty.overlay.items.len) {
            const r = &dirty.overlay.items[i];
            if (std.mem.eql(u8, r.key.slice(), k) and span.contains(r.seq)) {
                var victim = dirty.overlay.orderedRemove(i);
                victim.deinit();
                removed += 1;
            } else {
                i += 1;
            }
        }
        if (removed == 0) return 0;
        // D7.a stale-head guard: a surviving record NEWER than the removed
        // range folded the removed records into its head — the
        // concurrent-Δtime rollback race shape. Invalidate those heads;
        // older survivors' heads folded nothing that was removed and stay.
        const g = snapshot.runGroup(dirty.overlay.items, k);
        for (dirty.overlay.items[g.lo..g.hi]) |*r| {
            if (r.seq > span.lo) r.clearHead();
        }
        dirty.overlay_bytes = 0;
        dirty.head_bytes = 0;
        for (dirty.overlay.items) |*r| {
            dirty.overlay_bytes += r.recordRef().encodedLen();
            dirty.head_bytes += r.headBytes();
        }
        // Rollback is cold: resync the mirrors with a full re-merge and
        // publish the corrected view.
        try dirty.remergeStable(gpa);
        dirty.resyncCharge();
        try self.publishSnapshot(dirty, null);
        return removed;
    }

    /// The §4.6 pt 1 freeze-swap, UNDER THE HELD WRITE LOCK: move the open
    /// delta out as an immutable frozen bset image (4 KiB-padded), merge
    /// it into the snapshot's base index incrementally (readers see
    /// identical content, now serialized), and leave the append to run
    /// OUTSIDE the lock. Returns the parked frozen delta if one already
    /// awaits I/O, null for an empty overlay.
    ///
    /// Freeze-time shadow-fold: per key, only the newest base-establishing
    /// record (`Put`/`Delete`) and the `Delta`s newer than it reach the
    /// image; records completely shadowed by the fold never reach the
    /// device (`meta_kv_node_freeze_shadow_dropped`). Crash safety is
    /// unchanged: within one freeze the shadowing record and its victims
    /// share one bset that either survives whole or is dropped whole.
    pub fn freezeLocked(self: *CachedNode, dirty: *NodeDirty, layout: NodeLayout) FreezeError!?*const FrozenDelta {
        const gpa = self.cache.gpa;
        if (dirty.frozen) |*f| return f;
        // RES-22 self-heal: FREEZING latched with NO frozen delta on a live
        // node is a freeze that never completed. The two known windows
        // (the W-A error escape below, the forced-freeze drop) are closed
        // structurally, so observing the shape means a THIRD unknown
        // window: report loud, restore, proceed — one line instead of a
        // permanent per-node checkpoint wedge. Safe HERE, under the held
        // node write lock on the serialized writeback/SMO task.
        if (self.state.isFreezing() and !self.state.isSuperseded()) {
            freeze_latched_recovered.fire("FREEZING latched with no frozen delta — lifecycle word restored, freeze retried");
            if (dirty.overlay.items.len == 0) {
                _ = self.state.endFreeze();
            } else {
                self.state.abortFreeze();
            }
        }
        if (dirty.overlay.items.len == 0) {
            // A rollback emptied the overlay under this lock and left the
            // word DIRTY with nothing to write: the EMPTY freeze (begin →
            // end) closes it, so the node is collectable and evictable
            // again — without it the flush pass re-collected the node every
            // cycle and a hole checkpoint behind a failed window never
            // converged.
            if (self.state.isDirty() and !self.state.isFreezing() and !self.state.isSuperseded()) {
                self.state.beginFreeze() catch {};
                _ = self.state.endFreeze();
            }
            return null;
        }
        self.state.beginFreeze() catch |e| return switch (e) {
            error.NotDirty => error.FreezeRefusedNotDirty,
            error.AlreadyFreezing => error.FreezeRefusedAlreadyFreezing,
            error.Superseded => error.FreezeRefusedSuperseded,
        };
        // W-A restore: between the successful `beginFreeze` and the
        // `dirty.frozen = ...` publication, EVERY error path leaves the
        // overlay untouched — so the only state to restore is the word.
        // Without it an escaping error (the field's oversize record
        // failing the frame encoder) latched FREEZING forever.
        errdefer self.state.abortFreeze();
        if (testFreezeFailArmed()) return error.InjectedFreezeFailure;

        // Shadow-fold: one linear pass over per-key runs of the sorted overlay.
        const items = dirty.overlay.items;
        var records: std.ArrayListUnmanaged(rec.Record) = .empty;
        errdefer {
            rec.deinitRecords(gpa, records.items);
            records.deinit(gpa);
        }
        try records.ensureTotalCapacity(gpa, items.len);
        var dropped: u64 = 0;
        var horizon: u64 = 0;
        var i: usize = 0;
        while (i < items.len) {
            var j = i + 1;
            while (j < items.len and std.mem.eql(u8, items[j].key.slice(), items[i].key.slice())) j += 1;
            // Newest base-establishing record in the run (runs are seq-
            // ascending); none ⇒ delta-only run, keep everything.
            var keep_from = i;
            var k = j;
            while (k > i) {
                k -= 1;
                if (items[k].kind == .put or items[k].kind == .delete) {
                    keep_from = k;
                    break;
                }
            }
            dropped += keep_from - i;
            for (items[keep_from..j]) |*r| records.appendAssumeCapacity(try r.toRecord(gpa));
            for (items[i..j]) |*r| horizon = @max(horizon, r.seq);
            i = j;
        }
        if (dropped > 0) stats.meta_kv_node_freeze_shadow_dropped.add(dropped);
        const frozen_records = try records.toOwnedSlice(gpa);
        errdefer {
            rec.deinitRecords(gpa, frozen_records);
            gpa.free(frozen_records);
        }
        const refs = try gpa.alloc(RecordRef, frozen_records.len);
        defer gpa.free(refs);
        for (frozen_records, refs) |*r, *o| o.* = r.ref();
        const frame = node.encodeBsetFrame(gpa, layout.frame, layout.node_size, self.node_seq, refs, horizon) catch |e| switch (e) {
            error.ValueTooLarge => return error.ValueTooLarge,
            error.BsetTooLarge => return error.Corrupt,
            error.OutOfMemory => return error.OutOfMemory,
            // Non-empty and strictly ascending by the overlay's order.
            error.EmptyBset, error.RecordsNotAscending => unreachable,
        };
        const frame_len = frame.len;
        const frame_hdr = layout.frameLen();
        const bset_len: usize = std.mem.readInt(u32, frame[20..24], .little);
        const whole = Bytes.fromOwned(gpa, frame) catch |e| {
            gpa.free(frame);
            return e;
        };
        const image = whole.sub(frame_hdr, bset_len);
        whole.deinit();
        // The frame's bset becomes one more index source, merged
        // INCREMENTALLY (O(existing + new)).
        const cur = self.snapshot.load();
        defer cur.release();
        const extended = cur.value.base.value.extendWith(gpa, image) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            // Our own encoder's image; the source count is bounded by the log.
            error.Corrupt, error.ChecksumMismatch, error.TooManySources => unreachable,
        };
        const new_base = Arc(RecordIndex).create(gpa, extended) catch |e| {
            var x = extended;
            x.deinit(gpa);
            return e;
        };
        errdefer new_base.release();
        const empty_stable = try Arc(Run).create(gpa, Run.empty);
        errdefer empty_stable.release();
        const fresh = try Arc(NodeSnapshot).create(gpa, NodeSnapshot.init(new_base, empty_stable, &.{}, &self.cache.cached_bytes));
        const mirror_stable = try Arc(Run).create(gpa, Run.empty);
        // The last fallible step succeeded — the freeze WILL publish
        // (everything below is infallible RAM bookkeeping).
        for (dirty.overlay.items) |*r| r.deinit();
        dirty.overlay.clearRetainingCapacity();
        dirty.overlay_bytes = 0;
        // The heads freeze away with their records; the first post-freeze
        // read repopulates through the memo instead.
        dirty.head_bytes = 0;
        dirty.snap_stable.release();
        dirty.snap_stable = mirror_stable;
        dirty.clearTail();
        dirty.resyncCharge();
        dirty.frozen = .{ .records = frozen_records, .horizon = horizon, .frame_len = frame_len };
        self.snapshot.store(fresh);
        return &dirty.frozen.?;
    }

    /// The checkpoint flush step's lock window: freeze (or get the PARKED
    /// delta back), take the dirty floor, and KEEP the floor of the records
    /// still in the open delta. The eight-writer storm's "deleted stays
    /// deleted" loss: a parked delta (an SMO's freeze that failed before
    /// its swap) was appended alone while the whole floor was taken, so the
    /// newer open records were never walked by a later flush and left with
    /// the tree at the leave — RAM said Tombstone, the image said Live.
    /// Returns the taken floor (the caller restores it if the write fails)
    /// and whether a frozen delta awaits its append.
    pub fn flushFreezeLocked(self: *CachedNode, dirty: *NodeDirty, layout: NodeLayout) FreezeError!FlushFreeze {
        const frozen = try self.freezeLocked(dirty, layout);
        const floor = self.takeDirtyFloor();
        if (frozen != null) {
            if (dirty.overlayFloor()) |remaining| {
                stats.meta_kv_flush_floor_kept.inc();
                self.restoreDirtyFloor(remaining);
            }
        }
        return .{ .floor = floor, .frozen = frozen != null };
    }

    fn testFreezeFailArmed() bool {
        var v = test_freeze_encode_fail.load(.monotonic);
        while (v > 0) {
            v = test_freeze_encode_fail.cmpxchgWeak(v, v - 1, .monotonic, .monotonic) orelse return true;
        }
        return false;
    }
};

// ---------------------------------------------------------------------------
// The cache.
// ---------------------------------------------------------------------------

/// One immutable map shard: owns a reference to every node it maps.
const Shard = struct {
    nodes: std.AutoHashMapUnmanaged(u64, *CachedNode) = .empty,

    pub fn deinit(self: *Shard, gpa: Allocator) void {
        var it = self.nodes.valueIterator();
        while (it.next()) |n| n.*.release();
        self.nodes.deinit(gpa);
    }
};

const ShardSlot = struct {
    current: ArcSwap(Shard) align(std.atomic.cache_line),
    /// Writers only (insert / remove); the sanctioned std.Io.Mutex class.
    mutex: Io.Mutex = .init,
};

/// The clock ring: pushes at publish, pops at eviction — both on the miss
/// path, never on a hit — so the sanctioned mutex is the honest choice.
const Clock = struct {
    mutex: Io.Mutex = .init,
    items: std.ArrayListUnmanaged(u64) = .empty,
    head: usize = 0,

    fn push(self: *Clock, io: Io, gpa: Allocator, addr: u64) Allocator.Error!void {
        self.mutex.lockUncancelable(io);
        defer self.mutex.unlock(io);
        try self.items.append(gpa, addr);
    }

    fn pop(self: *Clock, io: Io) ?u64 {
        self.mutex.lockUncancelable(io);
        defer self.mutex.unlock(io);
        if (self.head >= self.items.items.len) return null;
        const v = self.items.items[self.head];
        self.head += 1;
        // Compact once the consumed prefix dominates.
        if (self.head > 1024 and self.head * 2 > self.items.items.len) {
            const live = self.items.items.len - self.head;
            std.mem.copyForwards(u64, self.items.items[0..live], self.items.items[self.head..]);
            self.items.shrinkRetainingCapacity(live);
            self.head = 0;
        }
        return v;
    }

    fn deinit(self: *Clock, gpa: Allocator) void {
        self.items.deinit(gpa);
    }
};

/// Extents an SMO retired whose disk image lags the RAM state that
/// superseded it (miss-path checks only).
const Retired = struct {
    mutex: Io.Mutex = .init,
    set: std.AutoHashMapUnmanaged(u64, void) = .empty,

    fn contains(self: *Retired, io: Io, addr: u64) bool {
        self.mutex.lockUncancelable(io);
        defer self.mutex.unlock(io);
        return self.set.contains(addr);
    }

    fn insert(self: *Retired, io: Io, gpa: Allocator, addr: u64) Allocator.Error!void {
        self.mutex.lockUncancelable(io);
        defer self.mutex.unlock(io);
        try self.set.put(gpa, addr, {});
    }

    fn remove(self: *Retired, io: Io, addr: u64) void {
        self.mutex.lockUncancelable(io);
        defer self.mutex.unlock(io);
        _ = self.set.remove(addr);
    }

    fn deinit(self: *Retired, gpa: Allocator) void {
        self.set.deinit(gpa);
    }
};

/// What one revalidation poll did.
pub const RevalidateOutcome = struct {
    advanced: bool = false,
    from_epoch: u64 = 0,
    epoch: u64 = 0,
    tail: u64 = 0,
    dropped: u64 = 0,
    /// Exactly `dropped × node_size` — the charge-conservation law.
    bytes_credited: u64 = 0,
    /// Mappings kept because they already carried the new epoch.
    retained: u64 = 0,
    /// Must stay 0: dirty nodes the pass refused to drop.
    skipped_dirty: u64 = 0,
};

pub const NodeCache = struct {
    gpa: Allocator,
    /// Captured at construction for the interior mutexes and the
    /// single-flight park (the sanctioned class, decision 0001 item 4).
    io: Io,
    cfg: Config,
    loader: NodeLoader,
    shards: []ShardSlot,
    shard_mask: usize,
    mapped: std.atomic.Value(usize) = .init(0),
    inflight: sync.SingleFlight(u64, void, MAX_INFLIGHT_LOADS) = .init,
    clock: Clock = .{},
    retired: Retired = .{},
    /// `meta_kv_node_cache_bytes`: Σ mapped extents + overlay bytes incl.
    /// heads + snapshot memo bytes (the latter two owned by the dirty
    /// halves and memos through this gauge, deinit-exact).
    cached_bytes: counters.Gauge = .{},
    /// `meta_kv_heap_promised`: Σ extents promised to pending SMOs.
    heap_promised: counters.Gauge = .{},
    /// `meta_kv_node_cache_pinned`: nodes exempt from eviction.
    pinned_nodes: counters.Gauge = .{},
    /// The shared environment: the durable tail + revalidation epoch pair
    /// and the mutation gates.
    env: epoch_core.NodeEnv,
    lease: LeaseGate = .unarmed,
    /// The min `dirty_floor` of every node whose mapping LEFT this cache
    /// since the checkpoint task last drained it: a floor that dies with
    /// its mapping would otherwise let the ledger tail sail past un-covered
    /// acked records (the FIND-VS-A loss).
    dying_floors: std.atomic.Value(u64) = .init(no_floor),
    /// The budget shed's lever: an early checkpoint (installed by the
    /// checkpoint task).
    checkpoint_kick: ?Hook = null,
    shed_requests: std.atomic.Value(u64) = .init(0),
    last_shed_target: std.atomic.Value(u64) = .init(0),

    pub fn init(gpa: Allocator, io: Io, cfg: Config, loader: NodeLoader) Allocator.Error!*NodeCache {
        const n = shardCount(cfg.budget_bytes / @max(cfg.layout.node_size, 1));
        const shards = try gpa.alloc(ShardSlot, n);
        var made: usize = 0;
        errdefer {
            for (shards[0..made]) |*s| s.current.deinit();
            gpa.free(shards);
        }
        for (shards) |*s| {
            const empty = try Arc(Shard).create(gpa, .{});
            s.* = .{ .current = .init(empty) };
            made += 1;
        }
        const self = try gpa.create(NodeCache);
        self.* = .{
            .gpa = gpa,
            .io = io,
            .cfg = cfg,
            .loader = loader,
            .shards = shards,
            .shard_mask = n - 1,
            .env = .init(0),
        };
        return self;
    }

    /// Tear down: every mapping's reference leaves; a node some caller
    /// still holds dies with that caller's last release — but it points
    /// at this cache, so callers release before `deinit`.
    pub fn deinit(self: *NodeCache) void {
        for (self.shards) |*s| s.current.deinit();
        self.gpa.free(self.shards);
        self.clock.deinit(self.gpa);
        self.retired.deinit(self.gpa);
        self.gpa.destroy(self);
    }

    pub fn config(self: *const NodeCache) *const Config {
        return &self.cfg;
    }

    pub fn extentAddr(self: *const NodeCache, extent: u64) u64 {
        return self.cfg.heap_base + extent * @as(u64, self.cfg.layout.node_size);
    }

    pub fn addrExtent(self: *const NodeCache, addr: u64) u64 {
        return (addr - self.cfg.heap_base) / @as(u64, self.cfg.layout.node_size);
    }

    pub fn cachedBytes(self: *const NodeCache) u64 {
        return self.cached_bytes.load();
    }

    pub fn heapPromised(self: *const NodeCache) u64 {
        return self.heap_promised.load();
    }

    pub fn mappedCount(self: *const NodeCache) usize {
        return self.mapped.load(.monotonic);
    }

    pub fn durableTail(self: *const NodeCache) u64 {
        return self.env.epoch.tail();
    }

    /// Advance the durable tail (monotone).
    pub fn setDurableTail(self: *NodeCache, tail: u64) void {
        self.env.epoch.advanceTail(tail);
    }

    pub fn setAppender(self: *NodeCache, writers: u16, writer_id: u16) error{InvalidPartition}!void {
        return self.env.gate.setAppender(writers, writer_id);
    }

    pub fn installLeaseGate(self: *NodeCache, gate: LeaseGate) void {
        self.lease = gate;
    }

    pub fn installCheckpointKick(self: *NodeCache, hook: Hook) void {
        self.checkpoint_kick = hook;
    }

    // ---- dying floors ----

    fn noteNodeDyingFloor(self: *NodeCache, n: *CachedNode) void {
        self.noteDyingFloor(n.dirtyFloor());
    }

    /// Fold a departing floor (or a root-swap position) into the
    /// accumulator; `no_floor` is a no-op.
    pub fn noteDyingFloor(self: *NodeCache, floor: u64) void {
        // acq_rel fetchMin paired with `takeDyingFloors`'s swap.
        if (floor != no_floor) _ = self.dying_floors.fetchMin(floor, .acq_rel);
    }

    /// Drain the accumulator — the checkpoint's tail computation, once per
    /// cycle; a failed covering write folds the value back.
    pub fn takeDyingFloors(self: *NodeCache) u64 {
        return self.dying_floors.swap(no_floor, .acq_rel);
    }

    pub fn restoreDyingFloors(self: *NodeCache, floor: u64) void {
        self.noteDyingFloor(floor);
    }

    // ---- the map ----

    fn shardOf(self: *NodeCache, addr: u64) *ShardSlot {
        return &self.shards[std.hash.int(addr) & self.shard_mask];
    }

    /// Latch-free mapped read: one arc-swap load, one probe, one retain.
    fn tryGetMapped(self: *NodeCache, addr: u64) ?*CachedNode {
        const cur = self.shardOf(addr).current.load();
        defer cur.release();
        const n = cur.value.nodes.get(addr) orelse return null;
        return n.retain();
    }

    /// Clone-mutate-publish under the shard mutex. The new shard retains
    /// every node it maps; the old shard's references leave with its last
    /// reader.
    fn cloneShard(self: *NodeCache, slot: *ShardSlot) Allocator.Error!Shard {
        const cur = slot.current.load();
        defer cur.release();
        var copy = try cur.value.nodes.clone(self.gpa);
        var it = copy.valueIterator();
        while (it.next()) |n| _ = n.*.retain();
        return .{ .nodes = copy };
    }

    fn publishShard(self: *NodeCache, slot: *ShardSlot, shard: Shard) Allocator.Error!void {
        var s = shard;
        const arc = Arc(Shard).create(self.gpa, s) catch |e| {
            s.deinit(self.gpa);
            return e;
        };
        slot.current.store(arc);
    }

    /// Map `addr → n` (retaining `n` for the map); returns the displaced
    /// node's map reference, if any.
    fn mapInsert(self: *NodeCache, addr: u64, n: *CachedNode) Allocator.Error!?*CachedNode {
        const slot = self.shardOf(addr);
        slot.mutex.lockUncancelable(self.io);
        defer slot.mutex.unlock(self.io);
        var shard = try self.cloneShard(slot);
        errdefer shard.deinit(self.gpa);
        const old = shard.nodes.get(addr);
        try shard.nodes.put(self.gpa, addr, n.retain());
        try self.publishShard(slot, shard);
        return old;
    }

    /// Unmap `addr` iff it maps `n`; the map's reference is released.
    fn mapRemoveIf(self: *NodeCache, addr: u64, n: *CachedNode) Allocator.Error!bool {
        const slot = self.shardOf(addr);
        slot.mutex.lockUncancelable(self.io);
        defer slot.mutex.unlock(self.io);
        var shard = try self.cloneShard(slot);
        errdefer shard.deinit(self.gpa);
        const mapped = shard.nodes.get(addr) orelse {
            shard.deinit(self.gpa);
            return false;
        };
        if (mapped != n) {
            shard.deinit(self.gpa);
            return false;
        }
        _ = shard.nodes.remove(addr);
        try self.publishShard(slot, shard);
        // The clone's reference on the removed node.
        n.release();
        return true;
    }

    /// Whether `addr` is currently mapped (tests).
    pub fn contains(self: *NodeCache, addr: u64) bool {
        const n = self.tryGetMapped(addr) orelse return false;
        n.release();
        return true;
    }

    /// Visit every mapped node (the checkpoint's dirty-set walk). Not a
    /// consistent snapshot across shards; callers re-check per node under
    /// its lock. The node is valid for the callback's duration.
    pub fn forEachNode(self: *NodeCache, ctx: anytype, comptime f: fn (@TypeOf(ctx), *CachedNode) void) void {
        for (self.shards) |*s| {
            const cur = s.current.load();
            defer cur.release();
            var it = cur.value.nodes.valueIterator();
            while (it.next()) |n| f(ctx, n.*);
        }
    }

    // ---- hits ----

    /// Latch-free map read: a hit is counted and touched. The returned
    /// reference stays valid across eviction. Carries the lazy staleness
    /// gate: a node stamped under a superseded epoch is a MISS, so an
    /// operation that begins after a revalidation never adopts a stale node
    /// even while the drop pass still walks. On a write mount both words
    /// read `unarmed_epoch` and the compare always agrees.
    pub fn tryGet(self: *NodeCache, addr: u64) ?*CachedNode {
        const n = self.tryGetMapped(addr) orelse return null;
        if (n.epochStamp() != self.env.epoch.probe()) {
            stats.meta_kv_revalidate_stale_serves.inc();
            n.release();
            return null;
        }
        n.touch();
        stats.meta_kv_node_cache_hits.inc();
        return n;
    }

    // ---- demand paging ----

    const LoadCtx = struct {
        cache: *NodeCache,
        addr: u64,
        slot: ?ForestSlot,
        result: ?*CachedNode = null,
        led: bool = false,

        fn compute(ctx: *LoadCtx, _: Io) anyerror!void {
            ctx.led = true;
            ctx.result = try ctx.cache.loadLeader(ctx.addr, ctx.slot);
        }
    };

    /// Single-flight demand page: exactly one loader per address reads the
    /// extent; losers wait on the leader and re-check the map. `null` = a
    /// RETIRED extent: its disk image lags the RAM state that superseded
    /// it, so serving it would time-travel acked records — only a traversal
    /// holding a pre-SMO parent snapshot can reach one, and it restarts.
    pub fn load(self: *NodeCache, addr: u64) LoadError!?*CachedNode {
        return self.loadForSlot(addr, null);
    }

    /// `load` for a node the caller knows to belong to slot tree `slot`:
    /// the loaded node is stamped BEFORE it is published.
    pub fn loadForSlot(self: *NodeCache, addr: u64, slot: ?ForestSlot) LoadError!?*CachedNode {
        while (true) {
            if (self.tryGetMapped(addr)) |n| {
                if (n.epochStamp() != self.env.epoch.probe()) {
                    stats.meta_kv_revalidate_stale_serves.inc();
                    // Release the mapping so the demand page re-reads it;
                    // whoever holds a reference keeps its snapshot.
                    if (try self.mapRemoveIf(addr, n)) {
                        self.cached_bytes.sub(self.cfg.layout.node_size);
                        _ = self.mapped.fetchSub(1, .monotonic);
                        stats.meta_kv_revalidate_nodes_dropped.inc();
                    }
                    n.release();
                    continue;
                }
                n.touch();
                stats.meta_kv_node_cache_hits.inc();
                return n;
            }
            if (self.retired.contains(self.io, addr)) return null;
            var ctx: LoadCtx = .{ .cache = self, .addr = addr, .slot = slot };
            self.inflight.run(self.io, addr, &ctx, LoadCtx.compute) catch |e| {
                return @as(LoadError, @errorCast(e));
            };
            if (ctx.led) return ctx.result;
            // Cohort: the leader published (or found the extent retired);
            // re-check the map.
        }
    }

    /// Whether a load verdict can be a live-writer artifact rather than
    /// corruption: a loud tear classification or a checksum mismatch (a
    /// half-landed image). Everything else is structural, never retried.
    fn mayBeWriteRace(e: node.Error) bool {
        return e == error.CheckpointCoveredBsetAfterTear or e == error.ChecksumMismatch;
    }

    fn loadLeader(self: *NodeCache, addr: u64, slot: ?ForestSlot) LoadError!?*CachedNode {
        const gpa = self.gpa;
        const layout = self.cfg.layout;
        // The coherent pair, read ONCE and BEFORE the device read (the
        // two-word law): the epoch a node loaded now may claim, and the
        // tail its torn-tail classification uses. Reading it first keeps
        // the stamp conservative.
        const snap = self.env.epoch.loadSnapshot();
        const reader_mode = snap.epoch != epoch_core.unarmed_epoch;
        var attempt: u32 = 0;
        var buf: *SharedBuf = undefined;
        var loaded: node.LoadedNode = undefined;
        while (true) {
            buf = try SharedBuf.createExtent(gpa, layout.node_size);
            errdefer buf.release();
            stats.meta_kv_node_cache_loads.inc();
            try self.loader.readExtent(self.loader.ctx, addr, buf.data);
            loaded = node.verifyNodeExtentScreened(gpa, buf.data, layout, addr, snap.tail, null, null) catch |e| {
                // A reader's whole-extent read can race the writer's
                // single `write_at` append and see a torn frame followed
                // by a complete one; bounded re-reads close the window.
                if (reader_mode and attempt < node.READER_LOAD_RETRIES and mayBeWriteRace(e)) {
                    attempt += 1;
                    stats.meta_kv_reader_load_retries.inc();
                    buf.release();
                    continue;
                }
                return e;
            };
            break;
        }
        // Re-check after the read: a retire during our load means the bytes
        // we hold are the lagging image (retire marks the set BEFORE
        // dropping the mapping, so a loader that missed the map sees it).
        if (self.retired.contains(self.io, addr)) {
            loaded.deinit(gpa);
            buf.release();
            return null;
        }
        const block_map_leaf = loaded.header.level == 0 and loaded.header.tree_id == key.TREE_BLOCK_MAP;
        const n = try CachedNode.fromLoaded(self, loaded, buf, false);
        errdefer n.release();
        if (slot) |s| n.stampForestSlot(s);
        if (n.level > 0) {
            n.pin();
        } else if (block_map_leaf) {
            n.beginProbation();
        }
        try self.publishStamped(n, snap.epoch);
        stats.meta_kv_node_cache_misses.inc();
        return n;
    }

    /// `tryGet` or `load`, erroring on a retired address — for callers that
    /// resolve through current state by construction.
    pub fn get(self: *NodeCache, addr: u64) ChildError!*CachedNode {
        if (self.tryGet(addr)) |n| return n;
        return (try self.load(addr)) orelse error.RetiredExtent;
    }

    /// Resolve an interior pointer `(child_addr, child_node_seq)`: a seq
    /// mismatch at read is a concurrent SMO ⇒ `error.ReloadParent` (the
    /// traversal reloads the parent snapshot and retries under a
    /// `RetryBudget`).
    pub fn getChild(self: *NodeCache, child_addr: u64, child_node_seq: u64) ChildError!*CachedNode {
        const n = try self.get(child_addr);
        if (n.node_seq != child_node_seq) {
            n.release();
            return error.ReloadParent;
        }
        return n;
    }

    /// Verify a full extent image the caller holds (an SMO successor's
    /// freshly written image, a test's hand-built node) and build the node
    /// for it — NOT published; `publish` maps it. Consumes `buf`.
    pub fn buildNode(self: *NodeCache, addr: u64, buf: *SharedBuf, pinned: bool) LoadError!*CachedNode {
        errdefer buf.release();
        const loaded = try node.verifyNodeExtentScreened(self.gpa, buf.data, self.cfg.layout, addr, self.env.epoch.tail(), null, null);
        return CachedNode.fromLoaded(self, loaded, buf, pinned);
    }

    // ---- publish / retire / evict ----

    /// Publish a caller-built node (an SMO successor, a fresh root): it is
    /// this mount's own current state, so it belongs to the epoch in force.
    pub fn publish(self: *NodeCache, n: *CachedNode) Allocator.Error!void {
        return self.publishStamped(n, self.env.epoch.probe());
    }

    /// `publish` with the LOAD epoch stamped explicitly — the demand-page
    /// path: the epoch snapshotted BEFORE the device read, never the
    /// possibly newer one in force now. Replacing a mapping supersedes the
    /// old object and folds its floor into the dying floors (loud if it was
    /// not clean); a vacant insert charges one extent; the extent leaves
    /// `retired`; an evictable node enters the clock; then the sweep runs.
    pub fn publishStamped(self: *NodeCache, n: *CachedNode, epoch: u64) Allocator.Error!void {
        n.epoch_stamp.store(epoch, .monotonic);
        const evictable = !n.isPinned();
        if (try self.mapInsert(n.addr, n)) |old| {
            const outcome = old.state.supersede();
            self.noteNodeDyingFloor(old);
            if (outcome) |o| {
                if (o.was_dirty or o.was_freezing) {
                    // Displaced RAM records cannot be carried here (this is
                    // not the SMO path): loud, and the floor fold above keeps
                    // them replay-covered.
                    log.err("node cache publish over a NON-CLEAN mapping at 0x{x} (was_dirty={} was_freezing={} floor={d}): displaced open-delta records stay replay-covered via the dying-floor clamp", .{ n.addr, o.was_dirty, o.was_freezing, old.dirtyFloor() });
                }
            } else |_| {}
            old.release();
        } else {
            self.cached_bytes.add(self.cfg.layout.node_size);
            _ = self.mapped.fetchAdd(1, .monotonic);
        }
        self.retired.remove(self.io, n.addr);
        if (evictable) try self.clock.push(self.io, self.gpa, n.addr);
        try self.evictToBudget();
    }

    /// Sever an SMO-superseded node: mark its extent retired BEFORE
    /// dropping the mapping (so a racing loader's post-read re-check sees
    /// it), then release the charge. The caller owns the `supersede`
    /// transition and the pending-free of the extent.
    pub fn retire(self: *NodeCache, n: *CachedNode) Allocator.Error!void {
        // The retiring node's floor leaves the live-floor walk with this
        // mapping; its records are durable in the successor images, but
        // their REACHABILITY is not tied down until a ledger record written
        // after this point.
        self.noteNodeDyingFloor(n);
        try self.retired.insert(self.io, self.gpa, n.addr);
        if (try self.mapRemoveIf(n.addr, n)) {
            self.cached_bytes.sub(self.cfg.layout.node_size);
            _ = self.mapped.fetchSub(1, .monotonic);
        }
    }

    /// Clock sweep: second-chance FIFO; clean unpinned nodes evict by
    /// dropping the map reference (in-flight readers keep their snapshots
    /// alive); dirty/serializing nodes are pinned by the clean-only CAS;
    /// pinned entries are skipped outright. Bounded to two laps so an
    /// all-pinned cache cannot spin.
    ///
    /// Externally-held nodes are skipped: evicting a node some task still
    /// holds frees NO memory while severing the mapping, which forces the
    /// commit path's resolve → lock → revalidate window into reload-thrash
    /// (the tiny-budget finding). Only what dropping the map reference
    /// would actually free is reclaimed: `refs == 2` (the map's + the
    /// sweep's own probe).
    pub fn evictToBudget(self: *NodeCache) Allocator.Error!void {
        var attempts = 2 * (self.mapped.load(.monotonic) + 1);
        while (self.cached_bytes.load() > self.cfg.budget_bytes and attempts > 0) {
            attempts -= 1;
            const addr = self.clock.pop(self.io) orelse break;
            const n = self.tryGetMapped(addr) orelse continue; // stale clock entry
            defer n.release();
            if (n.isPinned()) continue; // pinned entries never re-enter the clock
            // acq_rel swap paired with `touch`'s store: the bit consumed here.
            if (n.ref_bit.swap(false, .acq_rel)) {
                try self.clock.push(self.io, self.gpa, addr); // second chance
                continue;
            }
            if (n.refCount() > 2) {
                try self.clock.push(self.io, self.gpa, addr); // held outside the cache
                continue;
            }
            if (n.state.tryEvict()) {
                // A state-clean node can still carry a floor: threshold
                // maintenance appended its bytes, but only a checkpoint's
                // barrier makes them durable-covered. The floor must survive
                // the eviction.
                self.noteNodeDyingFloor(n);
                if (try self.mapRemoveIf(addr, n)) {
                    self.cached_bytes.sub(self.cfg.layout.node_size);
                    _ = self.mapped.fetchSub(1, .monotonic);
                    stats.meta_kv_node_cache_evictions.inc();
                }
            } else {
                try self.clock.push(self.io, self.gpa, addr); // dirty / serializing: pinned until writeback
            }
        }
    }

    // ---- writeback ----

    /// Append the frozen delta of `n` to its extent tail — the §4.6 pt 1
    /// write OUTSIDE the node lock, on the serialized writeback/SMO task.
    /// `true` = appended (bookkeeping updated, freeze ended); `false` = the
    /// frame does not fit (`NodeFull` downgraded to a signal) — the caller
    /// compacts/splits instead. The frame is planned under a read lock
    /// (RAM only); the device write runs with no lock held.
    pub fn appendFrozen(self: *NodeCache, n: *CachedNode) AppendError!bool {
        const gpa = self.gpa;
        const layout = self.cfg.layout;
        const io = self.io;
        var plan: node.AppendFrame = undefined;
        var tail: usize = 0;
        {
            const d = n.lockRead(io);
            defer n.unlockRead(io);
            const frozen: FrozenDelta = d.frozen orelse return true;
            tail = d.tail_offset;
            if (tail + frozen.frame_len > layout.node_size) return false;
            const refs = try frozen.refs(gpa);
            defer gpa.free(refs);
            plan = node.appendBset(gpa, layout, .{ .node_addr = n.addr, .node_seq = n.node_seq, .tail_offset = tail }, refs, frozen.horizon, null) catch |e| switch (e) {
                error.NodeFull => return false,
                else => return e,
            };
        }
        defer plan.deinit(gpa);
        // The foreign-append probe, on a PARTITIONED volume only: a peer
        // that appended into this node's log while we held it cached would
        // be silently overwritten (the append writes at our remembered tail
        // and validates only the incarnation). One read of the destination
        // page turns it into a loud refusal. Solo volumes have no peers by
        // construction and pay nothing.
        if (!self.env.gate.load().isSolo()) {
            const probe = try SharedBuf.createExtent(gpa, layout.node_size);
            defer probe.release();
            try self.loader.readExtent(self.loader.ctx, n.addr, probe.data);
            switch (bset.probeTailPage(probe.data[tail..][0..bset.NODE_PAGE], n.node_seq, layout.frame)) {
                .clear => {},
                .ours => {
                    partition_refusals.fire("foreign append detected: the destination page already holds a verified frame of this incarnation");
                    return error.ForeignAppend;
                },
                .foreign => {
                    node.meta_kv_foreign_frame_overwrite_detected.inc();
                    return error.ForeignFrameOverwrite;
                },
            }
        }
        try self.loader.appendBset(self.loader.ctx, n.addr, tail, plan.frame);
        {
            const d = n.lockWrite(io);
            defer n.unlockWrite(io);
            d.tail_offset = plan.new_tail;
            if (d.frozen) |*f| f.deinit(gpa);
            d.frozen = null;
            // The promise covers the SMO the node's PENDING bytes need:
            // fitting in place means no SMO is owed and the budget returns.
            if (d.projectedLogEnd(layout, 0) <= layout.node_size) d.releasePromise();
        }
        _ = n.state.endFreeze();
        return true;
    }

    // ---- reader-side revalidation (the epoch stamp machinery) ----

    /// Declare this cache a coherent READER as of ledger record
    /// `ledger_seq` with durable tail `tail`: once per mount.
    pub fn armRevalidation(self: *NodeCache, ledger_seq: u64, tail: u64) error{AlreadyArmed}!void {
        if (!self.env.epoch.arm(ledger_seq)) return error.AlreadyArmed;
        self.env.epoch.advanceTail(tail);
        self.env.gate.setReader();
    }

    pub fn revalidationEpoch(self: *const NodeCache) u64 {
        return self.env.epoch.probe();
    }

    pub fn isRevalidating(self: *const NodeCache) bool {
        return self.env.epoch.isArmed();
    }

    /// Adopt a newer checkpoint and drop every stale-stamped mapping
    /// (pinned roots included: pinning is against eviction, not
    /// staleness). Inert on an un-armed cache and on a non-newer record.
    pub fn revalidate(self: *NodeCache, ledger_seq: u64, tail: u64) RevalidateOutcome {
        var out: RevalidateOutcome = .{ .from_epoch = self.env.epoch.probe(), .epoch = self.env.epoch.probe(), .tail = self.env.epoch.tail() };
        const moved = self.env.epoch.publish(tail, ledger_seq) orelse return out;
        out.advanced = true;
        out.from_epoch = moved.previous;
        out.epoch = moved.new;
        out.tail = self.env.epoch.tail();
        var victims: std.ArrayListUnmanaged(*CachedNode) = .empty;
        defer victims.deinit(self.gpa);
        self.forEachNode(&victims, struct {
            fn collect(list: *std.ArrayListUnmanaged(*CachedNode), n: *CachedNode) void {
                list.append(n.cache.gpa, n.retain()) catch n.release();
            }
        }.collect);
        for (victims.items) |n| {
            defer n.release();
            if (n.epochStamp() == moved.new) {
                out.retained += 1;
                continue;
            }
            if (n.dirtyFloor() != no_floor) {
                out.skipped_dirty += 1;
                revalidate_dirty_skips.fire("a revalidation drop pass met un-durable RAM records");
                continue;
            }
            self.noteNodeDyingFloor(n);
            const removed = self.mapRemoveIf(n.addr, n) catch false;
            if (removed) {
                self.cached_bytes.sub(self.cfg.layout.node_size);
                _ = self.mapped.fetchSub(1, .monotonic);
                out.dropped += 1;
                out.bytes_credited += self.cfg.layout.node_size;
                stats.meta_kv_revalidate_nodes_dropped.inc();
            }
        }
        return out;
    }

    // ---- the memory budget component ----

    /// Register as a sheddable `mem_budget` component: gauge = this cache's
    /// charge, floor 64 MiB, weight 1, shed = a checkpoint kick (an early
    /// run of the exact drain the cadence performs anyway — never-lossy by
    /// construction: dirty state is flushed, never dropped).
    pub fn registerBudgetComponent(self: *NodeCache, authority: *mem_budget.MemBudget) void {
        authority.register(mem_budget.Component.init(
            "kv_node_cache",
            KV_NODE_CACHE_FLOOR_BYTES,
            KV_NODE_CACHE_SHED_WEIGHT,
            .{ .gauge = &self.cached_bytes },
            .{ .ctx = self, .call = shedHook },
        ));
    }

    fn shedHook(ctx: *anyopaque, target: u64) void {
        const self: *NodeCache = @ptrCast(@alignCast(ctx));
        _ = self.shed_requests.fetchAdd(1, .monotonic);
        self.last_shed_target.store(target, .monotonic);
        if (self.checkpoint_kick) |h| h.call(h.ctx);
    }

    pub fn shedRequests(self: *const NodeCache) u64 {
        return self.shed_requests.load(.monotonic);
    }
};

// ---------------------------------------------------------------------------
// Tests — the ported contracts over the in-RAM loader.
// ---------------------------------------------------------------------------

const testing = std.testing;
const InodeValue = rec.InodeValue;
const InodeDelta = rec.InodeDelta;
const Record = rec.Record;

const NODE_SIZE: usize = node.MIN_NODE_SIZE;

fn iv(seed: u64) InodeValue {
    return .{
        .mode = 0o100644 ^ @as(u32, @truncate(seed & 0xFFF)),
        .uid = 1000 +% @as(u32, @truncate(seed)),
        .gid = 2000,
        .nlink = 1,
        .size = seed *% 4096,
        .atime = seed +% 1,
        .mtime = seed +% 2,
        .ctime = seed +% 3,
    };
}

/// A RAM volume + cache; node images are written straight into the volume.
const Vol = struct {
    ram: RamLoader,
    cache: *NodeCache,
    layout: NodeLayout,
    next_seq: u64 = 1,

    fn init(extents: u64, budget_nodes: u64) !Vol {
        const gpa = testing.allocator;
        var ram = try RamLoader.init(gpa, @intCast(extents * NODE_SIZE));
        errdefer ram.deinit(gpa);
        const layout = try NodeLayout.new(NODE_SIZE, null);
        var v: Vol = .{ .ram = ram, .cache = undefined, .layout = layout };
        v.cache = try NodeCache.init(gpa, testing.io, .{
            .layout = layout,
            .heap_base = 0,
            .budget_bytes = budget_nodes * NODE_SIZE,
            // Threshold writeback is a caller's enqueue; these contracts
            // drive freezes explicitly.
            .writeback_delta_bytes = std.math.maxInt(usize),
        }, undefined);
        return v;
    }

    /// The loader binds to the moved-in `ram` field.
    fn arm(self: *Vol) void {
        self.cache.loader = self.ram.loader();
    }

    fn deinit(self: *Vol) void {
        self.cache.deinit();
        self.ram.deinit(testing.allocator);
    }

    fn addrOf(ext: u64) u64 {
        return ext * NODE_SIZE;
    }

    /// Write a node image at extent `ext`: level, bounds, base records.
    fn writeNode(self: *Vol, ext: u64, node_seq: u64, level: u8, min_key: []const u8, max_key: []const u8, records: []const Record) !void {
        const gpa = testing.allocator;
        const refs = try gpa.alloc(RecordRef, records.len);
        defer gpa.free(refs);
        var horizon: u64 = 0;
        for (records, refs) |*r, *o| {
            o.* = r.ref();
            horizon = @max(horizon, r.seq);
        }
        var w = try node.writeNode(gpa, self.layout, .{ .node_addr = addrOf(ext), .node_seq = node_seq, .tree_id = key.TREE_INODES, .level = level, .min_key = min_key, .max_key = max_key }, refs, horizon, null);
        defer w.deinit(gpa);
        self.ram.write(w.node_addr, w.image);
    }

    /// A header-only leaf spanning the whole key space.
    fn writeEmptyLeaf(self: *Vol, ext: u64, node_seq: u64, level: u8) !void {
        try self.writeNode(ext, node_seq, level, "", &key.KEY_SPACE_MAX, &.{});
    }

    /// Apply ONE record through the commit-path shape and return its seq.
    fn apply(self: *Vol, n: *CachedNode, k: []const u8, kind: RecordKind, value: []const u8) !u64 {
        const gpa = testing.allocator;
        const seq = self.next_seq;
        self.next_seq += 1;
        var recs = [_]OwnedRec{try OwnedRec.fromRecordRef(gpa, .{ .key = k, .seq = seq, .kind = kind, .value = value })};
        const d = n.lockWrite(testing.io);
        defer n.unlockWrite(testing.io);
        try n.applyLocked(d, &recs, seq);
        return seq;
    }

    /// Freeze the open delta and append it so its records become
    /// bset-resident; no-op when the overlay is empty.
    fn freezeAndAppend(self: *Vol, n: *CachedNode) !void {
        const had_frozen = blk: {
            const d = n.lockWrite(testing.io);
            defer n.unlockWrite(testing.io);
            break :blk (try n.freezeLocked(d, self.layout)) != null;
        };
        if (had_frozen) try testing.expect(try self.cache.appendFrozen(n));
    }
};

fn expectLive(n: *CachedNode, k: []const u8, want: []const u8) !void {
    const s = n.snapshotLoad();
    defer s.release();
    var l = try s.value.lookup(testing.allocator, k);
    defer l.deinit();
    try testing.expectEqualSlices(u8, want, l.liveValue() orelse return error.TestExpectedLive);
}

fn expectGone(n: *CachedNode, k: []const u8) !void {
    const s = n.snapshotLoad();
    defer s.release();
    var l = try s.value.lookup(testing.allocator, k);
    defer l.deinit();
    try testing.expectEqual(@as(?[]const u8, null), l.liveValue());
}

/// Expect exactly one loud line from `tw`'s next fire: its per-site
/// rate limiter (one line per second) is reset so the contract is
/// deterministic across tests that fire the same site.
fn expectTripwireLog(tw: *counters.Tripwire) void {
    tw.last_log_s.store(0, .monotonic);
    log.testing.expectErrorLogs(1);
}

fn decodes() u64 {
    return rec.stats.meta_kv_fold_record_decodes.load();
}

// ---- derivation ties and knobs ----

test "node-cache budget tie: max(budget/16, 512 MiB); MB verbatim > PCT > derived; the registry rows" {
    const GiB = derive.GiB;
    try testing.expectEqual(512 * MiB, DEFAULT_CACHE_BUDGET_BYTES);
    // Below 8 GiB the 512 MiB floor holds (never regress below shipped).
    try testing.expectEqual(512 * MiB, resolveNodeCacheBudget(4 * GiB, null, null));
    try testing.expectEqual(512 * MiB, resolveNodeCacheBudget(8 * GiB, null, null));
    try testing.expectEqual(1 * GiB, resolveNodeCacheBudget(16 * GiB, null, null));
    try testing.expectEqual(derive.tenths(251_000_000_000, 7) / 16, resolveNodeCacheBudget(derive.tenths(251_000_000_000, 7), null, null));
    // Explicit MiB wins verbatim, even below the floor; PCT is of the budget.
    try testing.expectEqual(64 * MiB, resolveNodeCacheBudget(16 * GiB, 64, null));
    try testing.expectEqual(64 * MiB, resolveNodeCacheBudget(16 * GiB, 64, 50));
    try testing.expectEqual(8 * GiB, resolveNodeCacheBudget(16 * GiB, null, 50));
    try testing.expectEqual(16 * GiB, resolveNodeCacheBudget(16 * GiB, null, 100));
    // Multiply-first, as the reference's float form rounds: 4096 × 3 % = 122.88 → 122.
    try testing.expectEqual(@as(u64, 4096 * 3 / 100), resolveNodeCacheBudget(4096, null, 3));
    // Through the registry.
    const mb = knobs.lookup("SQUEEZEFS_META_NODE_CACHE_MB").?;
    try testing.expectEqual(@as(i128, 1), mb.kind.int.lo);
    try testing.expectEqual(knobs.MIB_MAX, mb.kind.int.hi);
    const pct = knobs.lookup("SQUEEZEFS_META_NODE_CACHE_PCT").?;
    try testing.expectEqual(@as(i128, 1), pct.kind.int.lo);
    try testing.expectEqual(@as(i128, 100), pct.kind.int.hi);
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(1 * GiB, nodeCacheBudgetFromEnv(&env, 16 * GiB));
    try env.put("SQUEEZEFS_META_NODE_CACHE_PCT", "25");
    try testing.expectEqual(4 * GiB, nodeCacheBudgetFromEnv(&env, 16 * GiB));
    try env.put("SQUEEZEFS_META_NODE_CACHE_MB", "300");
    try testing.expectEqual(300 * MiB, nodeCacheBudgetFromEnv(&env, 16 * GiB));
}

test "shard count tie: clamp(next_pow2(ceil(nodes / 64)), 16, 4096); 32 at the shipped 512 MiB / 256 KiB posture, the cap at the largest budgets" {
    try testing.expectEqual(@as(usize, 16), shardCount(0));
    try testing.expectEqual(@as(usize, 16), shardCount(1024));
    try testing.expectEqual(@as(usize, 32), shardCount(DEFAULT_CACHE_BUDGET_BYTES / node.DEFAULT_NODE_SIZE));
    try testing.expectEqual(@as(usize, 64), shardCount(2048 + 1));
    try testing.expectEqual(@as(usize, 1024), shardCount(65_536));
    try testing.expectEqual(@as(usize, 4096), shardCount(1 << 24));
    try testing.expectEqual(@as(usize, 4 * counters.max_stripes), MAX_INFLIGHT_LOADS);
    try testing.expectEqual(bset.NODE_PAGE, DEFAULT_WRITEBACK_DELTA_BYTES);
}

// ---- the partitioning gate (kv_node_cache_coherence_tests) ----

fn applyOne(n: *CachedNode, k: []const u8, seq: u64, kind: RecordKind, value: []const u8) ApplyError!void {
    var recs = [_]OwnedRec{OwnedRec.fromRecordRef(testing.allocator, .{ .key = k, .seq = seq, .kind = kind, .value = value }) catch return error.OutOfMemory};
    const d = n.lockWrite(testing.io);
    defer n.unlockWrite(testing.io);
    return n.applyLocked(d, &recs, seq);
}

test "the partition gate is inert on a solo volume and does not arbitrate leaves; a non-authority appender may not mutate an interior node; an armed reader may not mutate any node" {
    var vol = try Vol.init(4, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 1); // interior
    try vol.writeEmptyLeaf(1, 2, 0); // leaf
    const interior = (try vol.cache.load(Vol.addrOf(0))).?;
    defer interior.release();
    const leaf = (try vol.cache.load(Vol.addrOf(1))).?;
    defer leaf.release();
    try testing.expect(interior.isPinned() and !leaf.isPinned());
    // Solo: the appender IS the authority.
    try applyOne(interior, "k", 1, .put, "v");
    // Appender 2 of 4: leaves are its own business; structure is refused loud.
    try vol.cache.setAppender(4, 2);
    try applyOne(leaf, "k", 2, .put, "v");
    const before = partition_refusals.load();
    expectTripwireLog(&partition_refusals);
    try testing.expectError(error.NonAuthorityInterior, applyOne(interior, "k", 3, .put, "v"));
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 1, partition_refusals.load());
    // An armed reader mutates nothing.
    try vol.cache.armRevalidation(1, 0);
    try testing.expectError(error.AlreadyArmed, vol.cache.armRevalidation(2, 0));
    expectTripwireLog(&partition_refusals);
    try testing.expectError(error.ReaderMayNotMutate, applyOne(leaf, "k", 4, .put, "v"));
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 2, partition_refusals.load());
}

const TestLease = struct {
    verdict: LeaseVerdict,
    fn f(ctx: ?*anyopaque, _: ForestSlot, _: bool) LeaseVerdict {
        const self: *TestLease = @ptrCast(@alignCast(ctx.?));
        return self.verdict;
    }
};

test "a leaf of a slot this mount does not lease is refused typed and counted on the lease tripwire; an allowed slot applies" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    var lease: TestLease = .{ .verdict = .not_leased };
    vol.cache.installLeaseGate(.{ .ctx = &lease, .verdict = TestLease.f });
    leaf.stampForestSlot(key.guestForestSlot(3));
    const before = leaf_lease_refusals.load();
    expectTripwireLog(&leaf_lease_refusals);
    try testing.expectError(error.LeafLeaseRefused, applyOne(leaf, "k", 1, .put, "v"));
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 1, leaf_lease_refusals.load());
    try testing.expect(!leaf.state.isDirty());
    lease.verdict = .allowed;
    try applyOne(leaf, "k", 2, .put, "v");
    try expectLive(leaf, "k", "v");
}

test "an unarmed cache never revalidates and never drops; an advanced epoch drops every stale node including pinned ones and credits exactly; a stale stamp is never served by the hit path" {
    var vol = try Vol.init(4, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 1);
    try vol.writeEmptyLeaf(1, 2, 0);
    try vol.writeEmptyLeaf(2, 3, 0);
    for (0..3) |e| (try vol.cache.load(Vol.addrOf(e))).?.release();
    try testing.expectEqual(@as(u64, 0), vol.cache.revalidationEpoch());
    try testing.expect(!vol.cache.isRevalidating());
    const before = vol.cache.cachedBytes();
    const inert = vol.cache.revalidate(9_999, 4_242);
    try testing.expect(!inert.advanced);
    try testing.expectEqual(@as(u64, 0), inert.dropped);
    try testing.expectEqual(before, vol.cache.cachedBytes());
    for (0..3) |e| try testing.expect(vol.cache.contains(Vol.addrOf(e)));
    // Arm at 1: the mapped nodes carry the unarmed stamp, so they are stale
    // to the hit path now and dropped by the next advance.
    try vol.cache.armRevalidation(1, 10);
    try testing.expectEqual(@as(u64, 10), vol.cache.durableTail());
    const stale0 = stats.meta_kv_revalidate_stale_serves.load();
    try testing.expectEqual(@as(?*CachedNode, null), vol.cache.tryGet(Vol.addrOf(1)));
    try testing.expectEqual(stale0 + 1, stats.meta_kv_revalidate_stale_serves.load());
    // The same record again is inert; a newer one advances and drops all three.
    try testing.expect(!vol.cache.revalidate(1, 10).advanced);
    const out = vol.cache.revalidate(2, 20);
    try testing.expect(out.advanced);
    try testing.expectEqual(@as(u64, 1), out.from_epoch);
    try testing.expectEqual(@as(u64, 2), out.epoch);
    try testing.expectEqual(@as(u64, 3), out.dropped);
    try testing.expectEqual(@as(u64, 3 * NODE_SIZE), out.bytes_credited);
    try testing.expectEqual(@as(u64, 0), out.skipped_dirty);
    try testing.expectEqual(@as(u64, 0), vol.cache.cachedBytes());
    for (0..3) |e| try testing.expect(!vol.cache.contains(Vol.addrOf(e)));
    // A reload under the new epoch is current (stamped 2) and served.
    const n = (try vol.cache.load(Vol.addrOf(1))).?;
    defer n.release();
    try testing.expectEqual(@as(u64, 2), n.epochStamp());
    const hit = vol.cache.tryGet(Vol.addrOf(1)).?;
    hit.release();
}

// ---- D7 slimming (kv_fold_slimming_tests) ----

test "overlay head serves the hot parent probe with zero decodes, and follows every later apply" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    const k = key.inodeKey(42);
    const base = iv(7);
    _ = try vol.apply(leaf, &k, .put, &base.encode());
    var expected = base;
    var t: u64 = 1;
    while (t <= 5) : (t += 1) {
        const d = InodeDelta.times(1000 + t, 2000 + t);
        d.apply(&expected);
        _ = try vol.apply(leaf, &k, .delta, d.encode().slice());
    }
    const s = leaf.snapshotLoad();
    defer s.release();
    const d0 = decodes();
    const h0 = snapshot.stats.meta_kv_fold_head_serves.load();
    var got = try s.value.lookup(testing.allocator, &k);
    defer got.deinit();
    try testing.expectEqualSlices(u8, &expected.encode(), got.liveValue().?);
    try testing.expectEqual(h0 + 1, snapshot.stats.meta_kv_fold_head_serves.load());
    try testing.expectEqual(d0, decodes());
    // The head follows the next apply (one fold at write, N free reads).
    const dc = InodeDelta.ctime(9999);
    dc.apply(&expected);
    _ = try vol.apply(leaf, &k, .delta, dc.encode().slice());
    const s2 = leaf.snapshotLoad();
    defer s2.release();
    const d1 = decodes();
    var got2 = try s2.value.lookup(testing.allocator, &k);
    defer got2.deinit();
    try testing.expectEqualSlices(u8, &expected.encode(), got2.liveValue().?);
    try testing.expectEqual(d1, decodes());
}

test "overlay head serves a tombstone and a plain Put decode-free; a re-Put after a Delete is live again" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    const k = key.inodeKey(5);
    _ = try vol.apply(leaf, &k, .put, &iv(1).encode());
    _ = try vol.apply(leaf, &k, .delta, InodeDelta.times(3, 4).encode().slice());
    _ = try vol.apply(leaf, &k, .delete, "");
    const d0 = decodes();
    try expectGone(leaf, &k);
    try testing.expectEqual(d0, decodes());
    _ = try vol.apply(leaf, &k, .put, &iv(9).encode());
    try expectLive(leaf, &k, &iv(9).encode());
    try testing.expectEqual(d0, decodes());
    // An orphan delta (no base) folds to absent on its head too.
    const k2 = key.inodeKey(6);
    _ = try vol.apply(leaf, &k2, .delta, InodeDelta.ctime(1).encode().slice());
    try expectGone(leaf, &k2);
}

test "memo serves the second fold of a bset-resident key; the memo dies with the snapshot at the next swap" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    const k = key.inodeKey(77);
    const base = iv(11);
    _ = try vol.apply(leaf, &k, .put, &base.encode());
    var expected = base;
    var t: u64 = 1;
    while (t <= 4) : (t += 1) {
        const d = InodeDelta.times(t, 100 + t);
        d.apply(&expected);
        _ = try vol.apply(leaf, &k, .delta, d.encode().slice());
    }
    try vol.freezeAndAppend(leaf);
    const s = leaf.snapshotLoad();
    defer s.release();
    try testing.expectEqual(@as(usize, 0), s.value.overlayLen());
    const m0_hit = snapshot.stats.meta_kv_fold_memo_hits.load();
    const m0_miss = snapshot.stats.meta_kv_fold_memo_misses.load();
    var first = try s.value.lookup(testing.allocator, &k);
    defer first.deinit();
    try testing.expectEqualSlices(u8, &expected.encode(), first.liveValue().?);
    try testing.expectEqual(m0_miss + 1, snapshot.stats.meta_kv_fold_memo_misses.load());
    const d0 = decodes();
    var second = try s.value.lookup(testing.allocator, &k);
    defer second.deinit();
    try testing.expectEqualSlices(u8, &expected.encode(), second.liveValue().?);
    try testing.expectEqual(m0_hit + 1, snapshot.stats.meta_kv_fold_memo_hits.load());
    try testing.expectEqual(d0, decodes());
    // An apply to ANY key publishes a fresh snapshot whose memo starts empty.
    _ = try vol.apply(leaf, &key.inodeKey(78), .put, &iv(1).encode());
    const fresh = leaf.snapshotLoad();
    defer fresh.release();
    const m1_miss = snapshot.stats.meta_kv_fold_memo_misses.load();
    var refold = try fresh.value.lookup(testing.allocator, &k);
    defer refold.deinit();
    try testing.expectEqualSlices(u8, &expected.encode(), refold.liveValue().?);
    try testing.expectEqual(m1_miss + 1, snapshot.stats.meta_kv_fold_memo_misses.load());
}

test "memo probes are race-free across threads: concurrent lookups of one snapshot agree and the memo charge stays inside the fixed bound" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    // 16 bset-resident delta chains: more keys than memo cells.
    var expected: [16][rec.INODE_VALUE_LEN]u8 = undefined;
    for (0..16) |ino| {
        const k = key.inodeKey(ino);
        const base = iv(ino + 1);
        _ = try vol.apply(leaf, &k, .put, &base.encode());
        var want = base;
        var t: u64 = 1;
        while (t <= 3) : (t += 1) {
            const d = InodeDelta.times(ino * 10 + t, ino * 20 + t);
            d.apply(&want);
            _ = try vol.apply(leaf, &k, .delta, d.encode().slice());
        }
        expected[ino] = want.encode();
    }
    try vol.freezeAndAppend(leaf);
    const s = leaf.snapshotLoad();
    defer s.release();
    const g0 = snapshot.stats.meta_kv_fold_memo_bytes.load();
    const h0 = snapshot.stats.meta_kv_fold_memo_hits.load();
    const Worker = struct {
        fn run(snap: *Arc(NodeSnapshot), want: *const [16][rec.INODE_VALUE_LEN]u8, w: usize) !void {
            for (0..500) |i| {
                const ino = (i + w) % 16;
                var got = try snap.value.lookup(testing.allocator, &key.inodeKey(ino));
                defer got.deinit();
                try testing.expectEqualSlices(u8, &want[ino], got.liveValue().?);
            }
        }
    };
    var ts: [8]std.Thread = undefined;
    for (&ts, 0..) |*t, w| t.* = try std.Thread.spawn(.{}, Worker.run, .{ s, &expected, w });
    for (ts) |t| t.join();
    const populated = snapshot.stats.meta_kv_fold_memo_bytes.load() - g0;
    try testing.expect(populated > 0);
    try testing.expect(populated <= snapshot.FOLD_MEMO_CAPACITY * 512);
    try testing.expectEqual(snapshot.FOLD_MEMO_CAPACITY, s.value.memo.populated());
    try testing.expect(snapshot.stats.meta_kv_fold_memo_hits.load() > h0);
}

test "a mid-range rollback removal folds exactly: the stale head above the removed record is invalidated" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    const k = key.inodeKey(5);
    const base = iv(1);
    _ = try vol.apply(leaf, &k, .put, &base.encode());
    const d_failing = InodeDelta.times(111, 222);
    const s_failing = try vol.apply(leaf, &k, .delta, d_failing.encode().slice());
    const d_newer = InodeDelta.ctime(999);
    _ = try vol.apply(leaf, &k, .delta, d_newer.encode().slice());
    {
        const d = leaf.lockWrite(testing.io);
        defer leaf.unlockWrite(testing.io);
        try testing.expectEqual(@as(usize, 1), try leaf.removeOverlayRecordsLocked(d, &k, SeqSpan.stamped(s_failing, 1)));
    }
    var expected = base;
    d_newer.apply(&expected);
    try expectLive(leaf, &k, &expected.encode());
    try expectLive(leaf, &k, &expected.encode());
    const d_after = InodeDelta.times(333, 444);
    _ = try vol.apply(leaf, &k, .delta, d_after.encode().slice());
    d_after.apply(&expected);
    try expectLive(leaf, &k, &expected.encode());
}

test "a rollback that empties the overlay leaves a DIRTY word with nothing to write: the next freeze closes it (Clean, evictable) instead of re-collecting the node forever" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    const k = key.inodeKey(5);
    const s = try vol.apply(leaf, &k, .put, &iv(1).encode());
    try testing.expect(leaf.state.isDirty());
    {
        const d = leaf.lockWrite(testing.io);
        defer leaf.unlockWrite(testing.io);
        try testing.expectEqual(@as(usize, 1), try leaf.removeOverlayRecordsLocked(d, &k, SeqSpan.stamped(s, 1)));
        try testing.expect(leaf.state.isDirty());
        // The flush pass's freeze: nothing to write — and the word is closed.
        const ff = try leaf.flushFreezeLocked(d, vol.layout);
        try testing.expect(!ff.frozen);
        try testing.expect(!leaf.state.isDirty());
        try testing.expect(!leaf.state.isFreezing());
    }
    try testing.expectEqual(no_floor, leaf.dirtyFloor());
    try testing.expect(leaf.state.tryEvict());
}

const Mirror = struct { r: Record, frozen: bool = false, removed: bool = false };

test "property: over random histories (puts, deltas, deletes, freezes, rollback removals) head and memo serves equal the from-scratch fold" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0xd7_f01d);
    const rnd = prng.random();
    var case: usize = 0;
    while (case < 48) : (case += 1) {
        var vol = try Vol.init(2, 8);
        defer vol.deinit();
        vol.arm();
        try vol.writeEmptyLeaf(0, 1, 0);
        const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
        defer leaf.release();
        var mirror: std.ArrayListUnmanaged(Mirror) = .empty;
        defer {
            for (mirror.items) |*m| m.r.deinit(gpa);
            mirror.deinit(gpa);
        }
        const ops = 1 + rnd.uintLessThan(usize, 48);
        for (0..ops) |_| {
            const kidx = rnd.uintLessThan(u64, 4);
            const k = key.inodeKey(kidx);
            switch (rnd.uintLessThan(u8, 6)) {
                0 => {
                    const v = iv(rnd.int(u64)).encode();
                    const seq = try vol.apply(leaf, &k, .put, &v);
                    try mirror.append(gpa, .{ .r = try Record.put(gpa, &k, seq, &v) });
                },
                1 => {
                    const t = rnd.int(u64);
                    const d = InodeDelta.times(t, t +% 1);
                    const seq = try vol.apply(leaf, &k, .delta, d.encode().slice());
                    try mirror.append(gpa, .{ .r = try Record.delta(gpa, &k, seq, d) });
                },
                2 => {
                    const d = InodeDelta.ctime(rnd.int(u64));
                    const seq = try vol.apply(leaf, &k, .delta, d.encode().slice());
                    try mirror.append(gpa, .{ .r = try Record.delta(gpa, &k, seq, d) });
                },
                3 => {
                    const seq = try vol.apply(leaf, &k, .delete, "");
                    try mirror.append(gpa, .{ .r = try Record.delete(gpa, &k, seq) });
                },
                4 => {
                    try vol.freezeAndAppend(leaf);
                    for (mirror.items) |*m| if (!m.removed) {
                        m.frozen = true;
                    };
                },
                else => {
                    // The depth-newest LIVE overlay record of this key.
                    var cands: [64]usize = undefined;
                    var nc: usize = 0;
                    for (mirror.items, 0..) |m, i| {
                        if (!m.frozen and !m.removed and std.mem.eql(u8, m.r.key, &k) and nc < cands.len) {
                            cands[nc] = i;
                            nc += 1;
                        }
                    }
                    if (nc == 0) continue;
                    const depth = rnd.uintLessThan(usize, 3);
                    const pick = cands[nc - 1 - (depth % nc)];
                    const seq = mirror.items[pick].r.seq;
                    const d = leaf.lockWrite(testing.io);
                    const removed = try leaf.removeOverlayRecordsLocked(d, &k, SeqSpan.stamped(seq, 1));
                    leaf.unlockWrite(testing.io);
                    try testing.expectEqual(@as(usize, 1), removed);
                    mirror.items[pick].removed = true;
                },
            }
        }
        const s = leaf.snapshotLoad();
        defer s.release();
        for (0..4) |kidx| {
            const k = key.inodeKey(kidx);
            var survivors: std.ArrayListUnmanaged(RecordRef) = .empty;
            defer survivors.deinit(gpa);
            var i = mirror.items.len;
            while (i > 0) {
                i -= 1;
                const m = mirror.items[i];
                if (!m.removed and std.mem.eql(u8, m.r.key, &k)) try survivors.append(gpa, m.r.ref());
            }
            var want = try rec.foldNewestFirst(gpa, survivors.items);
            defer want.deinit(gpa);
            var got1 = try s.value.lookup(gpa, &k);
            defer got1.deinit();
            var got2 = try s.value.lookup(gpa, &k);
            defer got2.deinit();
            if (want.liveValue()) |wv| {
                try testing.expectEqualSlices(u8, wv, got1.liveValue() orelse return error.TestExpectedLive);
                try testing.expectEqualSlices(u8, wv, got2.liveValue() orelse return error.TestExpectedLive);
            } else {
                try testing.expectEqual(@as(?[]const u8, null), got1.liveValue());
                try testing.expectEqual(@as(?[]const u8, null), got2.liveValue());
            }
        }
    }
}

// ---- §5.7 accounting and eviction ----

test "a node's charge is extent + overlay (incl. heads) + memo; freezing releases the overlay charge; a dead snapshot's memo bytes leave the gauge exactly" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    const extent_only = vol.cache.cachedBytes();
    try testing.expectEqual(@as(u64, NODE_SIZE), extent_only);
    const k = key.inodeKey(9);
    _ = try vol.apply(leaf, &k, .put, &iv(2).encode());
    var t: u64 = 1;
    while (t <= 8) : (t += 1) _ = try vol.apply(leaf, &k, .delta, InodeDelta.times(t, t).encode().slice());
    const with_overlay = vol.cache.cachedBytes();
    try testing.expect(with_overlay > extent_only);
    try vol.freezeAndAppend(leaf);
    const after_freeze = vol.cache.cachedBytes();
    try testing.expect(after_freeze < with_overlay);
    try testing.expectEqual(@as(u64, NODE_SIZE), after_freeze);
    const g0 = snapshot.stats.meta_kv_fold_memo_bytes.load();
    const s = leaf.snapshotLoad();
    var l = try s.value.lookup(testing.allocator, &k);
    l.deinit();
    const with_memo = vol.cache.cachedBytes();
    const gauge_delta = snapshot.stats.meta_kv_fold_memo_bytes.load() - g0;
    try testing.expect(gauge_delta > 0);
    try testing.expectEqual(after_freeze + gauge_delta, with_memo);
    s.release();
    _ = try vol.apply(leaf, &key.inodeKey(10), .put, &iv(1).encode());
    try testing.expectEqual(g0, snapshot.stats.meta_kv_fold_memo_bytes.load());
}

test "tiny-budget churn keeps the memo gauge bounded through eviction; the budget converges; every memo byte leaves at teardown" {
    const gpa = testing.allocator;
    const EXTENTS: u64 = 48;
    const BUDGET_NODES: u64 = 4;
    var vol = try Vol.init(EXTENTS, BUDGET_NODES);
    vol.arm();
    for (0..EXTENTS) |ext| {
        const ino = 1000 + ext;
        var records = [_]Record{
            try Record.put(gpa, &key.inodeKey(ino), ext * 10 + 1, &iv(ino).encode()),
            try Record.delta(gpa, &key.inodeKey(ino), ext * 10 + 2, InodeDelta.times(1, 1)),
            try Record.delta(gpa, &key.inodeKey(ino), ext * 10 + 3, InodeDelta.times(2, 2)),
            try Record.delta(gpa, &key.inodeKey(ino), ext * 10 + 4, InodeDelta.times(3, 3)),
        };
        defer rec.deinitRecords(gpa, &records);
        try vol.writeNode(ext, ext + 1, 0, &key.inodeKey(ino), &key.inodeKey(ino), &records);
    }
    const g0 = snapshot.stats.meta_kv_fold_memo_bytes.load();
    var peak: u64 = 0;
    for (0..3) |_| {
        for (0..EXTENTS) |ext| {
            const n = (try vol.cache.load(Vol.addrOf(ext))).?;
            defer n.release();
            var want = iv(1000 + ext);
            var t: u64 = 1;
            while (t <= 3) : (t += 1) InodeDelta.times(t, t).apply(&want);
            try expectLive(n, &key.inodeKey(1000 + ext), &want.encode());
            try expectLive(n, &key.inodeKey(1000 + ext), &want.encode());
            peak = @max(peak, snapshot.stats.meta_kv_fold_memo_bytes.load() - g0);
        }
    }
    try testing.expect(peak > 0);
    try testing.expect(peak <= snapshot.FOLD_MEMO_CAPACITY * 512 * (BUDGET_NODES + 4));
    try testing.expect(vol.cache.cachedBytes() <= (BUDGET_NODES + 2) * NODE_SIZE);
    try testing.expect(stats.meta_kv_node_cache_evictions.load() > 0);
    // Quiescence: tearing the cache down releases every mapped snapshot —
    // the gauge returns to its baseline (deinit-owned, leak-free).
    vol.deinit();
    try testing.expectEqual(g0, snapshot.stats.meta_kv_fold_memo_bytes.load());
}

test "eviction skips externally held nodes (evicting them frees nothing) and reclaims them once released" {
    var vol = try Vol.init(8, 1);
    defer vol.deinit();
    vol.arm();
    for (0..6) |ext| {
        const ino = 100 + ext;
        var records = [_]Record{try Record.put(testing.allocator, &key.inodeKey(ino), 1, &iv(ino).encode())};
        defer rec.deinitRecords(testing.allocator, &records);
        try vol.writeNode(ext, ext + 1, 0, &key.inodeKey(ino), &key.inodeKey(ino), &records);
    }
    const held = (try vol.cache.load(Vol.addrOf(0))).?;
    for (1..6) |ext| (try vol.cache.load(Vol.addrOf(ext))).?.release();
    try testing.expect(vol.cache.contains(held.addr));
    try testing.expect(!held.state.isSuperseded());
    try expectLive(held, &key.inodeKey(100), &iv(100).encode());
    held.release();
    var lap: usize = 0;
    while (lap < 4) : (lap += 1) {
        for (1..6) |ext| (try vol.cache.load(Vol.addrOf(ext))).?.release();
        if (!vol.cache.contains(0)) break;
    }
    try testing.expect(!vol.cache.contains(0));
}

test "eviction respects the budget, pins dirty and interior nodes, and a held snapshot keeps serving" {
    var vol = try Vol.init(32, 4);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 1); // the pinned interior
    for (1..32) |ext| {
        const ino = ext;
        var records = [_]Record{try Record.put(testing.allocator, &key.inodeKey(ino), 1, &iv(ino).encode())};
        defer rec.deinitRecords(testing.allocator, &records);
        try vol.writeNode(ext, ext + 1, 0, &key.inodeKey(ino), &key.inodeKey(ino), &records);
    }
    (try vol.cache.load(Vol.addrOf(0))).?.release();
    const held_leaf = (try vol.cache.load(Vol.addrOf(17))).?;
    const held_snap = held_leaf.snapshotLoad();
    held_leaf.release();
    const ev0 = stats.meta_kv_node_cache_evictions.load();
    for (1..32) |ext| (try vol.cache.load(Vol.addrOf(ext))).?.release();
    try testing.expect(stats.meta_kv_node_cache_evictions.load() > ev0);
    try testing.expect(vol.cache.cachedBytes() <= 6 * NODE_SIZE);
    // The held snapshot serves by refcount, evicted or not.
    var l = try held_snap.value.lookup(testing.allocator, &key.inodeKey(17));
    defer l.deinit();
    try testing.expectEqualSlices(u8, &iv(17).encode(), l.liveValue().?);
    held_snap.release();
    // The interior stays mapped; the mapped census and the extent arithmetic agree.
    try testing.expect(vol.cache.contains(Vol.addrOf(0)));
    try testing.expectEqual(@as(u64, 1), vol.cache.pinned_nodes.load());
    try testing.expect(vol.cache.mappedCount() >= 1 and vol.cache.mappedCount() <= 6);
    try testing.expectEqual(Vol.addrOf(17), vol.cache.extentAddr(17));
    try testing.expectEqual(@as(u64, 17), vol.cache.addrExtent(Vol.addrOf(17)));
    try testing.expectEqual(NODE_SIZE, vol.cache.config().layout.node_size);
    // Dirty pinning: a dirty leaf survives the sweep.
    const dirty = (try vol.cache.load(Vol.addrOf(5))).?;
    _ = try vol.apply(dirty, &key.inodeKey(5), .put, &iv(999_999).encode());
    try testing.expect(dirty.state.isDirty());
    dirty.release();
    for (1..32) |ext| (try vol.cache.load(Vol.addrOf(ext))).?.release();
    try testing.expect(vol.cache.contains(Vol.addrOf(5)));
    const again = vol.cache.tryGet(Vol.addrOf(5)).?;
    defer again.release();
    try expectLive(again, &key.inodeKey(5), &iv(999_999).encode());
}

test "a dirty-floor node that leaves the map folds its floor into the dying floors; take drains, restore folds back" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    _ = try vol.apply(leaf, "k", .put, "v");
    try testing.expectEqual(@as(u64, 1), leaf.dirtyFloor());
    try testing.expectEqual(no_floor, vol.cache.takeDyingFloors());
    try vol.cache.retire(leaf);
    try testing.expect(!vol.cache.contains(Vol.addrOf(0)));
    try testing.expectEqual(@as(?*CachedNode, null), try vol.cache.load(Vol.addrOf(0)));
    try testing.expectEqual(@as(u64, 1), vol.cache.takeDyingFloors());
    try testing.expectEqual(no_floor, vol.cache.takeDyingFloors());
    vol.cache.restoreDyingFloors(1);
    try testing.expectEqual(@as(u64, 1), vol.cache.takeDyingFloors());
    // Take / restore of the node's own floor.
    try testing.expectEqual(@as(u64, 1), leaf.takeDirtyFloor());
    try testing.expectEqual(no_floor, leaf.dirtyFloor());
    leaf.restoreDirtyFloor(1);
    try testing.expectEqual(@as(u64, 1), leaf.dirtyFloor());
    leaf.release();
}

// ---- demand paging ----

test "single-flight collapses racing cold loads: 32 racers, exactly one device read, the rest are hits" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    var records = [_]Record{try Record.put(testing.allocator, &key.inodeKey(123), 1, &iv(123).encode())};
    defer rec.deinitRecords(testing.allocator, &records);
    try vol.writeNode(0, 1, 0, &key.inodeKey(123), &key.inodeKey(123), &records);
    const hits0 = stats.meta_kv_node_cache_hits.load();
    const misses0 = stats.meta_kv_node_cache_misses.load();
    const Racer = struct {
        fn run(cache: *NodeCache) !void {
            const n = (try cache.load(Vol.addrOf(0))).?;
            defer n.release();
            try expectLive(n, &key.inodeKey(123), &iv(123).encode());
        }
    };
    var ts: [32]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, Racer.run, .{vol.cache});
    for (ts) |t| t.join();
    try testing.expectEqual(@as(u64, 1), vol.ram.reads.load(.monotonic));
    try testing.expectEqual(misses0 + 1, stats.meta_kv_node_cache_misses.load());
    try testing.expectEqual(hits0 + 31, stats.meta_kv_node_cache_hits.load());
}

test "reads proceed while a writer holds the node lock: the snapshot path takes no lock" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    _ = try vol.apply(leaf, &key.inodeKey(150), .put, &iv(150).encode());
    const d = leaf.lockWrite(testing.io); // a writer mid-apply
    try testing.expectEqual(@as(?*const NodeDirty, null), leaf.tryLockRead(testing.io));
    try expectLive(leaf, &key.inodeKey(150), &iv(150).encode());
    const hit = vol.cache.tryGet(Vol.addrOf(0)).?;
    hit.release();
    try testing.expectEqual(@as(usize, 1), d.overlayLen());
    leaf.unlockWrite(testing.io);
}

test "getChild: a pointer whose child seq mismatches the image is ReloadParent; the retry budget is node.RETRY_BUDGET and exhausts typed" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 7, 0);
    const n = try vol.cache.getChild(Vol.addrOf(0), 7);
    n.release();
    try testing.expectError(error.ReloadParent, vol.cache.getChild(Vol.addrOf(0), 8));
    var budget: RetryBudget = .{};
    var spent: u32 = 0;
    while (true) : (spent += 1) budget.spend() catch break;
    try testing.expectEqual(node.RETRY_BUDGET, spent);
    try testing.expectError(error.RetryBudgetExhausted, budget.spend());
}

test "the torn-tail classifier through the cache: a torn tail bset is dropped and counted (priors intact); a valid bset after a tear with horizon ≤ durable tail fails loud" {
    const gpa = testing.allocator;
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    _ = try vol.apply(leaf, &key.inodeKey(1), .put, &iv(1).encode());
    try vol.freezeAndAppend(leaf);
    _ = try vol.apply(leaf, &key.inodeKey(2), .put, &iv(2).encode());
    try vol.freezeAndAppend(leaf);
    const tail = blk: {
        const d = leaf.lockRead(testing.io);
        defer leaf.unlockRead(testing.io);
        break :blk d.tail_offset;
    };
    try testing.expectEqual(bset.NODE_PAGE * 3, tail);
    try vol.cache.retire(leaf);
    leaf.release();
    // Tear the second frame's bset bytes (the frame header verifies, the
    // records do not): the load truncates there and counts one drop.
    const second = Vol.addrOf(0) + 2 * bset.NODE_PAGE;
    vol.ram.bytes[second + vol.layout.frameLen() + bset.BSET_HEADER_LEN + 3] ^= 0x55;
    const dropped0 = node.meta_kv_node_dropped_tail_bsets.load();
    var fresh = try NodeCache.init(gpa, testing.io, vol.cache.cfg, vol.ram.loader());
    defer fresh.deinit();
    const reloaded = (try fresh.load(Vol.addrOf(0))).?;
    try testing.expectEqual(dropped0 + 1, node.meta_kv_node_dropped_tail_bsets.load());
    try expectLive(reloaded, &key.inodeKey(1), &iv(1).encode());
    try expectGone(reloaded, &key.inodeKey(2));
    reloaded.release();
    // A verifying bset AFTER a torn unit whose horizon (2) ≤ the durable
    // tail is checkpoint-covered data after a tear: loud. Tear the FIRST
    // frame instead and leave the second intact.
    vol.ram.bytes[second + vol.layout.frameLen() + bset.BSET_HEADER_LEN + 3] ^= 0x55;
    const first = Vol.addrOf(0) + bset.NODE_PAGE;
    vol.ram.bytes[first + vol.layout.frameLen() + bset.BSET_HEADER_LEN + 3] ^= 0x55;
    var loud = try NodeCache.init(gpa, testing.io, vol.cache.cfg, vol.ram.loader());
    defer loud.deinit();
    loud.setDurableTail(5);
    try testing.expectError(error.CheckpointCoveredBsetAfterTear, loud.load(Vol.addrOf(0)));
    // With the tail below the horizon it is the un-checkpointed replay window: dropped and counted.
    var quiet = try NodeCache.init(gpa, testing.io, vol.cache.cfg, vol.ram.loader());
    defer quiet.deinit();
    const q = (try quiet.load(Vol.addrOf(0))).?;
    defer q.release();
    try testing.expectEqual(dropped0 + 3, node.meta_kv_node_dropped_tail_bsets.load());
    try expectGone(q, &key.inodeKey(1));
}

// ---- writeback ----

test "writeback appends frozen deltas until the node is full, then signals NodeFull; a fresh cache over the device serves everything appended" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    // A 64 KiB node takes exactly 15 one-page appends (kv_node.md §8).
    var appended: usize = 0;
    var ino: u64 = 1;
    while (true) : (ino += 1) {
        _ = try vol.apply(leaf, &key.inodeKey(ino), .put, &iv(ino).encode());
        const d = leaf.lockWrite(testing.io);
        const frozen = try leaf.freezeLocked(d, vol.layout);
        try testing.expect(frozen != null);
        leaf.unlockWrite(testing.io);
        if (!try vol.cache.appendFrozen(leaf)) break;
        appended += 1;
        try testing.expect(!leaf.state.isFreezing());
    }
    try testing.expectEqual(@as(usize, 15), appended);
    try testing.expectEqual(@as(u64, 15), vol.ram.appends.load(.monotonic));
    // The 16th stays parked: FREEZING with a frozen delta; a later freeze returns it.
    try testing.expect(leaf.state.isFreezing());
    {
        const d = leaf.lockWrite(testing.io);
        defer leaf.unlockWrite(testing.io);
        try testing.expect(d.hasFrozen());
        try testing.expectEqual(@as(usize, 1), d.frozenRecords().len);
        try testing.expectEqual(d.frozenRecords()[0].ref().encodedLen(), d.frozenBytes());
        try testing.expectEqual(@as(usize, 0), d.overlayBytes());
        try testing.expectEqual(NODE_SIZE, d.tailOffset());
        try testing.expect(d.projectedLogEnd(vol.layout, 0) > NODE_SIZE);
        const again = try leaf.freezeLocked(d, vol.layout);
        try testing.expect(again.? == &d.frozen.?);
    }
    // Everything frozen (appended or parked) still serves from RAM.
    for (1..ino + 1) |i| try expectLive(leaf, &key.inodeKey(i), &iv(i).encode());
    // The device holds the 15 appended records; a fresh cache serves them.
    var fresh = try NodeCache.init(testing.allocator, testing.io, vol.cache.cfg, vol.ram.loader());
    defer fresh.deinit();
    const reloaded = (try fresh.load(Vol.addrOf(0))).?;
    defer reloaded.release();
    for (1..16) |i| try expectLive(reloaded, &key.inodeKey(i), &iv(i).encode());
    try expectGone(reloaded, &key.inodeKey(16));
}

test "an encode failure inside freeze leaves the node freezable (the W-A restore) without the RES-22 tripwire" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    for (0..24) |i| _ = try vol.apply(leaf, &key.inodeKey(i), .put, &iv(i).encode());
    const t0 = freeze_latched_recovered.load();
    test_freeze_encode_fail.store(1, .monotonic);
    defer test_freeze_encode_fail.store(0, .monotonic);
    {
        const d = leaf.lockWrite(testing.io);
        defer leaf.unlockWrite(testing.io);
        try testing.expectError(error.InjectedFreezeFailure, leaf.freezeLocked(d, vol.layout));
        try testing.expectEqual(@as(u64, 0), test_freeze_encode_fail.load(.monotonic));
        // The word was restored: dirty, not freezing; the overlay untouched.
        try testing.expect(leaf.state.isDirty() and !leaf.state.isFreezing());
        try testing.expectEqual(@as(usize, 24), d.overlayLen());
    }
    // The same records serialize one tick later — no AlreadyFreezing, ever.
    try vol.freezeAndAppend(leaf);
    try testing.expectEqual(t0, freeze_latched_recovered.load());
    for (0..24) |i| try expectLive(leaf, &key.inodeKey(i), &iv(i).encode());
    try testing.expect(!leaf.state.isDirty());
}

test "an over-cap record fails the freeze typed (ValueTooLarge) and leaves the node freezable" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    const big = try testing.allocator.alloc(u8, vol.layout.recordValueCapOf() + 1);
    defer testing.allocator.free(big);
    @memset(big, 0xAB);
    const seq = try vol.apply(leaf, &key.inodeKey(1), .put, big);
    {
        const d = leaf.lockWrite(testing.io);
        defer leaf.unlockWrite(testing.io);
        try testing.expectError(error.ValueTooLarge, leaf.freezeLocked(d, vol.layout));
        try testing.expect(leaf.state.isDirty() and !leaf.state.isFreezing());
        // The commit path's rollback strikes the record; the node freezes clean.
        try testing.expectEqual(@as(usize, 1), try leaf.removeOverlayRecordsLocked(d, &key.inodeKey(1), SeqSpan.stamped(seq, 1)));
        try testing.expectEqual(@as(?*const FrozenDelta, null), try leaf.freezeLocked(d, vol.layout));
    }
}

test "a hand-latched freeze recovers loud with exactly one tripwire; a clean follow-up pass is silent" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    for (0..24) |i| _ = try vol.apply(leaf, &key.inodeKey(i), .put, &iv(i).encode());
    // The wedge shape: FREEZING set with no frozen delta, overlay non-empty.
    try leaf.state.beginForcedFreeze();
    const t0 = freeze_latched_recovered.load();
    expectTripwireLog(&freeze_latched_recovered);
    try vol.freezeAndAppend(leaf);
    try log.testing.expectAllFired();
    try testing.expectEqual(t0 + 1, freeze_latched_recovered.load());
    for (0..24) |i| try expectLive(leaf, &key.inodeKey(i), &iv(i).encode());
    _ = try vol.apply(leaf, &key.inodeKey(99), .put, &iv(99).encode());
    try vol.freezeAndAppend(leaf);
    try testing.expectEqual(t0 + 1, freeze_latched_recovered.load());
    // The forced-freeze shape with an EMPTY overlay heals through endFreeze.
    try leaf.state.beginForcedFreeze();
    expectTripwireLog(&freeze_latched_recovered);
    {
        const d = leaf.lockWrite(testing.io);
        defer leaf.unlockWrite(testing.io);
        try testing.expectEqual(@as(?*const FrozenDelta, null), try leaf.freezeLocked(d, vol.layout));
    }
    try log.testing.expectAllFired();
    try testing.expectEqual(node_state_core.LifecycleState.clean, leaf.state.state());
}

test "a freeze refusal is a protocol error, typed apart from corruption" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    for (0..8) |i| _ = try vol.apply(leaf, &key.inodeKey(i), .put, &iv(i).encode());
    _ = try leaf.state.supersede();
    const d = leaf.lockWrite(testing.io);
    defer leaf.unlockWrite(testing.io);
    try testing.expectError(error.FreezeRefusedSuperseded, leaf.freezeLocked(d, vol.layout));
    // Every class is its own name; none is the corruption class.
    inline for (.{ error.FreezeRefusedNotDirty, error.FreezeRefusedAlreadyFreezing, error.FreezeRefusedSuperseded }) |e| {
        try testing.expect(e != error.Corrupt);
    }
}

test "a flush that appends a PARKED frozen delta keeps the floor of the records applied since" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    for (0..24) |i| _ = try vol.apply(leaf, &key.inodeKey(i), .put, &iv(i).encode());
    try vol.freezeAndAppend(leaf);
    _ = leaf.takeDirtyFloor(); // the covering checkpoint took the base's floor
    try testing.expectEqual(no_floor, leaf.dirtyFloor());
    // The parked delta: records 100..108 frozen by hand (a failed SMO's
    // freeze — the floor untouched, exactly as the SMO leaves it).
    for (100..108) |i| _ = try vol.apply(leaf, &key.inodeKey(i), .put, &iv(i).encode());
    {
        const d = leaf.lockWrite(testing.io);
        defer leaf.unlockWrite(testing.io);
        try testing.expect((try leaf.freezeLocked(d, vol.layout)) != null);
    }
    const floor_before = leaf.dirtyFloor();
    try testing.expect(floor_before != no_floor);
    // Newer records land in the OPEN delta while the frozen one is parked.
    for (0..12) |i| _ = try vol.apply(leaf, &key.inodeKey(i), .delete, "");
    const kept0 = stats.meta_kv_flush_floor_kept.load();
    // The checkpoint's flush step: the lock window (freeze — here the parked
    // delta comes back — take the floor, keep the open records' floor), then
    // the append outside the lock.
    const FlushStep = struct {
        fn run(v: *Vol, n: *CachedNode) !bool {
            const ff = blk: {
                const d = n.lockWrite(testing.io);
                defer n.unlockWrite(testing.io);
                break :blk try n.flushFreezeLocked(d, v.layout);
            };
            if (!ff.frozen) return false;
            return try v.cache.appendFrozen(n);
        }
    };
    try testing.expect(try FlushStep.run(&vol, leaf));
    // The node stays DIRTY for the records it did not take.
    try testing.expect(leaf.dirtyFloor() != no_floor);
    try testing.expectEqual(kept0 + 1, stats.meta_kv_flush_floor_kept.load());
    try testing.expect(try FlushStep.run(&vol, leaf));
    try testing.expectEqual(no_floor, leaf.dirtyFloor());
    try testing.expectEqual(node_state_core.LifecycleState.clean, leaf.state.state());
    // Every record — the parked delta's and the newer deletes — is in the
    // DEVICE image: a fresh cache over the same bytes reads them back.
    var fresh = try NodeCache.init(testing.allocator, testing.io, vol.cache.cfg, vol.ram.loader());
    defer fresh.deinit();
    const reopened = (try fresh.load(Vol.addrOf(0))).?;
    defer reopened.release();
    for (0..12) |i| try expectGone(reopened, &key.inodeKey(i));
    for (12..24) |i| try expectLive(reopened, &key.inodeKey(i), &iv(i).encode());
    for (100..108) |i| try expectLive(reopened, &key.inodeKey(i), &iv(i).encode());
}

test "a failed append leaves the frozen delta parked and the node serializing; the next append lands it" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    _ = try vol.apply(leaf, &key.inodeKey(1), .put, &iv(1).encode());
    {
        const d = leaf.lockWrite(testing.io);
        defer leaf.unlockWrite(testing.io);
        try testing.expect((try leaf.freezeLocked(d, vol.layout)) != null);
    }
    vol.ram.fail_appends.store(true, .monotonic);
    try testing.expectError(error.Io, vol.cache.appendFrozen(leaf));
    try testing.expect(leaf.state.isFreezing());
    try expectLive(leaf, &key.inodeKey(1), &iv(1).encode());
    vol.ram.fail_appends.store(false, .monotonic);
    try testing.expect(try vol.cache.appendFrozen(leaf));
    try testing.expectEqual(node_state_core.LifecycleState.clean, leaf.state.state());
}

test "takeOverlay moves the records out with their heads, releases the charge and the promise" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    _ = try vol.apply(leaf, &key.inodeKey(1), .put, &iv(1).encode());
    _ = try vol.apply(leaf, &key.inodeKey(1), .delta, InodeDelta.ctime(5).encode().slice());
    const d = leaf.lockWrite(testing.io);
    defer leaf.unlockWrite(testing.io);
    d.promise(3, 1000);
    try testing.expectEqual(@as(u64, 3), vol.cache.heapPromised());
    try testing.expectEqual(@as(usize, 1000), d.promiseBasis());
    d.notePromiseGrowth(10);
    d.retractPromiseGrowth(4);
    try testing.expectEqual(@as(usize, 6), d.promiseAdded());
    d.retractPromise(1);
    try testing.expectEqual(@as(u64, 2), d.promisedExtents());
    try testing.expect(vol.cache.cachedBytes() > NODE_SIZE);
    const taken = try d.takeOverlay(testing.allocator);
    defer {
        for (taken) |*r| r.deinit();
        testing.allocator.free(taken);
    }
    try testing.expectEqual(@as(usize, 2), taken.len);
    try testing.expect(taken[1].folded != null);
    try testing.expectEqual(@as(usize, 0), d.overlayLen());
    try testing.expectEqual(@as(u64, NODE_SIZE), vol.cache.cachedBytes());
    try testing.expectEqual(@as(u64, 0), vol.cache.heapPromised());
    try testing.expectEqual(@as(u64, 0), d.promisedExtents());
    // The published snapshot keeps serving the pre-swap view.
    var v = iv(1);
    v.ctime = 5;
    const enc = v.encode();
    try expectLive(leaf, &key.inodeKey(1), &enc);
}

test "publish over a NON-CLEAN mapping is loud and folds its floor into the dying floors; the displaced object refuses further applies" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const first = (try vol.cache.load(Vol.addrOf(0))).?;
    defer first.release();
    _ = try vol.apply(first, "k", .put, "v");
    // A second object for the same extent (an SMO successor's shape, built
    // from the device image and published over the stale mapping).
    const img = try SharedBuf.createExtent(testing.allocator, NODE_SIZE);
    @memcpy(img.data, vol.ram.bytes[0..NODE_SIZE]);
    const second = try vol.cache.buildNode(Vol.addrOf(0), img, false);
    defer second.release();
    log.testing.expectErrorLogs(1);
    try vol.cache.publish(second);
    try log.testing.expectAllFired();
    try testing.expect(first.state.isSuperseded());
    try testing.expectEqual(@as(u64, 1), vol.cache.takeDyingFloors());
    // One extent charged for the mapping; the displaced overlay still charges until it dies.
    try testing.expectEqual(@as(u64, NODE_SIZE), vol.cache.cachedBytes() - first.dirty.charged);
    const mapped = vol.cache.tryGet(Vol.addrOf(0)).?;
    defer mapped.release();
    try testing.expect(mapped == second);
    try testing.expectError(error.Superseded, applyOne(first, "k", 9, .put, "v"));
    // Publishing over a CLEAN mapping is quiet.
    const img2 = try SharedBuf.createExtent(testing.allocator, NODE_SIZE);
    @memcpy(img2.data, vol.ram.bytes[0..NODE_SIZE]);
    const third = try vol.cache.buildNode(Vol.addrOf(0), img2, false);
    defer third.release();
    try vol.cache.publish(third);
    try testing.expect(second.state.isSuperseded());
    try testing.expectEqual(no_floor, vol.cache.takeDyingFloors());
}

test "the foreign-append probe: a peer's frame at our remembered tail is refused on a partitioned volume and never consulted on a solo one" {
    const gpa = testing.allocator;
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    try vol.writeEmptyLeaf(0, 1, 0);
    const leaf = (try vol.cache.load(Vol.addrOf(0))).?;
    defer leaf.release();
    const peer_rec = [_]Record{try Record.put(gpa, &key.inodeKey(9), 9, &iv(9).encode())};
    defer gpa.free(peer_rec[0].key);
    defer gpa.free(peer_rec[0].value);
    const refs = [_]RecordRef{peer_rec[0].ref()};
    // Solo: the same shape appends without complaint.
    {
        const frame = try node.encodeBsetFrame(gpa, vol.layout.frame, NODE_SIZE, leaf.node_seq, &refs, 9);
        defer gpa.free(frame);
        vol.ram.write(Vol.addrOf(0) + bset.NODE_PAGE, frame);
        _ = try vol.apply(leaf, &key.inodeKey(10), .put, &iv(10).encode());
        try vol.freezeAndAppend(leaf);
    }
    // Partitioned (appender 1 of 2): a peer's frame at our tail refuses loud.
    try vol.cache.setAppender(2, 1);
    const tail = blk: {
        const d = leaf.lockRead(testing.io);
        defer leaf.unlockRead(testing.io);
        break :blk d.tail_offset;
    };
    const frame = try node.encodeBsetFrame(gpa, vol.layout.frame, NODE_SIZE, leaf.node_seq, &refs, 9);
    defer gpa.free(frame);
    vol.ram.write(Vol.addrOf(0) + tail, frame);
    _ = try vol.apply(leaf, &key.inodeKey(11), .put, &iv(11).encode());
    {
        const d = leaf.lockWrite(testing.io);
        defer leaf.unlockWrite(testing.io);
        try testing.expect((try leaf.freezeLocked(d, vol.layout)) != null);
    }
    const before = partition_refusals.load();
    expectTripwireLog(&partition_refusals);
    try testing.expectError(error.ForeignAppend, vol.cache.appendFrozen(leaf));
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 1, partition_refusals.load());
    try testing.expectEqual(@as(u64, 1), vol.ram.appends.load(.monotonic));
}

test "the cache registers as a sheddable mem_budget component: gauge, 64 MiB floor, weight 1; a Red tick kicks the checkpoint" {
    var vol = try Vol.init(2, 8);
    defer vol.deinit();
    vol.arm();
    var authority: mem_budget.MemBudget = .{};
    vol.cache.registerBudgetComponent(&authority);
    const comp = authority.components()[0];
    try testing.expectEqualStrings("kv_node_cache", comp.name);
    try testing.expectEqual(64 * MiB, comp.floor);
    try testing.expectEqual(@as(u64, 1), comp.weight);
    const Kick = struct {
        var kicks: u64 = 0;
        fn call(_: *anyopaque) void {
            kicks += 1;
        }
    };
    vol.cache.installCheckpointKick(.{ .ctx = vol.cache, .call = Kick.call });
    // Fake pressure: the gauge reads 100 MiB against a 100 MiB budget ⇒ Red
    // ⇒ a shed call with a target above the floor.
    vol.cache.cached_bytes.set(100 * MiB);
    authority.tickInner(100 * MiB, 0, 0);
    try testing.expectEqual(mem_budget.Level.red, authority.level());
    try testing.expectEqual(@as(u64, 1), vol.cache.shedRequests());
    try testing.expectEqual(@as(u64, 1), Kick.kicks);
    try testing.expectEqual(@as(u64, 85 * MiB), vol.cache.last_shed_target.load(.monotonic));
    vol.cache.cached_bytes.set(0);
}
