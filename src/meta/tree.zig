//! The CoW btree over the node cache: traversal, the commit-side apply and
//! mount replay (charter §5.1 "Node" — interior values `(child_addr,
//! child_node_seq)`, a seq mismatch at read is a concurrent SMO ⇒ reload
//! the parent snapshot and retry (bounded); §5.3 "SMOs" — writers resolve
//! → lock → revalidate → retry (`commit_smo_retries`), replay is
//! two-phase; §2 lock order 4b — the commit path locks LEAVES only; survey
//! `docs/porting/kv_tree_cache_commit_survey.md` §2.11, §3.7, §7 row 6,
//! §7.1; the reference's `tree.rs` `KvTree`).
//!
//! - `descend` is LATCH-FREE: every step reads an arc-swapped snapshot and
//!   takes no node lock. A stale route (root retired / root seq / routing
//!   hole / child retired / child seq) is a racing SMO's swap window and
//!   restarts the walk from the (possibly swapped) root, yielding between
//!   attempts, bounded by `node.RETRY_BUDGET`; the per-reason tallies ride
//!   the exhaustion error because a budget exhaustion is a protocol bug
//!   and the tallies make it self-describing (the K7 storm's
//!   `[0,0,0,256,0]` — every restart on a retired child — WAS the
//!   retire-before-flip ordering bug).
//! - `applyAtSeqClass` is THE ONE lock-window primitive both live commits
//!   and replay use: lock → revalidate (not superseded ∧ `min ≤ key ≤ max`
//!   — SUPERSEDED is set before any mapping change, so it IS the "address
//!   still bound to this object" check) → seq assigned INSIDE the window
//!   (minted, reserved by the caller's stamper, or the replayed record's
//!   own with the per-key LWW gate) → `applyLocked` → unlock → threshold
//!   enqueue. A failed revalidation is `.stale`, counted on
//!   `meta_kv_commit_smo_retries`; the caller re-resolves and retries,
//!   bounded by the same budget.
//! - Interior locks belong to the serialized SMO task (`smo.zig`,
//!   `leaf_merge.zig`); this file touches one only for mount replay's
//!   phase 1, which runs single-threaded before any SMO can exist.
//!
//! The SMO state machine and leaf merge live in `smo.zig` /
//! `leaf_merge.zig`; this file is what they build on.

const std = @import("std");
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const key = @import("key.zig");
const rec = @import("record.zig");
const node = @import("node.zig");
const node_cache = @import("node_cache.zig");
const node_seq = @import("node_seq.zig");
const snapshot = @import("node_snapshot.zig");
const journal = @import("journal.zig");
const ledger = @import("ledger.zig");
const alloc_ext = @import("alloc_ext.zig");
const derive = @import("../core/derive.zig");
const arc_swap = @import("../sync/arc_swap.zig");
const leaf_merge = @import("leaf_merge.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;

pub const Arc = arc_swap.Arc;
pub const ArcSwap = arc_swap.ArcSwap;
pub const NodeCache = node_cache.NodeCache;
pub const CachedNode = node_cache.CachedNode;
pub const NodeDirty = node_cache.NodeDirty;
pub const OwnedRec = node_cache.OwnedRec;
pub const NodeSeqHandle = node_seq.NodeSeqHandle;
pub const RecordKind = rec.RecordKind;
pub const ForestSlot = key.ForestSlot;
pub const KEY_SPACE_MAX = key.KEY_SPACE_MAX;

// ---------------------------------------------------------------------------
// Observability (survey §5.5 names).
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// `meta_kv_commit_smo_retries`: lock-revalidate failures (an SMO
    /// raced the resolve → lock window).
    pub var meta_kv_commit_smo_retries: counters.ShardedCounter = .{};
    /// A threshold enqueue found the bounded maintenance queue full: the
    /// node stays dirty and the checkpoint's dirty walk flushes it (a
    /// counted deferral, never a loss).
    pub var meta_kv_maintenance_queue_full: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

pub const Error = node_cache.ChildError || rec.FoldError || error{
    /// A traversal or writer retry budget exhausted (`node.RETRY_BUDGET`):
    /// revalidation never passed — a routing loop, an SMO protocol bug.
    RetryBudgetExhausted,
    /// A content key must be non-empty and sort strictly below
    /// `KEY_SPACE_MAX`; an interior separator at-or-below it.
    InvalidKey,
    /// A root pointer naming a node of another incarnation or tree, a
    /// malformed interior value, a double supersede.
    Corrupt,
    /// The node-seq mint space is exhausted (remount for a fresh one).
    SpaceExhausted,
    /// The mutation gates refused (a reader mount, an unleased slot tree).
    PartitionRefused,
    /// A value above the per-volume cap `min(64 KiB, node_size/4) + 256`.
    ValueTooLarge,
    /// A tombstone carrying a value: the one record shape the frame
    /// encoder asserts against — refused at the apply, never discovered at
    /// the flush (a replay that mislabels a frame's kind fails typed).
    InvalidRecord,
};

// ---------------------------------------------------------------------------
// Root pointer, seq plans, outcomes.
// ---------------------------------------------------------------------------

/// A tree root pointer: the ledger's `(node_addr, node_seq)` pair.
pub const RootPtr = struct {
    addr: u64,
    seq: u64,
};

/// A record's stamp: its seq and the §4.6 pt 2 floor contribution — the
/// containing journal entry's START (FIND-SMO-TAIL §1b: a mid-entry tail
/// drops the entry's ≥-tail acked records at replay; a fresh single-record
/// apply is its own entry start).
pub const Stamp = struct {
    seq: u64,
    entry_start: u64,
};

/// A caller that reserves its journal range INSIDE the leaf lock window:
/// `stamp` runs after revalidation passes, with the lock held, and answers
/// the record's seq and entry start — the conveyor's reserve-in-window
/// discipline for one record (§5.2: per-key seq order equals RAM apply
/// order only because the reservation is taken under the lock).
pub const Stamper = struct {
    ctx: *anyopaque,
    /// The registry's open list grows only past its pre-sized capacity;
    /// that allocation is the one way a reservation can fail.
    stamp: *const fn (ctx: *anyopaque) Allocator.Error!Stamp,
};

/// How `applyAtSeqClass` assigns the record's seq inside the window.
pub const SeqPlan = union(enum) {
    /// Mint from the tree's seq handle (the record IS its own entry start)
    /// — the standalone shape every tree-level contract drives.
    mint,
    /// Mount replay: the record's ORIGINAL seq and its entry start, with
    /// the per-key LWW gate (a seq at-or-below the node's newest for the
    /// key is already folded in — skipped, identical by the fold theorem).
    replay: Stamp,
    /// A live commit whose reservation the caller takes under the lock.
    reserve: Stamper,
};

/// What `applyAtSeqClass` did.
pub const ApplyOutcome = enum {
    /// Records applied and the snapshot swapped (or, in replay, already
    /// folded in).
    applied,
    /// Revalidation failed: the leaf was superseded or the key left its
    /// range between resolution and lock (an SMO) — re-resolve and retry.
    stale,
};

/// A live `(key, value)` a range walk returned; owned copies.
pub const KeyValue = struct {
    key: []u8,
    value: []u8,

    pub fn deinit(self: *KeyValue, gpa: Allocator) void {
        gpa.free(self.key);
        gpa.free(self.value);
        self.* = undefined;
    }
};

pub fn deinitKeyValues(gpa: Allocator, list: *std.ArrayList(KeyValue)) void {
    for (list.items) |*kv| kv.deinit(gpa);
    list.deinit(gpa);
}

/// What a read cost the trees: `nodes` fetched on the walk's descents
/// (root to leaf, once per leaf a range enters, restarts included),
/// `entries` copied out of the leaves — the read amplification's
/// numerator (a merged range reads entries it then drops) — and `sources`,
/// the trees the read consulted.
pub const WalkCost = struct {
    nodes: u64 = 0,
    entries: u64 = 0,
    sources: u64 = 0,
};

// ---------------------------------------------------------------------------
// The maintenance queue — a bounded lock-free MPMC ring of node addresses.
// ---------------------------------------------------------------------------

/// Maintenance-queue capacity floor: the smallest cache (16 shards × 1
/// node) still queues a storm's worth of distinct nodes.
pub const MAINTENANCE_QUEUE_FLOOR: u64 = 64;
/// Capacity cap: one slot per node of the largest budget (`budget /
/// node_size` at 16 GiB / 64 KiB = 2^18); 2^20 bounds the table at 16 MiB.
pub const MAINTENANCE_QUEUE_CAP: u64 = 1 << 20;

/// `clamp(next_pow2(budget_nodes), 64, 2^20)`: every node the cache can
/// hold queues once legitimately (an address is re-queued only after its
/// pop); duplicates beyond that are benign to drop — the flush pass's
/// dirty walk flushes what the queue could not name.
pub fn maintenanceQueueCapacity(budget_nodes: u64) usize {
    return @intCast(derive.clamp(derive.nextPow2(@max(budget_nodes, 1)), MAINTENANCE_QUEUE_FLOOR, MAINTENANCE_QUEUE_CAP));
}

/// Read `addr`'s extent through the loader and build an UNPUBLISHED node
/// for it — an SMO successor's load-back (verifies the fresh image under
/// `slot`'s screen and builds its snapshot zero-copy), a tree's first
/// root. `cache.publish` maps it; the caller releases its reference.
pub fn loadFresh(cache: *NodeCache, addr: u64, pinned: bool, slot: ?ForestSlot) node_cache.LoadError!*CachedNode {
    const buf = try snapshot.SharedBuf.createExtent(cache.gpa, cache.cfg.layout.node_size);
    cache.loader.readExtent(cache.loader.ctx, addr, buf.data) catch |e| {
        buf.release();
        return e;
    };
    return cache.buildNode(addr, buf, pinned, slot);
}

/// Vyukov's bounded MPMC ring over `u64` addresses: commit threads push
/// (one CAS on the producer cursor, then the payload store, then the
/// slot-stamp RELEASE), the serialized SMO task pops (one CAS on the
/// consumer cursor, the payload load, the vacate stamp RELEASE). The stamp
/// publishes the payload: a consumer that observes `stamp == pos + 1`
/// (acquire) sees the producer's address store. Publishing the stamp
/// BEFORE the payload is the weakening `MaintenanceQueueModel` proves
/// fails (a stale address pops). Hot ops allocate nothing.
pub const MaintenanceQueue = struct {
    const Slot = struct {
        stamp: std.atomic.Value(usize),
        addr: std.atomic.Value(u64),
    };

    slots: []Slot,
    mask: usize,
    head: std.atomic.Value(usize) align(std.atomic.cache_line),
    tail: std.atomic.Value(usize) align(std.atomic.cache_line),

    pub fn init(gpa: Allocator, capacity: usize) Allocator.Error!MaintenanceQueue {
        assert(std.math.isPowerOfTwo(capacity));
        const slots = try gpa.alloc(Slot, capacity);
        for (slots, 0..) |*s, i| s.* = .{ .stamp = .init(i), .addr = .init(0) };
        return .{ .slots = slots, .mask = capacity - 1, .head = .init(0), .tail = .init(0) };
    }

    pub fn deinit(self: *MaintenanceQueue, gpa: Allocator) void {
        gpa.free(self.slots);
        self.* = undefined;
    }

    /// Enqueue; `false` when full (the caller counts and lets the dirty
    /// walk cover the node).
    pub fn push(self: *MaintenanceQueue, addr: u64) bool {
        var pos = self.head.load(.monotonic);
        while (true) {
            const slot = &self.slots[pos & self.mask];
            // acquire pairs with the consumer's vacate release store.
            const stamp = slot.stamp.load(.acquire);
            if (stamp == pos) {
                // acq_rel CAS: the winner owns the slot for this lap.
                if (self.head.cmpxchgWeak(pos, pos + 1, .acq_rel, .monotonic)) |cur| {
                    pos = cur;
                    continue;
                }
                slot.addr.store(addr, .monotonic);
                // release pairs with the consumer's acquire stamp load: the
                // address store above is visible once the stamp publishes.
                slot.stamp.store(pos + 1, .release);
                return true;
            } else if (stamp < pos) {
                return false; // a lap behind: full
            } else {
                pos = self.head.load(.monotonic);
            }
        }
    }

    pub fn pop(self: *MaintenanceQueue) ?u64 {
        var pos = self.tail.load(.monotonic);
        while (true) {
            const slot = &self.slots[pos & self.mask];
            // acquire pairs with the producer's publish release store.
            const stamp = slot.stamp.load(.acquire);
            if (stamp == pos + 1) {
                if (self.tail.cmpxchgWeak(pos, pos + 1, .acq_rel, .monotonic)) |cur| {
                    pos = cur;
                    continue;
                }
                const addr = slot.addr.load(.monotonic);
                // release: vacate for the producer a lap ahead.
                slot.stamp.store(pos + self.mask + 1, .release);
                return addr;
            } else if (stamp < pos + 1) {
                return null; // empty
            } else {
                pos = self.tail.load(.monotonic);
            }
        }
    }

    /// Whether anything is queued (a snapshot; the SMO task's wake test).
    pub fn isEmpty(self: *const MaintenanceQueue) bool {
        return self.tail.load(.acquire) >= self.head.load(.acquire);
    }
};

// ---------------------------------------------------------------------------
// The tree.
// ---------------------------------------------------------------------------

/// What a mounted tree IS (charter §5.5): the manager's control tree
/// (header id `TREE_CONTROL`) or ONE slot tree (header id `KIND_INTERIOR`,
/// every node RAM-stamped with its slot). The one-member forest mounts
/// `.control` and `.native`; a guest slot tree's root lives in tree 0.
pub const TreeIdentity = union(enum) {
    control,
    slot: ForestSlot,

    pub const native: TreeIdentity = .{ .slot = key.NATIVE_FOREST_SLOT };

    /// The node-header class byte every node of the tree carries.
    pub fn headerTreeId(self: TreeIdentity) u8 {
        return switch (self) {
            .control => key.TREE_CONTROL,
            .slot => key.KIND_INTERIOR,
        };
    }

    pub fn forestSlot(self: TreeIdentity) ?ForestSlot {
        return switch (self) {
            .control => null,
            .slot => |s| s,
        };
    }

    pub fn fromRootId(id: ledger.RootId) TreeIdentity {
        return switch (id) {
            .control => .control,
            .native_slot => .native,
        };
    }

    /// The ledger root this tree publishes under; null for a guest slot
    /// tree (its root is a tree-0 `slot_state` record, charter §5.5).
    pub fn rootId(self: TreeIdentity) ?ledger.RootId {
        return switch (self) {
            .control => .control,
            .slot => |s| if (s == key.NATIVE_FOREST_SLOT) .native_slot else null,
        };
    }

    pub fn eql(a: TreeIdentity, b: TreeIdentity) bool {
        return switch (a) {
            .control => b == .control,
            .slot => |sa| b == .slot and b.slot == sa,
        };
    }
};

/// One logical btree over a shared per-volume `NodeCache`.
pub const KvTree = struct {
    gpa: Allocator,
    identity: TreeIdentity,
    /// `identity.headerTreeId()`: what every node header of the tree says.
    tree_id: u8,
    cache: *NodeCache,
    /// The live root pointer — latch-free (`ArcSwap`); swapped only inside
    /// an SMO's lock window.
    root: ArcSwap(RootPtr),
    /// The volume-shared node-seq mint; also the record-seq source of the
    /// standalone (`.mint`) commit shape, assigned INSIDE the node-lock
    /// window so per-key seq order equals RAM apply order.
    seq: *NodeSeqHandle,
    /// Addresses whose open delta crossed the writeback threshold —
    /// drained by the SMO task's maintenance pass (duplicates benign).
    maintenance: MaintenanceQueue,
    /// The §4.6a merge sweep's resumable lap state — touched only by the
    /// serialized SMO task that owns the `SmoContext`.
    merge_cursor: leaf_merge.SweepCursor = .{},
    /// `identity.forestSlot()`: `Some(slot)` ⇔ a slot tree (every node
    /// carries header id 0 and the slot stamp); null on the control tree.
    forest_slot: ?ForestSlot,
    /// The journal position the LIVE root came into force at (0 = the root
    /// the tree was opened with — durably named already). A root swap's
    /// durable form is exclusively the next ledger record, so until one
    /// lands the checkpoint tail must not pass this position.
    root_floor: std.atomic.Value(u64),
    /// The tree's node (= extent) census — the affinity ceiling's input
    /// (charter §5.5 `slot_tree_extents`): 1 at create; a split or root
    /// grow +1, a merge or root collapse −1 (a compaction moves nothing);
    /// an opened tree is seeded from the lease's durable word (0 = unknown
    /// until seeded; the ceiling reads it as the floor).
    nodes: std.atomic.Value(u64) = .init(0),

    /// Format a fresh, empty tree: claim one extent (`.internal` — tree
    /// roots are format/checkpoint internals), write an empty leaf root
    /// spanning the whole key space, publish it pinned.
    pub fn create(gpa: Allocator, cache: *NodeCache, alloc: *const alloc_ext.ExtentAllocator, identity: TreeIdentity, seq: *NodeSeqHandle) (Error || error{NoSpace})!*KvTree {
        const extent = try alloc.claimInternal();
        errdefer alloc.releaseUnpublished(extent);
        const addr = cache.extentAddr(extent);
        // A root is the one node no interior Put names, so a crash before
        // its first durable home replays nothing that raises the mint: the
        // re-created tree would re-mint the SAME seq into the SAME recycled
        // extent and the dead incarnation's frames there would verify as
        // its own (node.zig's incarnation check compares for equality).
        // The residue ceiling floors the mint above every stamp the extent
        // still carries (§2.6; a foreign-space stamp is ignored by the
        // handle's own law).
        {
            const residue = try snapshot.SharedBuf.createExtent(gpa, cache.cfg.layout.node_size);
            defer residue.release();
            try cache.loader.readExtent(cache.loader.ctx, addr, residue.data);
            seq.raiseTo(node.residueSeqCeiling(residue.data));
        }
        const ns = try seq.mint();
        var w = try node.writeNode(gpa, cache.cfg.layout, .{
            .node_addr = addr,
            .node_seq = ns,
            .tree_id = identity.headerTreeId(),
            .level = 0,
            .min_key = "",
            .max_key = &KEY_SPACE_MAX,
        }, &.{}, 0, null);
        defer w.deinit(gpa);
        try cache.loader.writeImage(cache.loader.ctx, addr, w.image);
        const root_node = try loadFresh(cache, addr, true, identity.forestSlot());
        defer root_node.release();
        try cache.publish(root_node);
        const t = try init(gpa, cache, identity, .{ .addr = addr, .seq = ns }, seq, 0);
        t.nodes.store(1, .release);
        return t;
    }

    /// Re-attach to an existing tree from its root pointer (the mount path:
    /// superblock → ledger → roots; a lease's install). The root loads
    /// under the slot's frame screen; its incarnation and header id are
    /// verified against the pointer (a torn or stale ledger word never
    /// installs an unverified root), it is stamped with the slot and pinned.
    pub fn open(gpa: Allocator, cache: *NodeCache, identity: TreeIdentity, root: RootPtr, seq: *NodeSeqHandle) Error!*KvTree {
        const n = (if (cache.tryGet(root.addr)) |hit| hit else try cache.loadForSlot(root.addr, identity.forestSlot())) orelse return error.RetiredExtent;
        defer n.release();
        if (n.node_seq != root.seq) {
            log.err("root pointer stale: ledger says node_seq {d}, extent 0x{x} holds {d}", .{ root.seq, root.addr, n.node_seq });
            return error.Corrupt;
        }
        if (n.tree_id != identity.headerTreeId()) {
            log.err("root tree_id mismatch: expected {d}, extent 0x{x} holds {d}", .{ identity.headerTreeId(), root.addr, n.tree_id });
            return error.Corrupt;
        }
        if (identity.forestSlot()) |slot| n.stampForestSlot(slot);
        n.pin();
        return init(gpa, cache, identity, root, seq, 0);
    }

    /// `open` from a ledger record: the root the record names for `id`
    /// (`error.Corrupt` when the record names no such root).
    pub fn openFromLedger(gpa: Allocator, cache: *NodeCache, record: *const ledger.LedgerRecord, id: ledger.RootId, seq: *NodeSeqHandle) Error!*KvTree {
        if (record.root(id)) |r| return open(gpa, cache, .fromRootId(id), .{ .addr = r.node_addr, .seq = r.node_seq }, seq);
        log.err("ledger record {d} names no root {d}", .{ record.seq, @backingInt(id) });
        return error.Corrupt;
    }

    fn init(gpa: Allocator, cache: *NodeCache, identity: TreeIdentity, root: RootPtr, seq: *NodeSeqHandle, root_floor: u64) Allocator.Error!*KvTree {
        const root_arc = try Arc(RootPtr).create(gpa, root);
        errdefer root_arc.release();
        var queue = try MaintenanceQueue.init(gpa, maintenanceQueueCapacity(cache.cfg.budget_bytes / @max(cache.cfg.layout.node_size, 1)));
        errdefer queue.deinit(gpa);
        const self = try gpa.create(KvTree);
        self.* = .{
            .gpa = gpa,
            .identity = identity,
            .tree_id = identity.headerTreeId(),
            .cache = cache,
            .root = .init(root_arc),
            .seq = seq,
            .maintenance = queue,
            .forest_slot = identity.forestSlot(),
            .root_floor = .init(root_floor),
        };
        return self;
    }

    pub fn destroy(self: *KvTree) void {
        self.root.deinit();
        self.maintenance.deinit(self.gpa);
        self.gpa.destroy(self);
    }

    // ---- identity ----

    /// The current root pointer (what a checkpoint names in its ledger
    /// record).
    pub fn rootPtr(self: *KvTree) RootPtr {
        const a = self.root.load();
        defer a.release();
        return a.value;
    }

    /// Swap the root pointer — SMO-only, inside the lock window.
    pub fn storeRoot(self: *KvTree, root: RootPtr) Allocator.Error!void {
        self.root.store(try Arc(RootPtr).create(self.gpa, root));
    }

    /// The ledger face of the root; null for a guest slot tree.
    pub fn ledgerRoot(self: *KvTree) ?ledger.TreeRoot {
        const id = self.identity.rootId() orelse return null;
        const r = self.rootPtr();
        return .{ .id = id, .node_addr = r.addr, .node_seq = r.seq };
    }

    /// The node census (0 = not yet seeded on an opened tree).
    pub fn nodeCount(self: *const KvTree) u64 {
        return self.nodes.load(.acquire);
    }

    /// Seed an opened tree's census from the lease's durable word (a
    /// fetch_max: a live count never regresses to a stale word).
    pub fn seedNodeCount(self: *KvTree, n: u64) void {
        _ = self.nodes.fetchMax(n, .acq_rel);
    }

    pub fn noteNodes(self: *KvTree, delta: i64) void {
        if (delta >= 0) {
            _ = self.nodes.fetchAdd(@intCast(delta), .acq_rel);
        } else {
            // Saturating at 0: an unseeded opened tree may merge first.
            var cur = self.nodes.load(.acquire);
            while (true) {
                const next = cur -| @as(u64, @intCast(-delta));
                if (self.nodes.cmpxchgWeak(cur, next, .acq_rel, .acquire)) |seen| cur = seen else break;
            }
        }
    }

    pub fn rootFloor(self: *const KvTree) u64 {
        return self.root_floor.load(.acquire);
    }

    /// A root swap came into force at journal position `floor`: release
    /// pairs with `rootFloor`'s acquire (the ledger writer's read).
    pub fn setRootFloor(self: *KvTree, floor: u64) void {
        self.root_floor.store(floor, .release);
    }

    pub fn isRoot(self: *KvTree, n: *const CachedNode) bool {
        return self.rootPtr().addr == n.addr;
    }

    /// Whether `n` belongs to THIS tree: by header id — and, on a slot
    /// tree, by the owner stamp too.
    pub fn owns(self: *const KvTree, n: *const CachedNode) bool {
        if (n.tree_id != self.tree_id) return false;
        const slot = self.forest_slot orelse return true;
        return n.forestSlot() == slot;
    }

    /// Stamp `n` with this tree's slot (a no-op on the control tree).
    pub fn stamp(self: *const KvTree, n: *CachedNode) void {
        if (self.forest_slot) |s| n.stampForestSlot(s);
    }

    /// The JOURNAL key of one of this tree's interior pointer records: the
    /// separator itself on the control tree; `slot ‖ separator` on a slot
    /// tree (a separator alone cannot name the tree — every tree's top one
    /// is `KEY_SPACE_MAX`). `route` is the inverse.
    pub fn interiorJournalKey(self: *const KvTree, separator: []const u8) key.KeyBuf {
        if (self.forest_slot) |s| return key.interiorJournalKey(s, separator);
        return key.KeyBuf.fromSlice(separator);
    }

    /// This tree's elision tail — the ONE tail every fold / merge decision
    /// of this tree reads.
    pub fn durableTail(self: *const KvTree) u64 {
        return self.cache.durableTail();
    }

    /// Root level: 0 = a single-leaf tree, 1 = one interior level, …
    pub fn rootLevel(self: *KvTree) Error!u8 {
        const n = try self.getNode(self.rootPtr().addr);
        defer n.release();
        return n.level;
    }

    /// `cache.get` for a node of THIS tree: a miss loads under the tree's
    /// slot (the frame screen's input) — the walks a tree's owner runs
    /// over its own nodes (the tails census, the extent census) resolve
    /// through current state, so a retired address is an error.
    pub fn getNode(self: *KvTree, addr: u64) node_cache.ChildError!*CachedNode {
        return (try self.fetch(addr)) orelse error.RetiredExtent;
    }

    // ---- maintenance queue ----

    pub fn maintenancePending(self: *const KvTree) bool {
        return !self.maintenance.isEmpty();
    }

    /// Enqueue a node for the next maintenance pass (duplicates benign; a
    /// full queue is a counted deferral the dirty walk covers).
    pub fn enqueueMaintenance(self: *KvTree, addr: u64) void {
        if (!self.maintenance.push(addr)) stats.meta_kv_maintenance_queue_full.inc();
    }

    pub fn popMaintenance(self: *KvTree) ?u64 {
        return self.maintenance.pop();
    }

    // ---- latch-free traversal ----

    /// `tryGet` then the demand page; `null` = a retired extent (stale route).
    fn fetch(self: *KvTree, addr: u64) node_cache.LoadError!?*CachedNode {
        if (self.cache.tryGet(addr)) |n| return n;
        if (node_cache.try_only) return error.WouldBlock;
        return self.cache.loadForSlot(addr, self.forest_slot);
    }

    /// Descend from the current root to the node of `target_level` whose
    /// range holds `k` — latch-free (module doc). The caller releases the
    /// returned reference.
    pub fn descend(self: *KvTree, k: []const u8, target_level: u8) Error!*CachedNode {
        var nodes: u64 = 0;
        return self.descendCounted(k, target_level, &nodes);
    }

    /// `descend`, adding every node it fetched (restarts included) to `nodes`.
    fn descendCounted(self: *KvTree, k: []const u8, target_level: u8, nodes: *u64) Error!*CachedNode {
        const gpa = self.cache.gpa;
        // Restart-reason tallies `[root-retired, root-seq, routing-hole,
        // child-retired, child-seq]`, carried into the exhaustion error.
        var reasons: [5]u32 = @splat(0);
        // The seq the root's extent held at the last root-seq restart.
        var root_extent_seq: ?u64 = null;
        var attempt: u32 = 0;
        restart: while (attempt < node.RETRY_BUDGET) : (attempt += 1) {
            // Cooperative restart: every reason to be here is a racing
            // SMO's swap window and the cache misses short-circuit
            // synchronously, so a spinning walker would burn the whole
            // budget inside one held window; yielding turns the budget
            // into 256 scheduling opportunities, not 256 spins.
            if (attempt > 0) {
                // A try-only reader never waits out an SMO's window.
                if (node_cache.try_only) return error.WouldBlock;
                std.Thread.yield() catch std.atomic.spinLoopHint();
            }
            const root = self.rootPtr();
            var cur = (try self.fetch(root.addr)) orelse {
                reasons[0] += 1;
                continue :restart;
            };
            nodes.* += 1;
            // A device error mid-walk drops the reference the walk holds.
            errdefer cur.release();
            if (cur.node_seq != root.seq) {
                root_extent_seq = cur.node_seq;
                cur.release();
                reasons[1] += 1;
                continue :restart;
            }
            while (true) {
                if (cur.level == target_level) {
                    self.stamp(cur);
                    return cur;
                }
                if (cur.level < target_level) {
                    // A §4.6a root collapse lowered the height under this
                    // walk (the ONLY way a tree shrinks): restart at the
                    // new root.
                    cur.release();
                    continue :restart;
                }
                const snap = cur.snapshotLoad();
                defer snap.release();
                var live = (try snap.value.nextLive(gpa, k, null)) orelse {
                    cur.release();
                    reasons[2] += 1;
                    continue :restart; // routing hole: a stale snapshot raced an SMO
                };
                defer live.deinit();
                const iv = key.InteriorValue.decode(live.value) catch return error.Corrupt;
                const child = (try self.fetch(iv.childAddr())) orelse {
                    cur.release();
                    reasons[3] += 1;
                    continue :restart; // retired extent: stale route
                };
                nodes.* += 1;
                if (child.node_seq != iv.childSeq()) {
                    child.release();
                    cur.release();
                    reasons[4] += 1;
                    continue :restart; // §5.1 stale pointer
                }
                cur.release();
                cur = child;
            }
        }
        const root = self.rootPtr();
        // Every restart a root-seq mismatch: no SMO window is that long —
        // the root's extent holds another node (an overwritten root).
        const cause = if (reasons[1] == node.RETRY_BUDGET) "the root's extent holds another node — an overwritten root" else "routing loop — SMO protocol bug";
        log.err("tree {d} slot {?d} (root 0x{x}@{d}, its extent last seen at seq {?d}): traversal retry budget exhausted descending to level {d} ({s}) restarts [root-retired, root-seq, routing-hole, child-retired, child-seq] = {any}", .{ self.tree_id, self.forest_slot, root.addr, root.seq, root_extent_seq, target_level, cause, reasons });
        return error.RetryBudgetExhausted;
    }

    /// Resolve `k` to its leaf — the commit path's latch-free step.
    pub fn resolveLeaf(self: *KvTree, k: []const u8) Error!*CachedNode {
        return self.descend(k, 0);
    }

    /// Resolve `n`'s parent: descend to `n.level + 1` routing by `n.min_key`
    /// (the partition rule makes the child's own separator the first one
    /// ≥ its min). Stable during an SMO — SMOs are serialized.
    pub fn resolveParent(self: *KvTree, n: *const CachedNode) Error!*CachedNode {
        return self.descend(n.min_key, n.level + 1);
    }

    // ---- point reads ----

    /// Latch-free point lookup: pinned-interior traverse + leaf snapshot
    /// fold. `null` for tombstoned and never-written keys alike; the
    /// value is an OWNED copy (`gpa.free`).
    pub fn lookup(self: *KvTree, gpa: Allocator, k: []const u8) Error!?[]u8 {
        var cost: WalkCost = .{};
        return self.lookupCounted(gpa, k, &cost);
    }

    /// `lookup`, adding its descent to `cost`.
    pub fn lookupCounted(self: *KvTree, gpa: Allocator, k: []const u8, cost: *WalkCost) Error!?[]u8 {
        const leaf = try self.descendCounted(k, 0, &cost.nodes);
        defer leaf.release();
        const snap = leaf.snapshotLoad();
        defer snap.release();
        var l = try snap.value.lookup(gpa, k);
        defer l.deinit();
        const v = l.liveValue() orelse return null;
        cost.entries += 1;
        return try gpa.dupe(u8, v);
    }

    // ---- the commit path: resolve → lock → revalidate → apply ----

    /// THE lock-window primitive (module doc). Never performs I/O and
    /// never touches an interior lock on the live path.
    pub fn applyAtSeqClass(self: *KvTree, leaf: *CachedNode, k: []const u8, kind: RecordKind, value: []const u8, plan: SeqPlan, structural: bool) Error!ApplyOutcome {
        if (kind == .delete and value.len != 0) return error.InvalidRecord;
        const io = self.cache.io;
        const d = leaf.lockWrite(io);
        if (leaf.state.isSuperseded() or key.lessThan(k, leaf.min_key) or key.lessThan(leaf.max_key, k)) {
            leaf.unlockWrite(io);
            stats.meta_kv_commit_smo_retries.inc();
            return .stale;
        }
        const st: Stamp = switch (plan) {
            .replay => |s| blk: {
                // Per-key LWW replay gate: an old-ledger mount reads node
                // bsets that already materialized part of the window; a
                // record whose seq is ≤ the node's newest for the key is
                // already folded in — re-applying it would sort an overlay
                // record BELOW the base and break the newest-first order.
                const snap = leaf.snapshotLoad();
                defer snap.release();
                if (snap.value.newestSeqOf(k)) |newest| {
                    if (newest >= s.seq) {
                        leaf.unlockWrite(io);
                        return .applied;
                    }
                }
                break :blk s;
            },
            .mint => blk: {
                const s = self.seq.mint() catch {
                    leaf.unlockWrite(io);
                    return error.SpaceExhausted;
                };
                break :blk .{ .seq = s, .entry_start = s };
            },
            .reserve => |r| r.stamp(r.ctx) catch {
                leaf.unlockWrite(io);
                return error.OutOfMemory;
            },
        };
        var recs = [_]OwnedRec{OwnedRec.fromRecordRef(self.gpa, .{ .key = k, .seq = st.seq, .kind = kind, .value = value }) catch {
            leaf.unlockWrite(io);
            return error.OutOfMemory;
        }};
        const applied = if (structural) leaf.applyLockedStructural(d, &recs, st.entry_start) else leaf.applyLocked(d, &recs, st.entry_start);
        applied catch |e| {
            leaf.unlockWrite(io);
            return switch (e) {
                // Superseded under our lock: a mapping replaced outside the
                // SMO path — the revalidation class, retried.
                error.Superseded => blk: {
                    stats.meta_kv_commit_smo_retries.inc();
                    break :blk .stale;
                },
                error.ReaderMayNotMutate, error.LeafLeaseRefused => error.PartitionRefused,
                error.OutOfMemory => error.OutOfMemory,
            };
        };
        const over_threshold = d.overlayBytes() >= self.cache.cfg.writeback_delta_bytes;
        leaf.unlockWrite(io);
        if (over_threshold) self.enqueueMaintenance(leaf.addr);
        return .applied;
    }

    /// The single-record live shape: minted seq, content class.
    pub fn applyAt(self: *KvTree, leaf: *CachedNode, k: []const u8, kind: RecordKind, value: []const u8) Error!ApplyOutcome {
        return self.applyAtSeqClass(leaf, k, kind, value, .mint, false);
    }

    fn checkKey(k: []const u8) Error!void {
        if (k.len == 0 or !key.lessThan(k, &KEY_SPACE_MAX)) return error.InvalidKey;
    }

    /// Interior separators live in a wider domain: the rightmost sibling's
    /// is exactly `KEY_SPACE_MAX`, and every rightmost-leaf SMO journals a
    /// pointer record under it — replay must accept it.
    fn checkInteriorKey(k: []const u8) Error!void {
        if (k.len == 0 or key.lessThan(&KEY_SPACE_MAX, k)) return error.InvalidKey;
    }

    /// The writer loop: resolve → `applyAtSeqClass` → retry on `.stale`,
    /// bounded by `node.RETRY_BUDGET`.
    pub fn mutate(self: *KvTree, k: []const u8, kind: RecordKind, value: []const u8, plan: SeqPlan) Error!void {
        try checkKey(k);
        var attempt: u32 = 0;
        while (attempt < node.RETRY_BUDGET) : (attempt += 1) {
            const leaf = try self.resolveLeaf(k);
            defer leaf.release();
            switch (try self.applyAtSeqClass(leaf, k, kind, value, plan, false)) {
                .applied => return,
                .stale => continue,
            }
        }
        log.err("tree {d}: writer retry budget exhausted (revalidation never passed — SMO protocol bug)", .{self.tree_id});
        return error.RetryBudgetExhausted;
    }

    /// Insert / overwrite `k` (a `Put`; per-key LWW by seq). Values above
    /// the per-volume cap are refused typed.
    pub fn insert(self: *KvTree, k: []const u8, value: []const u8) Error!void {
        if (value.len > self.cache.cfg.layout.recordValueCapOf()) return error.ValueTooLarge;
        return self.mutate(k, .put, value, .mint);
    }

    /// Delete `k` (a tombstone; unconditional — elided at compaction once
    /// durable).
    pub fn delete(self: *KvTree, k: []const u8) Error!void {
        return self.mutate(k, .delete, &.{}, .mint);
    }

    // ---- mount replay (two-phase) ----

    /// Phase-2 content replay: resolve → lock → revalidate → apply with
    /// the record's JOURNAL seq (per-key LWW must reproduce the pre-crash
    /// history, so replay never mints). The seq handle is floored above
    /// the replayed seq so post-replay mints stay newer.
    pub fn applyReplayed(self: *KvTree, k: []const u8, seq: u64, kind: RecordKind, value: []const u8, entry_start: u64) Error!void {
        try checkKey(k);
        self.seq.raiseTo(seq);
        var attempt: u32 = 0;
        while (attempt < node.RETRY_BUDGET) : (attempt += 1) {
            const leaf = try self.resolveLeaf(k);
            defer leaf.release();
            switch (try self.applyAtSeqClass(leaf, k, kind, value, .{ .replay = .{ .seq = seq, .entry_start = entry_start } }, false)) {
                .applied => return,
                .stale => continue,
            }
        }
        log.err("tree {d}: replay retry budget exhausted (no SMO can be running at mount)", .{self.tree_id});
        return error.RetryBudgetExhausted;
    }

    /// Phase-1 interior replay: route to the level-`level` node covering
    /// `k` and apply with the record's journal seq. `false` = DROPPED, never
    /// loud: the mounted (older) ledger may predate a root growth, so the
    /// mounted tree is shorter than the crashed one and the pointer's
    /// target parent does not exist — sound, because content replays by
    /// key through the old routing; the successor extents stay allocated
    /// but unreferenced (a bounded crash-window leak for fsck).
    pub fn applyReplayedInterior(self: *KvTree, k: []const u8, level: u8, seq: u64, kind: RecordKind, value: []const u8, entry_start: u64) Error!bool {
        try checkInteriorKey(k);
        self.seq.raiseTo(seq);
        if (try self.rootLevel() < level) return false;
        var attempt: u32 = 0;
        while (attempt < node.RETRY_BUDGET) : (attempt += 1) {
            const target = try self.descend(k, level);
            defer target.release();
            switch (try self.applyAtSeqClass(target, k, kind, value, .{ .replay = .{ .seq = seq, .entry_start = entry_start } }, true)) {
                .applied => return true,
                .stale => continue,
            }
        }
        log.err("tree {d}: interior replay retry budget exhausted (no SMO can be running at mount)", .{self.tree_id});
        return error.RetryBudgetExhausted;
    }

    // ---- range scans ----

    /// Append up to `max` live `(key, value)` with `start ≤ key ≤ end`
    /// (memcmp order) to `out`, fold-walked leaf by leaf over held
    /// snapshots — latch-free; a concurrent SMO at worst retries the NEXT
    /// leaf's resolution. Resume with `start = key_successor(last key)`.
    pub fn range(self: *KvTree, gpa: Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(KeyValue)) Error!void {
        var cost: WalkCost = .{};
        return self.rangeCounted(gpa, start, end, max, out, &cost);
    }

    /// `range`, adding its descents and the entries it copied to `cost`.
    pub fn rangeCounted(self: *KvTree, gpa: Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(KeyValue), cost: *WalkCost) Error!void {
        if (max == 0 or key.lessThan(end, start)) return;
        var cursor = key.KeyBuf.fromSlice(start);
        var got: usize = 0;
        defer cost.entries += got;
        while (true) {
            const leaf = try self.descendCounted(cursor.slice(), 0, &cost.nodes);
            defer leaf.release();
            const snap = leaf.snapshotLoad();
            defer snap.release();
            var pos = cursor;
            while (got < max) {
                // Bounded by `end` BEFORE any fold: the walk never folds the
                // tombstone desert beyond the requested window.
                var live = (try snap.value.nextLive(gpa, pos.slice(), end)) orelse break;
                defer live.deinit();
                const k = try gpa.dupe(u8, live.key);
                errdefer gpa.free(k);
                const v = try gpa.dupe(u8, live.value);
                errdefer gpa.free(v);
                try out.append(gpa, .{ .key = k, .value = v });
                got += 1;
                pos = key.keySuccessor(live.key);
            }
            if (got >= max or !key.lessThan(leaf.max_key, end)) return;
            // The next leaf: the partition rule makes successor(max_key) the
            // right sibling's exact min_key.
            cursor = key.keySuccessor(leaf.max_key);
        }
    }
};

// ---------------------------------------------------------------------------
// The volume's live tree set.
// ---------------------------------------------------------------------------

/// The mounted trees of one volume as an immutable, atomically published
/// list (charter §5.5 "slot trees are minted lazily"; §5.4 a slot migrates
/// in or out of a live volume): readers — the conveyor pass's routing,
/// every lookup, the checkpoint seam — `load` one pointer and walk the
/// slice; a writer (the mount, a first mint, a flip's add or drop) builds a
/// new list and stores it. Nothing a reader may hold is ever freed before
/// the table goes: superseded lists and dropped trees are RETIRED and
/// released with the table (a few per mount — one per tree change), so no
/// reader needs a grace protocol. Writers are serialized by the caller
/// (the volume's tree-set lock).
pub const TreeTable = struct {
    gpa: Allocator,
    current: std.atomic.Value(*const List),
    retired_lists: std.ArrayList(*const List) = .empty,
    retired_trees: std.ArrayList(*KvTree) = .empty,

    pub const List = struct { trees: []const *KvTree };

    pub fn init(gpa: Allocator, initial: []const *KvTree) Allocator.Error!TreeTable {
        return .{ .gpa = gpa, .current = .init(try makeList(gpa, initial)) };
    }

    fn makeList(gpa: Allocator, trees: []const *KvTree) Allocator.Error!*const List {
        const l = try gpa.create(List);
        errdefer gpa.destroy(l);
        l.* = .{ .trees = try gpa.dupe(*KvTree, trees) };
        return l;
    }

    fn freeList(gpa: Allocator, l: *const List) void {
        gpa.free(l.trees);
        gpa.destroy(l);
    }

    /// Frees the lists and destroys every DROPPED tree; the current trees
    /// are the caller's to destroy (`load` them first).
    pub fn deinit(self: *TreeTable) void {
        freeList(self.gpa, self.current.load(.monotonic));
        for (self.retired_lists.items) |l| freeList(self.gpa, l);
        self.retired_lists.deinit(self.gpa);
        for (self.retired_trees.items) |t| t.destroy();
        self.retired_trees.deinit(self.gpa);
        self.* = undefined;
    }

    /// The live trees (acquire pairs with the writer's release store).
    pub fn load(self: *const TreeTable) []const *KvTree {
        return self.current.load(.acquire).trees;
    }

    pub fn find(self: *const TreeTable, identity: TreeIdentity) ?*KvTree {
        for (self.load()) |t| if (t.identity.eql(identity)) return t;
        return null;
    }

    fn publish(self: *TreeTable, next: []const *KvTree) Allocator.Error!void {
        const l = try makeList(self.gpa, next);
        errdefer freeList(self.gpa, l);
        try self.retired_lists.ensureUnusedCapacity(self.gpa, 1);
        const old = self.current.swap(l, .acq_rel);
        self.retired_lists.appendAssumeCapacity(old);
    }

    /// Add a tree (its identity must be new).
    pub fn add(self: *TreeTable, t: *KvTree) Allocator.Error!void {
        assert(self.find(t.identity) == null);
        const cur = self.load();
        const next = try self.gpa.alloc(*KvTree, cur.len + 1);
        defer self.gpa.free(next);
        @memcpy(next[0..cur.len], cur);
        next[cur.len] = t;
        try self.publish(next);
    }

    /// Drop a tree: it leaves the list now and is destroyed with the table.
    pub fn remove(self: *TreeTable, t: *KvTree) Allocator.Error!void {
        const cur = self.load();
        try self.retired_trees.ensureUnusedCapacity(self.gpa, 1);
        const next = try self.gpa.alloc(*KvTree, cur.len);
        defer self.gpa.free(next);
        var n: usize = 0;
        for (cur) |c| if (c != t) {
            next[n] = c;
            n += 1;
        };
        assert(n + 1 == cur.len);
        try self.publish(next[0..n]);
        self.retired_trees.appendAssumeCapacity(t);
    }
};

// ---------------------------------------------------------------------------
// The two-phase replay driver over a volume's mounted trees.
// ---------------------------------------------------------------------------

/// What one replay pass did.
pub const ReplayOutcome = struct {
    /// Phase-1 pointer records routed into an interior node.
    interior_applied: u64 = 0,
    /// Phase-1 pointer records the mounted (shorter) structure could not
    /// route — dropped sound and silent.
    interior_dropped: u64 = 0,
    /// Phase-2 content records applied (or LWW-skipped).
    content: u64 = 0,
    /// Records of trees not in `trees` (allocator deltas, unmounted kinds).
    skipped: u64 = 0,
};

/// A routed record: the tree it belongs to and the key the tree applies.
pub const Route = struct { tree: *KvTree, key: []const u8 };

/// THE record routing law (charter §5.5 "a record's slot is `key_ino >> 40`
/// (refs by their owner ino)"), the inverse of every tag a commit or SMO
/// writes: a content record of a slot-tree kind belongs to the slot tree
/// its key's slot names; a control record (kind 8) to the control tree; an
/// interior record (level > 0) to the tree of its header id — a slot
/// tree's is named by the `slot ‖ separator` journal key, and the tree
/// applies the separator. Null = no mounted tree takes it (an allocator
/// delta, an unmounted slot, a key that decodes as no kind).
pub fn route(trees: []const *KvTree, tree_id: u8, level: u8, k: []const u8) ?Route {
    if (level > 0) {
        if (tree_id == key.KIND_INTERIOR) {
            const split = key.splitInteriorJournalKey(k) catch return null;
            for (trees) |t| if (t.forest_slot == split.slot) return .{ .tree = t, .key = split.separator };
            return null;
        }
        for (trees) |t| if (t.forest_slot == null and t.tree_id == tree_id) return .{ .tree = t, .key = k };
        return null;
    }
    if (key.isSlotTreeKind(tree_id)) {
        // The key must decode as the tagged kind (admission and the mount
        // fold refuse a disagreeing pair; here it routes nowhere).
        const slot = key.checkSlotTreeKey(tree_id, k) catch return null;
        for (trees) |t| if (t.forest_slot == slot) return .{ .tree = t, .key = k };
        return null;
    }
    for (trees) |t| if (t.forest_slot == null and t.tree_id == tree_id) return .{ .tree = t, .key = k };
    return null;
}

/// `route` for a content record (level 0).
pub fn routeContent(trees: []const *KvTree, tree_id: u8, k: []const u8) ?*KvTree {
    const r = route(trees, tree_id, 0, k) orelse return null;
    return r.tree;
}

/// A journal frame's kind as the record kind — the two enums share one
/// encoding (`put = 1, delta = 2, delete = 3`), asserted at comptime: a
/// replay that folded `delta` into `delete` turned every inode update of
/// the window into a tombstone (the recovery's `buildBset` abort).
pub fn kindOf(k: journal.RecordKind) RecordKind {
    return @fromBackingInt(@intCast(@backingInt(k)));
}

comptime {
    const info = @typeInfo(journal.RecordKind).@"enum";
    for (info.field_names, info.field_values) |name, value| {
        std.debug.assert(std.mem.eql(u8, @tagName(kindOf(@fromBackingInt(@intCast(value)))), name));
    }
}

/// Mount replay, two-phase (charter §5.3): phase 1 applies every interior
/// record ordered `(level DESC, seq)` — upper flips route lower ones, and a
/// leaf-SMO flip is itself "content" to the interior it applies to, so a
/// pure-seq order would strand it exactly as single-pass replay stranded
/// leaf content; the node-seq mint is raised above every child incarnation
/// a `Put` names. Phase 2 applies content records at their original seqs
/// by key through the now-FINAL routing, so every record folds into the
/// node covering its key in the final structure (whose image the SMO
/// barriered before its flip could exist). Both phases walk the SAME
/// checksummed entries, so neither can see half a transaction. Replay is
/// read-only into the cache and `(level DESC, seq)` is a deterministic
/// total order — replaying twice folds to one digest.
pub fn replayEntries(gpa: Allocator, trees: []const *KvTree, entries: []const journal.ReplayedEntry) Error!ReplayOutcome {
    var out: ReplayOutcome = .{};
    const interior = try journal.interiorRecordsSorted(gpa, entries);
    defer gpa.free(interior);
    for (interior) |r| {
        const routed = route(trees, r.tree_id, r.level, r.record.key) orelse {
            out.skipped += 1;
            continue;
        };
        const t = routed.tree;
        const kind = kindOf(r.record.kind);
        if (kind == .put) {
            if (key.InteriorValue.decode(r.record.value)) |iv| t.seq.raiseTo(iv.childSeq()) else |_| {}
        }
        if (try t.applyReplayedInterior(routed.key, r.level, r.record.seq, kind, r.record.value, r.entry_seq)) {
            out.interior_applied += 1;
        } else {
            out.interior_dropped += 1;
        }
    }
    for (entries) |e| {
        var it = e.records();
        while (it.next()) |r| {
            const tag = journal.untag(r.tag);
            if (tag.level > 0) continue; // phase 1 applied it
            const t = routeContent(trees, tag.tree_id, r.key) orelse {
                out.skipped += 1;
                continue;
            };
            try t.applyReplayed(r.key, r.seq, kindOf(r.kind), r.value, e.seq);
            out.content += 1;
        }
    }
    return out;
}

/// The ledger writer's roots: one `TreeRoot` per ledger-rooted tree, in
/// `trees` order.
pub fn roots(trees: []const *KvTree, out: []ledger.TreeRoot) []ledger.TreeRoot {
    assert(out.len >= trees.len);
    var n: usize = 0;
    for (trees) |t| {
        if (t.ledgerRoot()) |r| {
            out[n] = r;
            n += 1;
        }
    }
    return out[0..n];
}

// ---------------------------------------------------------------------------
// Tests — the row-6 contracts over the standalone (`.mint`) shape; the SMO
// contracts live in `smo.zig` and `leaf_merge.zig` over `smo.TestVolume`.
// ---------------------------------------------------------------------------

const testing = std.testing;
const smo = @import("smo.zig");
const explorer = @import("../model/explorer.zig");

fn ikey(i: u64) [key.INODE_KEY_LEN]u8 {
    return key.inodeKey(i);
}

/// A `len`-byte value carrying `i` in its first 8 bytes.
pub fn testValue(i: u64, len: usize, buf: []u8) []u8 {
    assert(len >= 8 and buf.len >= len);
    @memset(buf[0..len], @truncate(i % 251));
    std.mem.writeInt(u64, buf[0..8], i, .little);
    return buf[0..len];
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

test "maintenance queue capacity tie: clamp(next_pow2(budget_nodes), 64, 2^20)" {
    try testing.expectEqual(@as(usize, 64), maintenanceQueueCapacity(0));
    try testing.expectEqual(@as(usize, 64), maintenanceQueueCapacity(37));
    try testing.expectEqual(@as(usize, 2048), maintenanceQueueCapacity(2048));
    try testing.expectEqual(@as(usize, 4096), maintenanceQueueCapacity(2049));
    try testing.expectEqual(@as(usize, 1 << 20), maintenanceQueueCapacity(1 << 30));
}

test "maintenance queue: FIFO, bounded, full is a refusal, pop on empty is null, wraps across laps" {
    var q = try MaintenanceQueue.init(testing.allocator, 4);
    defer q.deinit(testing.allocator);
    try testing.expect(q.isEmpty());
    try testing.expectEqual(@as(?u64, null), q.pop());
    for (1..5) |i| try testing.expect(q.push(i * 100));
    try testing.expect(!q.push(999));
    try testing.expect(!q.isEmpty());
    try testing.expectEqual(@as(?u64, 100), q.pop());
    try testing.expectEqual(@as(?u64, 200), q.pop());
    try testing.expect(q.push(500));
    try testing.expect(q.push(600));
    try testing.expectEqual(@as(?u64, 300), q.pop());
    try testing.expectEqual(@as(?u64, 400), q.pop());
    try testing.expectEqual(@as(?u64, 500), q.pop());
    try testing.expectEqual(@as(?u64, 600), q.pop());
    try testing.expectEqual(@as(?u64, null), q.pop());
    try testing.expect(q.isEmpty());
}

test "maintenance queue: 8 producers × 256 pushes against one consumer lose and duplicate nothing" {
    var q = try MaintenanceQueue.init(testing.allocator, 1 << 12);
    defer q.deinit(testing.allocator);
    const Producer = struct {
        fn run(queue: *MaintenanceQueue, base: u64) void {
            for (0..256) |i| while (!queue.push(base + i)) std.atomic.spinLoopHint();
        }
    };
    var ts: [8]std.Thread = undefined;
    for (&ts, 0..) |*t, w| t.* = try std.Thread.spawn(.{}, Producer.run, .{ &q, @as(u64, w) * 1000 });
    var seen = std.AutoHashMap(u64, void).init(testing.allocator);
    defer seen.deinit();
    var got: usize = 0;
    while (got < 8 * 256) {
        const a = q.pop() orelse {
            std.atomic.spinLoopHint();
            continue;
        };
        try testing.expect(!seen.contains(a));
        try seen.put(a, {});
        got += 1;
    }
    for (ts) |t| t.join();
    try testing.expectEqual(@as(?u64, null), q.pop());
}

/// The queue's dependency-free MODEL (charter §9): two producers, one
/// consumer, capacity 2, every atomic action a step. The consumer must
/// never pop an address no producer pushed (a stale slot payload), never
/// pop one twice, and at the end the two pushed addresses were popped
/// exactly once (or still sit in the queue). `publish_before_payload` is
/// the WEAKENING: the producer stores the stamp before the address, and
/// the consumer pops the slot's stale `0`.
fn MaintenanceQueueModel(comptime publish_before_payload: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const cap: usize = 2;

        stamp: [cap]usize = .{ 0, 1 },
        addr: [cap]u64 = .{ 0, 0 },
        head: usize = 0,
        tail: usize = 0,
        pc: [3]u8 = @splat(0),
        pos: [3]usize = @splat(0),
        seen: [3]usize = @splat(0),
        popped: [2]u8 = @splat(0),
        pops: u8 = 0,
        bad_pop: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 255;
        }

        pub fn key(self: *const Self) u64 {
            var h = std.hash.Wyhash.init(0);
            std.hash.autoHash(&h, self.*);
            return h.final();
        }

        fn producerAddr(t: usize) u64 {
            return @as(u64, t) + 1;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t < 2) {
                // Producer: 0 load head; 1 load stamp; 2 CAS head; 3/4 the
                // two stores (order by the weakening); 255 done.
                switch (self.pc[t]) {
                    0 => {
                        self.pos[t] = self.head;
                        self.pc[t] = 1;
                    },
                    1 => {
                        self.seen[t] = self.stamp[self.pos[t] % cap];
                        self.pc[t] = if (self.seen[t] == self.pos[t]) 2 else 0;
                    },
                    2 => {
                        if (self.head == self.pos[t]) {
                            self.head += 1;
                            self.pc[t] = 3;
                        } else self.pc[t] = 0;
                    },
                    3 => {
                        if (publish_before_payload) self.stamp[self.pos[t] % cap] = self.pos[t] + 1 else self.addr[self.pos[t] % cap] = producerAddr(t);
                        self.pc[t] = 4;
                    },
                    4 => {
                        if (publish_before_payload) self.addr[self.pos[t] % cap] = producerAddr(t) else self.stamp[self.pos[t] % cap] = self.pos[t] + 1;
                        self.pc[t] = 255;
                    },
                    else => unreachable,
                }
                return;
            }
            // Consumer: pops until it has taken two addresses or observed
            // both producers done with an empty queue.
            switch (self.pc[t]) {
                0 => {
                    self.pos[t] = self.tail;
                    self.pc[t] = 1;
                },
                1 => {
                    self.seen[t] = self.stamp[self.pos[t] % cap];
                    if (self.seen[t] == self.pos[t] + 1) {
                        self.pc[t] = 2;
                    } else if (self.pc[0] == 255 and self.pc[1] == 255 and self.tail == self.head) {
                        self.pc[t] = 255;
                    } else self.pc[t] = 0;
                },
                2 => {
                    if (self.tail == self.pos[t]) {
                        self.tail += 1;
                        self.pc[t] = 3;
                    } else self.pc[t] = 0;
                },
                3 => {
                    const a = self.addr[self.pos[t] % cap];
                    if (a == 0 or a > 2 or self.popped[@intCast(a - 1)] != 0) self.bad_pop = true else self.popped[@intCast(a - 1)] = 1;
                    self.pops += 1;
                    self.pc[t] = 4;
                },
                4 => {
                    self.stamp[self.pos[t] % cap] = self.pos[t] + cap;
                    self.pc[t] = if (self.pops == 2) 255 else 0;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.bad_pop) return error.StalePayloadPopped;
        }

        pub fn final(self: *const Self) !void {
            if (self.head != 2 or self.tail != 2) return error.NotDrained;
            if (self.popped[0] != 1 or self.popped[1] != 1) return error.LostPush;
        }
    };
}

test "model: the maintenance queue publishes each address exactly once; the stamp-before-payload weakening pops a stale slot" {
    try explorer.expectHolds(MaintenanceQueueModel(false), .{});
    try explorer.expectFails(MaintenanceQueueModel(true), .{}, error.InvariantViolated);
}

test "interior value roundtrip and bounds (the traversal's pointer format)" {
    const v = key.encodeInteriorValue(0x1234_5678_9ABC_DEF0, 42);
    try testing.expectEqual(@as(usize, 16), v.len);
    const iv = try key.InteriorValue.decode(&v);
    try testing.expectEqual(@as(u64, 0x1234_5678_9ABC_DEF0), iv.childAddr());
    try testing.expectEqual(@as(u64, 42), iv.childSeq());
    try testing.expectError(error.Corrupt, key.InteriorValue.decode(v[0..15]));
    const oversized: [17]u8 = @splat(0);
    try testing.expectError(error.Corrupt, key.InteriorValue.decode(&oversized));
}

test "point_ops_insert_overwrite_delete_reinsert: LWW by seq, tombstones fold to absent, keys are non-empty and below the sentinel" {
    var vol = try smo.TestVolume.init(testing.allocator, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    try expectLookup(t, &ikey(7), null);
    try t.insert(&ikey(7), testValue(7, 32, &buf));
    try expectLookup(t, &ikey(7), testValue(7, 32, &buf));
    try t.insert(&ikey(7), testValue(700, 48, &buf));
    try expectLookup(t, &ikey(7), testValue(700, 48, &buf));
    try t.delete(&ikey(7));
    try expectLookup(t, &ikey(7), null);
    try t.insert(&ikey(7), testValue(7000, 16, &buf));
    try expectLookup(t, &ikey(7), testValue(7000, 16, &buf));
    try testing.expectError(error.InvalidKey, t.insert("", "x"));
    try testing.expectError(error.InvalidKey, t.insert(&KEY_SPACE_MAX, "x"));
    // The value cap: 64 KiB nodes ⇒ 16 KiB + the 256 B xattr envelope.
    const cap = vol.cache.cfg.layout.recordValueCapOf();
    try testing.expectEqual(@as(usize, 16 * 1024 + 256), cap);
    const big = try testing.allocator.alloc(u8, cap + 1);
    defer testing.allocator.free(big);
    @memset(big, 0);
    try t.insert(&ikey(1), big[0..cap]);
    try testing.expectError(error.ValueTooLarge, t.insert(&ikey(2), big));
}

test "range_scan_folds_orders_and_resumes: tombstones and overwrites fold through the scan; chunks stitch; both bounds hold" {
    const gpa = testing.allocator;
    var vol = try smo.TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    for (0..200) |i| try t.insert(&ikey(i), testValue(i, 24, &buf));
    var i: u64 = 0;
    while (i < 200) : (i += 3) try t.delete(&ikey(i));
    try t.insert(&ikey(100), testValue(9999, 40, &buf));

    var all: std.ArrayList(KeyValue) = .empty;
    defer deinitKeyValues(gpa, &all);
    try t.range(gpa, &ikey(0), &ikey(199), std.math.maxInt(usize), &all);
    var want: usize = 0;
    for (0..200) |n| {
        if (n % 3 == 0) continue;
        try testing.expectEqualSlices(u8, &ikey(n), all.items[want].key);
        if (n == 100) try testing.expectEqualSlices(u8, testValue(9999, 40, &buf), all.items[want].value);
        want += 1;
    }
    try testing.expectEqual(want, all.items.len);

    // Bounded + resumable: chunks of 7 stitch to the same stream.
    var resumed: std.ArrayList(KeyValue) = .empty;
    defer deinitKeyValues(gpa, &resumed);
    var start = key.KeyBuf.fromSlice(&ikey(0));
    while (true) {
        const before = resumed.items.len;
        try t.range(gpa, start.slice(), &ikey(199), 7, &resumed);
        if (resumed.items.len == before) break;
        start = key.keySuccessor(resumed.items[resumed.items.len - 1].key);
    }
    try testing.expectEqual(all.items.len, resumed.items.len);
    for (all.items, resumed.items) |a, r| try testing.expectEqualSlices(u8, a.key, r.key);

    // Sub-ranges respect both bounds.
    var mid: std.ArrayList(KeyValue) = .empty;
    defer deinitKeyValues(gpa, &mid);
    try t.range(gpa, &ikey(50), &ikey(60), std.math.maxInt(usize), &mid);
    var want_mid: usize = 0;
    for (50..61) |n| want_mid += @intFromBool(n % 3 != 0);
    try testing.expectEqual(want_mid, mid.items.len);
    try testing.expectEqualSlices(u8, &ikey(50), mid.items[0].key);
    // An inverted window and a zero cap return nothing.
    try t.range(gpa, &ikey(60), &ikey(50), 10, &mid);
    try t.range(gpa, &ikey(0), &ikey(199), 0, &mid);
    try testing.expectEqual(want_mid, mid.items.len);
}

test "reads_proceed_while_writer_holds_the_node_lock: lookups and scans take no node lock" {
    const gpa = testing.allocator;
    var vol = try smo.TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    for (0..300) |i| try t.insert(&ikey(i), testValue(i, 32, &buf));
    const leaf = try t.resolveLeaf(&ikey(150));
    defer leaf.release();
    _ = leaf.lockWrite(testing.io); // a writer mid-apply
    defer leaf.unlockWrite(testing.io);
    try expectLookup(t, &ikey(150), testValue(150, 32, &buf));
    var all: std.ArrayList(KeyValue) = .empty;
    defer deinitKeyValues(gpa, &all);
    try t.range(gpa, &ikey(0), &ikey(299), std.math.maxInt(usize), &all);
    try testing.expectEqual(@as(usize, 300), all.items.len);
}

test "open verifies the root pointer: a stale ledger node_seq or a foreign tree id is refused loud; roots() is the ledger face" {
    const gpa = testing.allocator;
    var vol = try smo.TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    try t.insert(&ikey(1), testValue(1, 24, &buf));
    try vol.flushDirty(t);
    const root = t.rootPtr();
    var out: [1]ledger.TreeRoot = undefined;
    const rs = roots(&.{t}, &out);
    try testing.expectEqual(ledger.TreeRoot{ .id = .native_slot, .node_addr = root.addr, .node_seq = root.seq }, rs[0]);
    var fresh = try vol.freshCache();
    defer fresh.deinit();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Corrupt, KvTree.open(gpa, fresh, .native, .{ .addr = root.addr, .seq = root.seq + 1 }, vol.seq));
    try log.testing.expectAllFired();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Corrupt, KvTree.open(gpa, fresh, .control, root, vol.seq));
    try log.testing.expectAllFired();
    const reopened = try KvTree.open(gpa, fresh, .native, root, vol.seq);
    defer reopened.destroy();
    try expectLookup(reopened, &ikey(1), testValue(1, 24, &buf));
    const root_node = try reopened.cache.get(root.addr);
    defer root_node.release();
    try testing.expect(reopened.owns(root_node));
    // From a ledger record.
    var record = ledger.bootstrapRecord(rs, 2, vol.seq.load(), .oneMember("set-uuid-fixture".*));
    const via_ledger = try KvTree.openFromLedger(gpa, fresh, &record, .native_slot, vol.seq);
    defer via_ledger.destroy();
    try testing.expectEqual(root, via_ledger.rootPtr());
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Corrupt, KvTree.openFromLedger(gpa, fresh, &record, .control, vol.seq));
    try log.testing.expectAllFired();
    // A guest slot tree publishes no ledger root: its root is tree 0's.
    const guest = try vol.createTree(.{ .slot = key.guestForestSlot(3) });
    try testing.expectEqual(@as(?ledger.TreeRoot, null), guest.ledgerRoot());
    var two: [2]ledger.TreeRoot = undefined;
    try testing.expectEqual(@as(usize, 1), roots(&.{ t, guest }, &two).len);
}

test "create floors the root's seq above the extent's residue: a tree re-created after a crash into the recycled extent of a never-named root resurrects none of the dead incarnation's frames" {
    const gpa = testing.allocator;
    var vol = try smo.TestVolume.init(gpa, .{});
    defer vol.deinit();
    // Incarnation 1: a first-touch root, records appended into its extent,
    // nothing naming the root durably (no interior Put, no ledger root).
    const dead = try vol.createTree(.{ .slot = key.guestForestSlot(3) });
    var buf: [64]u8 = undefined;
    try dead.insert(&ikey(1), testValue(1, 24, &buf));
    try dead.insert(&ikey(2), testValue(2, 24, &buf));
    try vol.flushDirty(dead);
    const dead_root = dead.rootPtr();
    // The crash: the allocation and the mint were never durable — the
    // next incarnation reseeds the handle at the same watermark and sees
    // the extent free again.
    var fresh = try vol.freshCache();
    defer fresh.deinit();
    var alloc = try alloc_ext.ExtentAllocator.format(gpa, vol.cfg.extents, vol.cfg.reserve, vol.cfg.pending_cap);
    defer alloc.deinit();
    var seq = node_seq.NodeSeqHandle.shared(0);
    const reborn = try KvTree.create(gpa, fresh, &alloc, .{ .slot = key.guestForestSlot(5) }, &seq);
    defer reborn.destroy();
    try testing.expectEqual(dead_root.addr, reborn.rootPtr().addr);
    try testing.expect(reborn.rootPtr().seq > dead_root.seq);
    try testing.expectEqual(@as(?[]u8, null), try reborn.lookup(gpa, &ikey(1)));
    try testing.expectEqual(@as(?[]u8, null), try reborn.lookup(gpa, &ikey(2)));
}

test "the routing law: slot-tree kinds route by the key's slot (the key must decode as the tagged kind), control by kind 8, interior by header id — slot trees through the slot ‖ separator key; alloc deltas and unmounted slots route nowhere" {
    var vol = try smo.TestVolume.init(testing.allocator, .{});
    defer vol.deinit();
    const native = try vol.createTree(.native);
    const control = try vol.createTree(.control);
    const guest = try vol.createTree(.{ .slot = key.guestForestSlot(7) });
    const trees = [_]*KvTree{ native, control, guest };
    try testing.expectEqual(native, routeContent(&trees, key.TREE_INODES, &key.inodeKey(5)));
    try testing.expectEqual(native, routeContent(&trees, key.TREE_DENTRIES, &key.dentryKey(1, 2, 0)));
    try testing.expectEqual(native, routeContent(&trees, key.TREE_XATTRS, &key.xattrKey(1, 2, 0)));
    try testing.expectEqual(native, routeContent(&trees, key.TREE_BLOCK_REFS, &key.blockRefKey(1, 2, 9, 0)));
    try testing.expectEqual(guest, routeContent(&trees, key.TREE_INODES, &key.inodeKey(key.guestLocalIno(7, 9))));
    try testing.expectEqual(control, routeContent(&trees, key.TREE_CONTROL, "slot_state:3"));
    // Nowhere: an unmounted slot, a disagreeing (tag, key) pair, a kindless
    // key, an allocator delta.
    try testing.expect(routeContent(&trees, key.TREE_INODES, &key.inodeKey(key.guestLocalIno(8, 9))) == null);
    try testing.expect(routeContent(&trees, key.TREE_INODES, &key.dentryKey(1, 2, 0)) == null);
    try testing.expect(routeContent(&trees, key.TREE_INODES, &[_]u8{ 0, 0, 0, 0, 0, 0, 0, 5 }) == null);
    try testing.expect(routeContent(&trees, alloc_ext.tree_alloc_reserved, &alloc_ext.extentKey(4)) == null);
    // Interior: a slot tree's pointer record rides `slot ‖ separator`; the
    // tree applies the separator. The control tree's rides the separator.
    const jk = native.interiorJournalKey(&key.inodeKey(100));
    const r = route(&trees, key.KIND_INTERIOR, 1, jk.slice()).?;
    try testing.expectEqual(native, r.tree);
    try testing.expectEqualSlices(u8, &key.inodeKey(100), r.key);
    const gk = guest.interiorJournalKey(&KEY_SPACE_MAX);
    try testing.expectEqual(guest, route(&trees, key.KIND_INTERIOR, 1, gk.slice()).?.tree);
    const ck = control.interiorJournalKey("slot_state:3");
    const cr = route(&trees, key.TREE_CONTROL, 2, ck.slice()).?;
    try testing.expectEqual(control, cr.tree);
    try testing.expectEqualStrings("slot_state:3", cr.key);
    try testing.expect(route(&trees, key.KIND_INTERIOR, 1, key.interiorJournalKey(key.guestForestSlot(8), "x").slice()) == null);
    try testing.expect(route(&trees, key.TREE_INODES, 1, &key.inodeKey(1)) == null);
}

test "replay LWW gate: a replayed record at-or-below the node's newest seq for its key is skipped as applied; a newer one lands with its own seq" {
    const gpa = testing.allocator;
    var vol = try smo.TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    try t.insert(&ikey(5), testValue(5, 24, &buf));
    const newest = vol.seq.load();
    // At or below: skipped (the old value stands).
    try t.applyReplayed(&ikey(5), newest, .put, testValue(55, 24, &buf), newest);
    try expectLookup(t, &ikey(5), testValue(5, 24, &buf));
    try t.applyReplayed(&ikey(5), newest - 1, .delete, &.{}, newest - 1);
    try expectLookup(t, &ikey(5), testValue(5, 24, &buf));
    // Above: applied; the handle is floored above it.
    try t.applyReplayed(&ikey(5), newest + 100, .put, testValue(555, 24, &buf), newest + 100);
    try expectLookup(t, &ikey(5), testValue(555, 24, &buf));
    try testing.expect(vol.seq.load() >= newest + 100);
    // A replayed interior record on a single-leaf tree is unroutable: dropped, never loud.
    try testing.expect(!try t.applyReplayedInterior(&KEY_SPACE_MAX, 1, newest + 200, .put, &key.encodeInteriorValue(0, 1), newest + 200));
    try testing.expectError(error.InvalidKey, t.applyReplayedInterior("", 1, 1, .put, &.{}, 1));
    // A tombstone carrying a value (a frame kind mislabelled by a replay)
    // is refused typed at the apply — the frame encoder never sees it.
    try testing.expectError(error.InvalidRecord, t.applyReplayed(&ikey(5), newest + 300, .delete, testValue(5, 24, &buf), newest + 300));
    try expectLookup(t, &ikey(5), testValue(555, 24, &buf));
    // A delta replays as a delta: folded into the key's newest base.
    try t.applyReplayed(&ikey(6), newest + 301, .put, &(rec.InodeValue{ .mode = 0o100644, .nlink = 1 }).encode(), newest + 301);
    try t.applyReplayed(&ikey(6), newest + 302, .delta, (rec.InodeDelta{ .mask = rec.DELTA_SIZE, .fields = .{ .size = 77 } }).encode().slice(), newest + 302);
    const folded = (try t.lookup(testing.allocator, &ikey(6))).?;
    defer testing.allocator.free(folded);
    try testing.expectEqual(@as(u64, 77), (try rec.InodeValue.decode(folded)).size);
}

test "a journal frame's kind maps 1:1 onto the record kind (put, delta, delete)" {
    try testing.expectEqual(RecordKind.put, kindOf(.put));
    try testing.expectEqual(RecordKind.delta, kindOf(.delta));
    try testing.expectEqual(RecordKind.delete, kindOf(.delete));
}
