//! Durable block references — the ledger that RIDES the layout transaction
//! (charter §5.6; `docs/porting/data_alloc_reclaim.md` §2). PORTABLE over any
//! store with the kv_fs `range`/`lookup` shape (`RefStore`): the Linux
//! `meta/backend.KvBackend` is one binding, the RAM store below is the test's.
//!
//! ONE record per REFERENCE: key `06 ‖ vol_tag ‖ block_idx ‖ owner_ino ‖
//! block_index` (`meta/key.zig`), value `{version 1, flags(bit0 map blob,
//! bit1 SHARED), reserved 0}` (`meta/record.zig`). `refcount(block)` is the
//! population of the `(vol_tag, block_idx)` prefix — ONE range probe
//! (`block_ref_probes`). WHY not a count (an RMW across two inos with no
//! common lock loses increments) and not a delta (a Δ with no base folds to
//! absent; "first reference?" is not knowable race-free): a Put at a key
//! nobody else writes is idempotent and a release is a Delete (DREF:L117).
//!
//! The delta rides the SAME tx as the layout mutation: `stageRefs(tx, ino,
//! old_map, new_map)` is the ONE site the layout publish calls (the
//! `RefsSeam` `data/layout.zig` installs) — per index whose binding changed
//! `+new`/`−prev`, an idempotent re-bind nothing, every `(released r,
//! taken r)` pair over ONE reference cancelled so a passthrough clip never
//! strips a durable SHARED bit. Measured on the reference: 40.1 ns per +1,
//! 51.8 ns per +1/−1, 46 B of journal per reference; the port's frame is
//! `recordFrameLen(29, 4)` = 50 B (the 16 B v4 header plus the tree tag
//! byte the spec's 49 omitted — the tie test restates the arithmetic).
//!
//! Three laws complete it: the per-ino DEFERRED-OP ACCUMULATOR for sites
//! that mutate the RAM map and persist later (drained only into a save that
//! persists the WHOLE map — a delta-class commit would durably claim
//! releases for shadow bindings its folded map does not reflect); the
//! RELEASE LAW (a release decrements RAM only if THIS owner's durable record
//! existed at release time — a Delete of an absent key is a no-op in the
//! tree and must be one in RAM, else the decrement lands on whichever LIVE
//! owner holds the block now, generic/749); and the reclaimed-ino DESTROY
//! RIDER (a corpse's releases ride the SAME journal entry as its destroy,
//! chunked to the whole-entry cap, so neither half-state is representable).
//!
//! The ledger is ALWAYS ON (decision 0002 ledger, Q1): no engaged predicate,
//! no stamp seam. The derived layout walk survives as the fsck C8 ORACLE
//! (`verifyRefs`; `meta_kv_block_refs_drift` is fed by the VERDICT site
//! alone) with a one-directional OFFLINE repair (release records whose
//! owner's layout does not name the block; NEVER fabricate) and as the
//! mount-time seed of a volume whose population is empty.

const std = @import("std");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const knobs = @import("../core/knobs.zig");
const key = @import("../meta/key.zig");
const record = @import("../meta/record.zig");
const tx_mod = @import("../meta/tx.zig");
const tree = @import("../meta/tree.zig");
const journal = @import("../meta/journal.zig");
const jc = @import("../meta/journal_core.zig");

pub const KvTx = tx_mod.KvTx;

// ---------------------------------------------------------------------------
// Constants.
// ---------------------------------------------------------------------------

/// Journal bytes ONE reference record costs: the tag + the 16 B v4 record
/// header + the 29 B key + the 4 B value (`journal.recordFrameLen`).
pub const REF_RECORD_JOURNAL_BYTES: u64 = journal.recordFrameLen(key.BLOCK_REF_KEY_LEN, record.BLOCK_REF_VALUE_LEN);

/// The mount-recovery scan's page: 512 records per `range` call keeps a
/// page under 32 KiB of owned copies while the volume scan streams
/// (REFS:L552; RT:L4575).
pub const SCAN_PAGE: usize = 512;

/// Drift rows logged with their owners before the report truncates: enough
/// to name the publishing site, never a log storm (RT:L4871).
pub const DRIFT_LOG_ROWS: usize = 8;

// ---------------------------------------------------------------------------
// Stats (spec §7).
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// Must-stay-0: fed by the VERDICT sites only (fsck C8 confirmed, the
    /// quiesced mount verify) — the raw comparison convicts nothing.
    pub var meta_kv_block_refs_drift: counters.Tripwire = .init("meta_kv_block_refs_drift");
    pub var reclaim_destroy_refused_release_failed: counters.Tripwire = .init("reclaim_destroy_refused_release_failed");
    pub var meta_kv_block_refs_staged: counters.ShardedCounter = .{};
    pub var meta_kv_block_refs_released: counters.ShardedCounter = .{};
    pub var meta_kv_block_refs_unresolved: counters.ShardedCounter = .{};
    pub var block_ref_probes: counters.ShardedCounter = .{};
    pub var block_release_skipped_no_record: counters.ShardedCounter = .{};
    pub var reclaim_release_destroy_joint_commits: counters.ShardedCounter = .{};
    pub var reclaim_single_ino_chunked_destroys: counters.ShardedCounter = .{};
    pub var fsck_repair_class_c8: counters.ShardedCounter = .{};
    pub var pack_ledger_exempted: counters.ShardedCounter = .{};
    pub var inflight_exempted: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "meta_kv_block_refs_staged", "meta_kv_block_refs_released", "meta_kv_block_refs_unresolved", "block_ref_probes", "block_release_skipped_no_record", "reclaim_release_destroy_joint_commits", "reclaim_single_ino_chunked_destroys", "fsck_repair_class_c8", "pack_ledger_exempted", "inflight_exempted" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"meta_kv_block_refs_drift\":{d},\"reclaim_destroy_refused_release_failed\":{d}}}", .{ counters.tripwireCount("meta_kv_block_refs_drift"), counters.tripwireCount("reclaim_destroy_refused_release_failed") });
}

// ---------------------------------------------------------------------------
// References and ops.
// ---------------------------------------------------------------------------

pub const Reference = struct {
    vol_tag: u64,
    block_idx: u64,
    owner_ino: u64,
    /// `key.BLOCK_INDEX_MAP_BLOB` = the owner's indirect map blob.
    block_index: u32,

    pub fn eql(a: Reference, b: Reference) bool {
        return a.vol_tag == b.vol_tag and a.block_idx == b.block_idx and a.owner_ino == b.owner_ino and a.block_index == b.block_index;
    }

    pub fn encodeKey(self: Reference) [key.BLOCK_REF_KEY_LEN]u8 {
        return key.blockRefKey(self.vol_tag, self.block_idx, self.owner_ino, self.block_index);
    }

    pub fn isMapBlob(self: Reference) bool {
        return self.block_index == key.BLOCK_INDEX_MAP_BLOB;
    }
};

pub const RefOp = struct {
    ref: Reference,
    /// `true` = Put (taken), `false` = Delete (released).
    take: bool,
    /// The SHARED bit rides a taken record (the clone protocol); never set
    /// on a release.
    shared: bool = false,
};

pub fn taken(ref: Reference) RefOp {
    return .{ .ref = ref, .take = true };
}

pub fn takenShared(ref: Reference) RefOp {
    return .{ .ref = ref, .take = true, .shared = true };
}

pub fn released(ref: Reference) RefOp {
    return .{ .ref = ref, .take = false };
}

/// Drop every `(released r, taken r)` pair over ONE reference (a
/// passthrough clip `bk:off:len → bk:off:len'` re-binds the same block):
/// the record survives UNCHANGED, where a from-scratch re-Put would strip
/// a durable SHARED bit. A `takenShared` is never cancelled (its bit is
/// the point). In place; returns the surviving length (REFS:L298-343).
pub fn cancelSameReferencePairs(ops: []RefOp) usize {
    if (ops.len > MAX_CANCEL_OPS) return ops.len;
    var cancelled: CancelSet = .empty;
    var i: usize = 0;
    while (i < ops.len) : (i += 1) {
        if (cancelled.isSet(i) or ops[i].take) continue;
        var j: usize = 0;
        while (j < ops.len) : (j += 1) {
            if (j == i or cancelled.isSet(j)) continue;
            if (ops[j].take and !ops[j].shared and ops[j].ref.eql(ops[i].ref)) {
                cancelled.set(i);
                cancelled.set(j);
                break;
            }
        }
    }
    var n: usize = 0;
    i = 0;
    while (i < ops.len) : (i += 1) {
        if (cancelled.isSet(i)) continue;
        ops[n] = ops[i];
        n += 1;
    }
    return n;
}

/// A fixed bitset over an op list: a publish frame is O(batch) (≤ a few
/// thousand entries); 4096 covers `RUN_LEN_MAX` and the coalesced publish
/// cap with headroom, and a larger frame degrades to no cancellation
/// (correct — one extra Delete+Put pair per re-bound index, never a lost
/// record).
const MAX_CANCEL_OPS: usize = 4096;
const CancelSet = std.StaticBitSet(MAX_CANCEL_OPS);

/// Stage every op into `tx` on the refs kind: a take is a Put of the 4 B
/// value, a release a Delete. Counts `meta_kv_block_refs_{staged,released}`.
pub fn stageOps(tx: *KvTx, ops: []const RefOp) Allocator.Error!void {
    for (ops) |op| {
        const k = op.ref.encodeKey();
        if (op.take) {
            const v = record.BlockRefValue{ .map_blob = op.ref.isMapBlob(), .shared = op.shared };
            try tx.stagePut(key.TREE_BLOCK_REFS, &k, &v.encode());
            stats.meta_kv_block_refs_staged.inc();
        } else {
            try tx.stageDelete(key.TREE_BLOCK_REFS, &k);
            stats.meta_kv_block_refs_released.inc();
        }
    }
}

// ---------------------------------------------------------------------------
// The layout seam: per-index diff of two block maps.
// ---------------------------------------------------------------------------

/// One block-map binding as the layout layer sees it: the index, the
/// volume and the BLOCK index (`offset / block_size`, resolved by the
/// router from the mapping's `vol_tag`; the port resolves by tag, so the
/// only "unresolved" class is a tag naming no mounted volume).
pub const Binding = struct {
    block_index: u32,
    vol_tag: u64,
    block_idx: u64,
    shared: bool = false,

    fn sameBlock(a: Binding, b: Binding) bool {
        return a.vol_tag == b.vol_tag and a.block_idx == b.block_idx;
    }

    fn ref(self: Binding, ino: u64) Reference {
        return .{ .vol_tag = self.vol_tag, .block_idx = self.block_idx, .owner_ino = ino, .block_index = self.block_index };
    }
};

/// `block_ref_ops_for_map_swap(old, new)`: both ascending by `block_index`
/// (the canonical map order); `+new`/`−prev` per index whose binding
/// changed, nothing for an idempotent re-bind, a SHARED new binding always
/// re-Put (RT:L9224-9251). Appends to `out`.
pub fn mapSwapOps(gpa: Allocator, ino: u64, old: []const Binding, new: []const Binding, out: *std.ArrayList(RefOp)) Allocator.Error!void {
    var i: usize = 0;
    var j: usize = 0;
    while (i < old.len or j < new.len) {
        if (j == new.len or (i < old.len and old[i].block_index < new[j].block_index)) {
            try out.append(gpa, released(old[i].ref(ino)));
            i += 1;
        } else if (i == old.len or new[j].block_index < old[i].block_index) {
            try out.append(gpa, if (new[j].shared) takenShared(new[j].ref(ino)) else taken(new[j].ref(ino)));
            j += 1;
        } else {
            assert(old[i].block_index == new[j].block_index);
            if (!old[i].sameBlock(new[j])) {
                try out.append(gpa, released(old[i].ref(ino)));
                try out.append(gpa, if (new[j].shared) takenShared(new[j].ref(ino)) else taken(new[j].ref(ino)));
            } else if (new[j].shared and !old[i].shared) {
                try out.append(gpa, takenShared(new[j].ref(ino)));
            }
            i += 1;
            j += 1;
        }
    }
}

/// The ONE site the layout publish calls: the swap diff, the pair
/// cancellation, the staging — into the SAME `tx` as the layout record, so
/// the entry count per publish is unchanged (+0 journal entries).
pub fn stageRefs(tx: *KvTx, ino: u64, old: []const Binding, new: []const Binding) Allocator.Error!void {
    const a = tx.arena.allocator();
    var ops: std.ArrayList(RefOp) = .empty;
    try mapSwapOps(a, ino, old, new, &ops);
    const n = cancelSameReferencePairs(ops.items);
    try stageOps(tx, ops.items[0..n]);
}

/// The indirect map blob's own reference (`BLOCK_INDEX_MAP_BLOB`): `+`/`−`
/// the blob in the same tx as `save_metadata_to_backend_ext`.
pub fn stageMapBlobRef(tx: *KvTx, ino: u64, vol_tag: u64, old_blob: ?u64, new_blob: ?u64) Allocator.Error!void {
    if (old_blob) |o| if (new_blob == null or new_blob.? != o) {
        try stageOps(tx, &.{released(.{ .vol_tag = vol_tag, .block_idx = o, .owner_ino = ino, .block_index = key.BLOCK_INDEX_MAP_BLOB })});
    };
    if (new_blob) |n| if (old_blob == null or old_blob.? != n) {
        try stageOps(tx, &.{taken(.{ .vol_tag = vol_tag, .block_idx = n, .owner_ino = ino, .block_index = key.BLOCK_INDEX_MAP_BLOB })});
    };
}

/// The vtable `data/layout.zig`'s `publishLayout` holds (its test double
/// records calls; the product installs `default`).
pub const RefsSeam = struct {
    ctx: ?*anyopaque,
    stageRefs: *const fn (?*anyopaque, *KvTx, u64, []const Binding, []const Binding) Allocator.Error!void,

    fn defaultStage(_: ?*anyopaque, tx: *KvTx, ino: u64, old: []const Binding, new: []const Binding) Allocator.Error!void {
        return stageRefs(tx, ino, old, new);
    }

    pub const default: RefsSeam = .{ .ctx = null, .stageRefs = &defaultStage };

    pub fn call(self: RefsSeam, tx: *KvTx, ino: u64, old: []const Binding, new: []const Binding) Allocator.Error!void {
        return self.stageRefs(self.ctx, tx, ino, old, new);
    }
};

// ---------------------------------------------------------------------------
// The store face and the range probe.
// ---------------------------------------------------------------------------

pub const Error = error{ OutOfMemory, Io, Corrupt };

/// What the refs module needs of a store: the inclusive range scan of one
/// kind and the point lookup — the kv_fs Store shape, bound as a vtable so
/// the oracle and the probe compile once.
pub const RefStore = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        range: *const fn (*anyopaque, Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void,
        lookup: *const fn (*anyopaque, Allocator, k: []const u8) Error!?[]u8,
    };

    /// Bind any store with `range(gpa, kind, start, end, max, out)` and
    /// `lookup(gpa, kind, k)` (the `KvBackend`/`MemStore` shape).
    pub fn from(comptime S: type, s: *S) RefStore {
        const Adapter = struct {
            fn range(ctx: *anyopaque, gpa: Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void {
                const st: *S = @ptrCast(@alignCast(ctx));
                st.range(gpa, key.TREE_BLOCK_REFS, start, end, max, out) catch |e| return mapStoreError(e);
            }
            fn lookup(ctx: *anyopaque, gpa: Allocator, k: []const u8) Error!?[]u8 {
                const st: *S = @ptrCast(@alignCast(ctx));
                return st.lookup(gpa, key.TREE_BLOCK_REFS, k) catch |e| return mapStoreError(e);
            }
            const vtable: VTable = .{ .range = &range, .lookup = &lookup };
        };
        return .{ .ctx = s, .vtable = &Adapter.vtable };
    }

    fn mapStoreError(e: anyerror) Error {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.Corrupt,
            else => error.Io,
        };
    }

    /// The half-open `[lo, hi)` scan over an inclusive-range store: the
    /// store's `end` is `hi` and a record AT `hi` is dropped here.
    fn scanHalfOpen(self: RefStore, gpa: Allocator, r: key.KeyRange, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void {
        const before = out.items.len;
        try self.vtable.range(self.ctx, gpa, r.lo.slice(), r.hi.slice(), max, out);
        var n = out.items.len;
        while (n > before and !key.lessThan(out.items[n - 1].key, r.hi.slice())) : (n -= 1) {
            var kv = out.items[n - 1];
            kv.deinit(gpa);
        }
        out.items.len = n;
    }

    fn recordExists(self: RefStore, gpa: Allocator, ref: Reference) Error!bool {
        const k = ref.encodeKey();
        const v = (try self.vtable.lookup(self.ctx, gpa, &k)) orelse return false;
        gpa.free(v);
        return true;
    }
};

/// `refcount(block)` = ONE range probe over the `(vol_tag, block_idx)`
/// prefix (`block_ref_probes`). An unreadable record is `Corrupt` — a
/// malformed reference refuses the population rather than under-counting.
pub fn refcount(store: RefStore, gpa: Allocator, vol_tag: u64, block_idx: u64) Error!u32 {
    stats.block_ref_probes.inc();
    var out: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &out);
    try store.scanHalfOpen(gpa, key.blockRange(vol_tag, block_idx), std.math.maxInt(usize), &out);
    for (out.items) |kv| {
        _ = key.BlockRefKey.decode(kv.key) catch return error.Corrupt;
        _ = record.BlockRefValue.decode(kv.value) catch return error.Corrupt;
    }
    return @intCast(out.items.len);
}

/// Every reference record of one block (the oracle's per-block judgement
/// and the repair's input): owner-sorted copies into `out`.
pub fn blockRecords(store: RefStore, gpa: Allocator, vol_tag: u64, block_idx: u64, out: *std.ArrayList(Reference)) Error!void {
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &kvs);
    try store.scanHalfOpen(gpa, key.blockRange(vol_tag, block_idx), std.math.maxInt(usize), &kvs);
    for (kvs.items) |kv| {
        const k = key.BlockRefKey.decode(kv.key) catch return error.Corrupt;
        _ = record.BlockRefValue.decode(kv.value) catch return error.Corrupt;
        try out.append(gpa, .{ .vol_tag = k.volTag(), .block_idx = k.blockIdx(), .owner_ino = k.ownerIno(), .block_index = k.blockIndex() });
    }
}

/// The mount-recovery scan: every reference on `vol_tag`, paged
/// `SCAN_PAGE` at a time, block indices appended to `idxs` (one per
/// record — `seedFromDurableRefs` folds them). Returns null for an EMPTY
/// population (the caller runs the derived walk; the volume "genuinely
/// holds no data"). A malformed record refuses the population.
pub fn volumePopulation(store: RefStore, gpa: Allocator, vol_tag: u64, idxs: *std.ArrayList(u64)) Error!?usize {
    const range = key.volumeRange(vol_tag);
    var cursor: key.KeyBuf = range.lo;
    var total: usize = 0;
    while (true) {
        var page: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &page);
        try store.scanHalfOpen(gpa, .{ .lo = cursor, .hi = range.hi }, SCAN_PAGE, &page);
        if (page.items.len == 0) break;
        for (page.items) |kv| {
            const k = key.BlockRefKey.decode(kv.key) catch return error.Corrupt;
            _ = record.BlockRefValue.decode(kv.value) catch return error.Corrupt;
            try idxs.append(gpa, k.blockIdx());
            total += 1;
        }
        if (page.items.len < SCAN_PAGE) break;
        cursor = key.keySuccessor(page.items[page.items.len - 1].key);
    }
    if (total == 0) {
        log.info("data volume tag {x}: the durable reference population is EMPTY — declining the seed; the derived layout walk seeds the allocator", .{vol_tag});
        return null;
    }
    return total;
}

// ---------------------------------------------------------------------------
// The per-ino deferred-op accumulator (§2.3).
// ---------------------------------------------------------------------------

/// `pending_block_refs: ino → [RefOp]`. A site that mutates the RAM map
/// WITHOUT persisting NOTES its delta here (inside the same meta-stripe
/// section); whichever save persists the map DRAINS it (order preserved:
/// take-then-release on one index keeps its order) into the same tx; a
/// failed commit re-notes. THE DRAIN LAW: notes drain only into saves that
/// persist the WHOLE map (`requiresWholeMapSave`), so any future deferring
/// site is correct by construction rather than by remembering.
pub const PendingRefs = struct {
    gpa: Allocator,
    lock: jc.SpinLock = .{},
    map: std.AutoHashMapUnmanaged(u64, std.ArrayList(RefOp)) = .empty,

    pub fn init(gpa: Allocator) PendingRefs {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *PendingRefs) void {
        var it = self.map.valueIterator();
        while (it.next()) |list| list.deinit(self.gpa);
        self.map.deinit(self.gpa);
    }

    /// `note_block_ref_ops`.
    pub fn note(self: *PendingRefs, ino: u64, ops: []const RefOp) Allocator.Error!void {
        if (ops.len == 0) return;
        self.lock.lock();
        defer self.lock.unlock();
        const gop = try self.map.getOrPut(self.gpa, ino);
        if (!gop.found_existing) gop.value_ptr.* = .empty;
        try gop.value_ptr.appendSlice(self.gpa, ops);
    }

    /// `take_block_ref_ops`: the ino's notes in note order; caller frees.
    pub fn take(self: *PendingRefs, ino: u64) Allocator.Error![]RefOp {
        self.lock.lock();
        defer self.lock.unlock();
        var kv = self.map.fetchRemove(ino) orelse return try self.gpa.alloc(RefOp, 0);
        return kv.value.toOwnedSlice(self.gpa);
    }

    /// `retract_block_ref_ops`: a FAILED promotion's own ops leave (it never
    /// published the RAM entry, so re-noting would land as C8 drift).
    pub fn retract(self: *PendingRefs, ino: u64, ops: []const RefOp) void {
        self.lock.lock();
        defer self.lock.unlock();
        const list = self.map.getPtr(ino) orelse return;
        for (ops) |op| {
            for (list.items, 0..) |have, i| {
                if (have.take == op.take and have.ref.eql(op.ref)) {
                    _ = list.orderedRemove(i);
                    break;
                }
            }
        }
        if (list.items.len == 0) {
            var kv = self.map.fetchRemove(ino).?;
            kv.value.deinit(self.gpa);
        }
    }

    pub fn hasDeferred(self: *PendingRefs, ino: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        const list = self.map.get(ino) orelse return false;
        return list.items.len != 0;
    }

    /// The drain law's predicate: a publish-class save is forced off the
    /// layout-delta path while notes exist.
    pub fn requiresWholeMapSave(self: *PendingRefs, ino: u64) bool {
        return self.hasDeferred(ino);
    }

    /// Drain into a WHOLE-map save's tx (the caller asserted the class);
    /// on a failed commit the caller re-notes the returned slice — the
    /// notes themselves, in note order (the cancellation runs on a copy in
    /// the tx's arena).
    pub fn drainInto(self: *PendingRefs, tx: *KvTx, ino: u64) Allocator.Error![]RefOp {
        const ops = try self.take(ino);
        errdefer self.gpa.free(ops);
        const work = try tx.arena.allocator().dupe(RefOp, ops);
        const n = cancelSameReferencePairs(work);
        try stageOps(tx, work[0..n]);
        return ops;
    }

    pub fn free(self: *PendingRefs, ops: []RefOp) void {
        self.gpa.free(ops);
    }
};

// ---------------------------------------------------------------------------
// The release law (§2.4) and the destroy rider (§2.5).
// ---------------------------------------------------------------------------

/// The releases whose record EXISTS now (probed under the owner's exclusive
/// guard): only these may decrement RAM; the rest are counted
/// `block_release_skipped_no_record`. Takes are passed through.
pub fn admitReleases(store: RefStore, gpa: Allocator, ops: []const RefOp, out: *std.ArrayList(RefOp)) Error!usize {
    var skipped: usize = 0;
    for (ops) |op| {
        if (op.take or try store.recordExists(gpa, op.ref)) {
            try out.append(gpa, op);
        } else {
            skipped += 1;
            stats.block_release_skipped_no_record.inc();
        }
    }
    return skipped;
}

pub const DestroyVerdict = union(enum) {
    /// The releases were staged beside the destroy; `held` = how many.
    destroyed: struct { held: usize },
    /// A live (`nlink > 0`) or missing ino stages NOTHING.
    skipped,
};

/// How a corpse's releases ride its destroy entry.
pub const DestroyPlan = union(enum) {
    /// One entry: destroy records + every release.
    whole,
    /// Past the cap: `entries` entries under ONE held exclusive guard,
    /// releases first, the inode record LAST (every committed prefix is a
    /// corpse the next sweep converges on).
    chunked: struct { entries: u64, releases_per_entry: u64 },
};

/// `destroy_bytes` = the inode + xattr Deletes' framed bytes; the plan
/// keeps every entry under `journal.MAX_ENTRY_LEN` using the arithmetic the
/// admission enforces (`tx.entryLenFor`'s). A destroy that cannot fit even
/// alone is a caller bug (asserted).
pub fn planDestroy(destroy_bytes: u64, releases: u64) DestroyPlan {
    const cap = journal.MAX_ENTRY_LEN - journal.ENTRY_HDR_LEN;
    assert(destroy_bytes <= cap);
    const release_bytes = releases * REF_RECORD_JOURNAL_BYTES;
    if (destroy_bytes + release_bytes <= cap) return .whole;
    // Releases ride entries of their own up to the cap; the destroy rides
    // the LAST entry with whatever fits beside it.
    const per_entry = cap / REF_RECORD_JOURNAL_BYTES;
    const beside_destroy = (cap - destroy_bytes) / REF_RECORD_JOURNAL_BYTES;
    const remaining = releases - beside_destroy;
    const release_entries = (remaining + per_entry - 1) / per_entry;
    return .{ .chunked = .{ .entries = release_entries + 1, .releases_per_entry = per_entry } };
}

/// Test seams of the corpse sweep, read once per sweep.
pub const DestroySeams = struct {
    /// `SQUEEZEFS_TEST_CORPSE_SWEEP_FAIL_DESTROY`: refuse every chunk's destroy.
    fail_destroy: bool = false,
    /// `SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER`: stop a chunked destroy after N entries (0 = never).
    stop_after: u32 = 0,

    pub fn fromEnv(env: *const std.process.Environ.Map) DestroySeams {
        return .{
            .fail_destroy = knobs.boolKnob("SQUEEZEFS_TEST_CORPSE_SWEEP_FAIL_DESTROY", env),
            .stop_after = knobs.intKnob("SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER", u32, env),
        };
    }
};

pub const RiderError = Error || error{
    /// The seam refused the destroy: the releases were planned, nothing
    /// landed; a retry is a leak-free no-op by the release law.
    DestroyRefused,
};

/// The destroy rider for ONE corpse: admits its releases (the witness),
/// stages them into `tx` beside the caller's destroy records (the caller
/// stages those; `nlink` decides `Skipped`). The multi-ino batching and the
/// chunk iteration are `stageCorpseReleases`' callers' — this is the
/// per-corpse half every shape shares.
pub fn stageCorpseReleases(store: RefStore, gpa: Allocator, tx: *KvTx, nlink: u32, ops: []const RefOp, seams: DestroySeams) RiderError!DestroyVerdict {
    if (nlink > 0) return .skipped;
    var admitted: std.ArrayList(RefOp) = .empty;
    defer admitted.deinit(gpa);
    _ = try admitReleases(store, gpa, ops, &admitted);
    if (seams.fail_destroy) {
        stats.reclaim_destroy_refused_release_failed.fire("corpse destroy refused by the test seam");
        return error.DestroyRefused;
    }
    try stageOps(tx, admitted.items);
    stats.reclaim_release_destroy_joint_commits.inc();
    return .{ .destroyed = .{ .held = admitted.items.len } };
}

/// A chunked single-ino destroy's cursor: releases in `per_entry` slices,
/// the destroy (the caller's records) riding the last; honours the
/// stop-after seam. One `next` = one entry's releases staged into `tx`.
pub const ChunkedDestroy = struct {
    ops: []const RefOp,
    plan: DestroyPlan,
    seams: DestroySeams,
    pos: usize = 0,
    entries: u32 = 0,
    stopped: bool = false,

    pub fn init(ops: []const RefOp, plan: DestroyPlan, seams: DestroySeams) ChunkedDestroy {
        assert(plan == .chunked);
        return .{ .ops = ops, .plan = plan, .seams = seams };
    }

    pub const Chunk = struct {
        /// The destroy records ride this (last) entry.
        carries_destroy: bool,
        releases: usize,
    };

    /// Stage the next entry's releases; null once every planned entry was
    /// emitted (the last carries the destroy) — or when stopped.
    pub fn next(self: *ChunkedDestroy, tx: *KvTx) Allocator.Error!?Chunk {
        if (self.entries >= self.plan.chunked.entries) return null;
        if (self.seams.stop_after != 0 and self.entries >= self.seams.stop_after) {
            self.stopped = true;
            return null;
        }
        const per = self.plan.chunked.releases_per_entry;
        const remaining = self.ops.len - self.pos;
        const last = self.entries + 1 == self.plan.chunked.entries;
        const n: usize = if (last) remaining else @intCast(@min(per, remaining));
        try stageOps(tx, self.ops[self.pos .. self.pos + n]);
        self.pos += n;
        self.entries += 1;
        if (last) stats.reclaim_single_ino_chunked_destroys.inc();
        return .{ .carries_destroy = last, .releases = n };
    }
};

// ---------------------------------------------------------------------------
// The C8 oracle (§2.6).
// ---------------------------------------------------------------------------

/// What the layout walk yields per live inode (corpses INCLUDED): its
/// bindings on ONE data volume and its indirect blob there, if any.
pub const OwnerBindings = struct {
    ino: u64,
    bindings: []const Binding,
    map_blob: ?u64 = null,
};

pub const Drift = struct {
    block_idx: u64,
    durable: u32,
    derived: u32,
};

pub const DriftReport = struct {
    rows: []Drift,
    /// Blocks excused: an open pack block, a live in-flight owner.
    pack_exempted: u64,
    inflight_exempted: u64,

    pub fn deinit(self: *DriftReport, gpa: Allocator) void {
        gpa.free(self.rows);
        self.* = undefined;
    }

    pub fn clean(self: DriftReport) bool {
        return self.rows.len == 0;
    }
};

/// The exemptions fsck C8 applies before a finding stands (FSCK:L5948).
pub const Exemptions = struct {
    ctx: ?*anyopaque,
    /// An OPEN pack block (pinned at `committed + mid-flight + 1`).
    isOpenPack: *const fn (?*anyopaque, block_idx: u64) bool,
    /// A block with a live in-flight owner (`InflightAllocGuard`).
    isInflight: *const fn (?*anyopaque, block_idx: u64) bool,
};

/// Durable population per block from the ledger scan vs the derived census
/// over `owners`; rows for every disagreeing block, ascending. OBSERVES and
/// convicts nothing — `confirm` is the verdict site.
pub fn verifyRefs(gpa: Allocator, store: RefStore, vol_tag: u64, owners: []const OwnerBindings, exempt: ?Exemptions) Error!DriftReport {
    var durable: std.AutoArrayHashMapUnmanaged(u64, u32) = .empty;
    defer durable.deinit(gpa);
    var derived: std.AutoArrayHashMapUnmanaged(u64, u32) = .empty;
    defer derived.deinit(gpa);

    var idxs: std.ArrayList(u64) = .empty;
    defer idxs.deinit(gpa);
    _ = try volumePopulationQuiet(store, gpa, vol_tag, &idxs);
    for (idxs.items) |idx| (try durable.getOrPutValue(gpa, idx, 0)).value_ptr.* += 1;
    for (owners) |o| {
        for (o.bindings) |b| if (b.vol_tag == vol_tag) {
            (try derived.getOrPutValue(gpa, b.block_idx, 0)).value_ptr.* += 1;
        };
        if (o.map_blob) |blob| (try derived.getOrPutValue(gpa, blob, 0)).value_ptr.* += 1;
    }

    var rows: std.ArrayList(Drift) = .empty;
    errdefer rows.deinit(gpa);
    var report: DriftReport = .{ .rows = &.{}, .pack_exempted = 0, .inflight_exempted = 0 };
    var it = durable.iterator();
    while (it.next()) |e| {
        const d = derived.get(e.key_ptr.*) orelse 0;
        if (d != e.value_ptr.*) try judge(&rows, gpa, &report, exempt, e.key_ptr.*, e.value_ptr.*, d);
    }
    var it2 = derived.iterator();
    while (it2.next()) |e| {
        if (durable.contains(e.key_ptr.*)) continue;
        try judge(&rows, gpa, &report, exempt, e.key_ptr.*, 0, e.value_ptr.*);
    }
    std.mem.sort(Drift, rows.items, {}, driftLess);
    report.rows = try rows.toOwnedSlice(gpa);
    return report;
}

fn judge(rows: *std.ArrayList(Drift), gpa: Allocator, report: *DriftReport, exempt: ?Exemptions, idx: u64, durable: u32, derived: u32) Allocator.Error!void {
    if (exempt) |x| {
        if (x.isOpenPack(x.ctx, idx)) {
            report.pack_exempted += 1;
            stats.pack_ledger_exempted.inc();
            return;
        }
        if (x.isInflight(x.ctx, idx)) {
            report.inflight_exempted += 1;
            stats.inflight_exempted.inc();
            return;
        }
    }
    try rows.append(gpa, .{ .block_idx = idx, .durable = durable, .derived = derived });
}

fn driftLess(_: void, a: Drift, b: Drift) bool {
    return a.block_idx < b.block_idx;
}

/// `volumePopulation` without the empty-population INFO line (the oracle
/// scans an empty volume legitimately).
fn volumePopulationQuiet(store: RefStore, gpa: Allocator, vol_tag: u64, idxs: *std.ArrayList(u64)) Error!usize {
    const range = key.volumeRange(vol_tag);
    var cursor: key.KeyBuf = range.lo;
    var total: usize = 0;
    while (true) {
        var page: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &page);
        try store.scanHalfOpen(gpa, .{ .lo = cursor, .hi = range.hi }, SCAN_PAGE, &page);
        if (page.items.len == 0) break;
        for (page.items) |kv| {
            const k = key.BlockRefKey.decode(kv.key) catch return error.Corrupt;
            _ = record.BlockRefValue.decode(kv.value) catch return error.Corrupt;
            try idxs.append(gpa, k.blockIdx());
            total += 1;
        }
        if (page.items.len < SCAN_PAGE) break;
        cursor = key.keySuccessor(page.items[page.items.len - 1].key);
    }
    return total;
}

/// THE verdict site: a confirmed finding (fsck's memoized re-verify kept
/// it, or the quiesced mount-init verify) feeds `meta_kv_block_refs_drift`
/// and logs the first `DRIFT_LOG_ROWS` offenders WITH their owners.
pub fn confirm(gpa: Allocator, store: RefStore, vol_tag: u64, report: DriftReport) void {
    if (report.clean()) return;
    stats.meta_kv_block_refs_drift.fire("durable block references disagree with the derived layout census");
    for (report.rows[0..@min(report.rows.len, DRIFT_LOG_ROWS)]) |row| {
        var owners: std.ArrayList(Reference) = .empty;
        defer owners.deinit(gpa);
        blockRecords(store, gpa, vol_tag, row.block_idx, &owners) catch {};
        log.err("C8 drift: vol {x} block {d}: durable {d} vs derived {d}; durable owners (ino:index):", .{ vol_tag, row.block_idx, row.durable, row.derived });
        for (owners.items) |o| log.err("  ← ino {d} index {d}", .{ o.owner_ino, o.block_index });
    }
}

/// `layout_names_block_at`: three-valued — an unreadable justification is
/// never `not_named` (a refusal protects the whole finding).
pub const Naming = enum { named, not_named, unknown };

pub const LayoutJudge = struct {
    ctx: ?*anyopaque,
    namesBlock: *const fn (?*anyopaque, owner_ino: u64, block_index: u32, block_idx: u64) Naming,
};

pub const RepairError = Error || error{
    /// The verification read and the release must not interleave with a
    /// live publish: repair runs OFFLINE only.
    OnlineRepairRefused,
    /// A record's justification could not be read: the whole finding is
    /// refused (nothing released).
    UnknownJustification,
};

pub const RepairPlan = struct {
    /// Records to release (Delete): every record of a drifting block whose
    /// owner's layout does not name it.
    releases: []Reference,
    /// Rows where the layout names a block with NO record: reported, never
    /// fabricated.
    unfabricated: u64,

    pub fn deinit(self: *RepairPlan, gpa: Allocator) void {
        gpa.free(self.releases);
        self.* = undefined;
    }
};

/// The one-directional repair plan: every record of each drifting block is
/// judged BEFORE any is released; `unknown` refuses the whole finding; the
/// other direction (a layout naming a block with no record) is counted and
/// never fabricated (DREF:L200-214).
pub fn repairPlan(gpa: Allocator, store: RefStore, vol_tag: u64, report: DriftReport, judge_fn: LayoutJudge, online: bool) RepairError!RepairPlan {
    if (online) return error.OnlineRepairRefused;
    var releases: std.ArrayList(Reference) = .empty;
    errdefer releases.deinit(gpa);
    var unfabricated: u64 = 0;
    for (report.rows) |row| {
        if (row.durable < row.derived) {
            unfabricated += 1;
            continue;
        }
        var recs: std.ArrayList(Reference) = .empty;
        defer recs.deinit(gpa);
        try blockRecords(store, gpa, vol_tag, row.block_idx, &recs);
        var stale: std.ArrayList(Reference) = .empty;
        defer stale.deinit(gpa);
        for (recs.items) |r| {
            switch (judge_fn.namesBlock(judge_fn.ctx, r.owner_ino, r.block_index, r.block_idx)) {
                .named => {},
                .not_named => try stale.append(gpa, r),
                .unknown => return error.UnknownJustification,
            }
        }
        try releases.appendSlice(gpa, stale.items);
    }
    return .{ .releases = try releases.toOwnedSlice(gpa), .unfabricated = unfabricated };
}

/// Stage the plan's releases (`fsck_repair_class_c8` per record).
pub fn stageRepair(tx: *KvTx, plan: RepairPlan) Allocator.Error!void {
    for (plan.releases) |r| {
        try stageOps(tx, &.{released(r)});
        stats.fsck_repair_class_c8.inc();
    }
}

/// `SQUEEZEFS_BLOCK_REFS_VERIFY`: run the oracle at mount.
pub fn verifyAtMount(env: *const std.process.Environ.Map) bool {
    return knobs.boolKnob("SQUEEZEFS_BLOCK_REFS_VERIFY", env);
}

// ---------------------------------------------------------------------------
// Tests. The RAM store: a sorted (key, value) list with the kv_fs shape.
// ---------------------------------------------------------------------------

const testing = std.testing;

const RamStore = struct {
    gpa: Allocator,
    rows: std.ArrayList(Row) = .empty,
    const Row = struct { k: []u8, v: []u8 };
    const StoreError = error{ OutOfMemory, Io };

    fn init(gpa: Allocator) RamStore {
        return .{ .gpa = gpa };
    }

    fn deinit(self: *RamStore) void {
        for (self.rows.items) |r| {
            self.gpa.free(r.k);
            self.gpa.free(r.v);
        }
        self.rows.deinit(self.gpa);
    }

    fn find(self: *RamStore, k: []const u8) struct { idx: usize, hit: bool } {
        var lo: usize = 0;
        var hi: usize = self.rows.items.len;
        while (lo < hi) {
            const mid = lo + (hi - lo) / 2;
            switch (key.order(k, self.rows.items[mid].k)) {
                .lt => hi = mid,
                .gt => lo = mid + 1,
                .eq => return .{ .idx = mid, .hit = true },
            }
        }
        return .{ .idx = lo, .hit = false };
    }

    fn put(self: *RamStore, k: []const u8, v: []const u8) !void {
        const f = self.find(k);
        const vd = try self.gpa.dupe(u8, v);
        if (f.hit) {
            self.gpa.free(self.rows.items[f.idx].v);
            self.rows.items[f.idx].v = vd;
            return;
        }
        const kd = try self.gpa.dupe(u8, k);
        try self.rows.insert(self.gpa, f.idx, .{ .k = kd, .v = vd });
    }

    fn del(self: *RamStore, k: []const u8) void {
        const f = self.find(k);
        if (!f.hit) return;
        const r = self.rows.orderedRemove(f.idx);
        self.gpa.free(r.k);
        self.gpa.free(r.v);
    }

    /// Apply a tx's staged records (Put/Delete) and drop it.
    fn commit(self: *RamStore, t: *KvTx) !void {
        defer t.deinit();
        for (t.staged.items) |s| {
            try testing.expectEqual(key.TREE_BLOCK_REFS, s.tree_id);
            switch (s.kind) {
                .put => try self.put(s.key, s.value),
                .delete => self.del(s.key),
                .delta => unreachable,
            }
        }
    }

    pub fn lookup(self: *RamStore, gpa: Allocator, kind: u8, k: []const u8) StoreError!?[]u8 {
        assert(kind == key.TREE_BLOCK_REFS);
        const f = self.find(k);
        if (!f.hit) return null;
        return try gpa.dupe(u8, self.rows.items[f.idx].v);
    }

    pub fn range(self: *RamStore, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) StoreError!void {
        assert(kind == key.TREE_BLOCK_REFS);
        var i = self.find(start).idx;
        var n: usize = 0;
        while (i < self.rows.items.len and n < max) : (i += 1) {
            const r = self.rows.items[i];
            if (key.lessThan(end, r.k)) break;
            try out.append(gpa, .{ .key = try gpa.dupe(u8, r.k), .value = try gpa.dupe(u8, r.v) });
            n += 1;
        }
    }

    fn store(self: *RamStore) RefStore {
        return RefStore.from(RamStore, self);
    }
};

fn bind(index: u32, idx: u64) Binding {
    return .{ .block_index = index, .vol_tag = 0x10, .block_idx = idx };
}

test "the journal cost of ONE reference is the framed record: tag + 16 B header + 29 B key + 4 B value = 50 B (the spec's 49 omitted the tag)" {
    try testing.expectEqual(@as(u64, 1 + 16 + 29 + 4), REF_RECORD_JOURNAL_BYTES);
    try testing.expectEqual(@as(u64, 50), REF_RECORD_JOURNAL_BYTES);
}

test "same_reference_pairs_cancel_and_nothing_else_moves" {
    const r1: Reference = .{ .vol_tag = 1, .block_idx = 9, .owner_ino = 5, .block_index = 0 };
    const r2: Reference = .{ .vol_tag = 1, .block_idx = 10, .owner_ino = 5, .block_index = 1 };
    var ops = [_]RefOp{ released(r1), taken(r2), taken(r1), released(r2), takenShared(r2) };
    const n = cancelSameReferencePairs(&ops);
    // (−r1, +r1) cancel; (+r2, −r2) cancel; the shared take of r2 survives.
    try testing.expectEqual(@as(usize, 1), n);
    try testing.expect(ops[0].shared and ops[0].take and ops[0].ref.eql(r2));
    // A shared take is never cancelled against a release.
    var ops2 = [_]RefOp{ released(r1), takenShared(r1) };
    try testing.expectEqual(@as(usize, 2), cancelSameReferencePairs(&ops2));
}

test "stageRefs: a map swap stages +new/−prev per changed index in the SAME tx; a re-bind stages nothing; the indirect blob carries its own reference" {
    const gpa = testing.allocator;
    var t = KvTx.init(gpa, @src());
    defer t.deinit();
    const old = [_]Binding{ bind(0, 100), bind(1, 101), bind(2, 102) };
    const new = [_]Binding{ bind(0, 100), bind(1, 201), bind(3, 303) };
    const staged_before = stats.meta_kv_block_refs_staged.load();
    const released_before = stats.meta_kv_block_refs_released.load();
    try RefsSeam.default.call(&t, 77, &old, &new);
    // index 0 re-bound identically: nothing; 1 changed: −101 +201; 2 pruned: −102; 3 new: +303.
    try testing.expectEqual(@as(usize, 4), t.staged.items.len);
    try testing.expectEqual(staged_before + 2, stats.meta_kv_block_refs_staged.load());
    try testing.expectEqual(released_before + 2, stats.meta_kv_block_refs_released.load());
    var takes: usize = 0;
    for (t.staged.items) |s| {
        const k = try key.BlockRefKey.decode(s.key);
        try testing.expectEqual(@as(u64, 77), k.ownerIno());
        try testing.expectEqual(@as(u64, 0x10), k.volTag());
        switch (s.kind) {
            .put => {
                takes += 1;
                const v = try record.BlockRefValue.decode(s.value);
                try testing.expect(!v.map_blob and !v.shared);
            },
            .delete => try testing.expectEqual(@as(usize, 0), s.value.len),
            .delta => unreachable,
        }
    }
    try testing.expectEqual(@as(usize, 2), takes);
    // The blob: − old, + new, both in this tx, flagged map_blob.
    try stageMapBlobRef(&t, 77, 0x10, 500, 501);
    try testing.expectEqual(@as(usize, 6), t.staged.items.len);
    const blob = t.staged.items[5];
    try testing.expect((try key.BlockRefKey.decode(blob.key)).isMapBlob());
    try testing.expect((try record.BlockRefValue.decode(blob.value)).map_blob);
    try stageMapBlobRef(&t, 77, 0x10, 501, 501); // unchanged: nothing
    try testing.expectEqual(@as(usize, 6), t.staged.items.len);
    // A SHARED new binding over the same block re-Puts with the bit.
    var t2 = KvTx.init(gpa, @src());
    defer t2.deinit();
    var shared_new = [_]Binding{bind(0, 100)};
    shared_new[0].shared = true;
    try stageRefs(&t2, 77, &.{bind(0, 100)}, &shared_new);
    try testing.expectEqual(@as(usize, 1), t2.staged.items.len);
    try testing.expect((try record.BlockRefValue.decode(t2.staged.items[0].value)).shared);
}

test "refcount(block) is ONE range probe over the (vol_tag, block_idx) prefix; a malformed record refuses the population" {
    const gpa = testing.allocator;
    var ram = RamStore.init(gpa);
    defer ram.deinit();
    const store = ram.store();
    var t = KvTx.init(gpa, @src());
    try stageRefs(&t, 5, &.{}, &.{ bind(0, 42), bind(1, 43) });
    try stageRefs(&t, 6, &.{}, &.{bind(9, 42)});
    try ram.commit(&t);
    const probes = stats.block_ref_probes.load();
    try testing.expectEqual(@as(u32, 2), try refcount(store, gpa, 0x10, 42));
    try testing.expectEqual(@as(u32, 1), try refcount(store, gpa, 0x10, 43));
    try testing.expectEqual(@as(u32, 0), try refcount(store, gpa, 0x10, 44));
    try testing.expectEqual(@as(u32, 0), try refcount(store, gpa, 0x11, 42));
    try testing.expectEqual(probes + 4, stats.block_ref_probes.load());
    // The half-open bound: a record at the NEXT block's first key is not ours.
    try ram.put(&key.blockRefKey(0x10, 43, 0, 0), &(record.BlockRefValue{ .map_blob = false, .shared = false }).encode());
    try testing.expectEqual(@as(u32, 2), try refcount(store, gpa, 0x10, 42));
    // The population: 4 records; the volume next door is empty and declines.
    var idxs: std.ArrayList(u64) = .empty;
    defer idxs.deinit(gpa);
    try testing.expectEqual(@as(?usize, 4), try volumePopulation(store, gpa, 0x10, &idxs));
    try testing.expectEqual(@as(?usize, null), try volumePopulation(store, gpa, 0x11, &idxs));
    // A malformed value refuses the whole population on either layout.
    try ram.put(&key.blockRefKey(0x10, 50, 1, 0), &.{ 9, 0, 0, 0 });
    try testing.expectError(error.Corrupt, refcount(store, gpa, 0x10, 50));
    try testing.expectError(error.Corrupt, volumePopulation(store, gpa, 0x10, &idxs));
}

test "the deferred-op accumulator: notes drain in order into a whole-map save only; a failed promotion retracts its own ops" {
    const gpa = testing.allocator;
    var pending = PendingRefs.init(gpa);
    defer pending.deinit();
    const r1: Reference = .{ .vol_tag = 1, .block_idx = 9, .owner_ino = 5, .block_index = 0 };
    const r2: Reference = .{ .vol_tag = 1, .block_idx = 10, .owner_ino = 5, .block_index = 0 };
    try testing.expect(!pending.requiresWholeMapSave(5));
    try pending.note(5, &.{taken(r1)});
    try pending.note(5, &.{ released(r1), taken(r2) });
    try testing.expect(pending.requiresWholeMapSave(5)); // the drain law forces a publish-class save
    try testing.expect(!pending.requiresWholeMapSave(6));
    var t = KvTx.init(gpa, @src());
    defer t.deinit();
    const drained = try pending.drainInto(&t, 5);
    defer pending.free(drained);
    try testing.expectEqual(@as(usize, 3), drained.len); // take-then-release order preserved in the returned notes
    try testing.expect(drained[0].take and drained[0].ref.eql(r1));
    try testing.expect(!drained[1].take and drained[1].ref.eql(r1));
    try testing.expectEqual(@as(usize, 1), t.staged.items.len); // the (+r1, −r1) pair cancelled; +r2 staged
    try testing.expect(!pending.hasDeferred(5));
    // A failed commit re-notes; a failed promotion retracts only its own.
    try pending.note(5, drained);
    pending.retract(5, &.{taken(r2)});
    const rest = try pending.take(5);
    defer pending.free(rest);
    try testing.expectEqual(@as(usize, 2), rest.len);
    try testing.expectEqual(@as(usize, 0), (try pending.take(5)).len);
}

test "the release law: a release decrements only when THIS owner's record exists (generic/749); the rest are counted skipped" {
    const gpa = testing.allocator;
    var ram = RamStore.init(gpa);
    defer ram.deinit();
    const store = ram.store();
    var t = KvTx.init(gpa, @src());
    try stageRefs(&t, 5, &.{}, &.{bind(0, 42)});
    try ram.commit(&t);
    const mine: Reference = .{ .vol_tag = 0x10, .block_idx = 42, .owner_ino = 5, .block_index = 0 };
    const not_mine: Reference = .{ .vol_tag = 0x10, .block_idx = 42, .owner_ino = 6, .block_index = 0 };
    var admitted: std.ArrayList(RefOp) = .empty;
    defer admitted.deinit(gpa);
    const skipped_before = stats.block_release_skipped_no_record.load();
    const skipped = try admitReleases(store, gpa, &.{ released(mine), released(not_mine), taken(not_mine) }, &admitted);
    try testing.expectEqual(@as(usize, 1), skipped);
    try testing.expectEqual(skipped_before + 1, stats.block_release_skipped_no_record.load());
    try testing.expectEqual(@as(usize, 2), admitted.items.len);
    try testing.expect(admitted.items[0].ref.eql(mine));
    try testing.expect(admitted.items[1].take);
}

test "the destroy rider: a corpse's releases ride its destroy; a live ino stages nothing; the seam's refusal lands nothing and a retry is a no-op" {
    const gpa = testing.allocator;
    var ram = RamStore.init(gpa);
    defer ram.deinit();
    const store = ram.store();
    var t0 = KvTx.init(gpa, @src());
    try stageRefs(&t0, 5, &.{}, &.{ bind(0, 42), bind(1, 43) });
    try ram.commit(&t0);
    const ops = [_]RefOp{ released(bind(0, 42).ref(5)), released(bind(1, 43).ref(5)), released(bind(2, 44).ref(5)) };
    var t1 = KvTx.init(gpa, @src());
    defer t1.deinit();
    try testing.expectEqual(DestroyVerdict.skipped, try stageCorpseReleases(store, gpa, &t1, 1, &ops, .{}));
    try testing.expectEqual(@as(usize, 0), t1.staged.items.len);
    const joint = stats.reclaim_release_destroy_joint_commits.load();
    const v = try stageCorpseReleases(store, gpa, &t1, 0, &ops, .{});
    try testing.expectEqual(@as(usize, 2), v.destroyed.held); // 44 had no record: skipped by the witness
    try testing.expectEqual(@as(usize, 2), t1.staged.items.len);
    try testing.expectEqual(joint + 1, stats.reclaim_release_destroy_joint_commits.load());
    // The seam: planned, refused, nothing staged.
    var t2 = KvTx.init(gpa, @src());
    defer t2.deinit();
    const refused = counters.tripwireCount("reclaim_destroy_refused_release_failed");
    stats.reclaim_destroy_refused_release_failed.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.DestroyRefused, stageCorpseReleases(store, gpa, &t2, 0, &ops, .{ .fail_destroy = true }));
    try log.testing.expectAllFired();
    try testing.expectEqual(refused + 1, counters.tripwireCount("reclaim_destroy_refused_release_failed"));
    try testing.expectEqual(@as(usize, 0), t2.staged.items.len);
    const rc42 = try refcount(store, gpa, 0x10, 42);
    const rc43 = try refcount(store, gpa, 0x10, 43);
    try testing.expectEqual(@as(u32, 2), rc42 + rc43);
}

test "planDestroy: whole under the cap; chunked past it with the destroy riding the last entry; the stop-after seam halts between entries" {
    const cap = journal.MAX_ENTRY_LEN - journal.ENTRY_HDR_LEN;
    try testing.expectEqual(DestroyPlan.whole, planDestroy(200, 10));
    const fits = (cap - 200) / REF_RECORD_JOURNAL_BYTES;
    try testing.expectEqual(DestroyPlan.whole, planDestroy(200, fits));
    const plan = planDestroy(200, fits + 1);
    try testing.expectEqual(@as(u64, 2), plan.chunked.entries);
    try testing.expectEqual(cap / REF_RECORD_JOURNAL_BYTES, plan.chunked.releases_per_entry);
    const big = planDestroy(200, 3 * (cap / REF_RECORD_JOURNAL_BYTES) + 5);
    try testing.expectEqual(@as(u64, 4), big.chunked.entries);
    // Every entry's releases stay under the cap (the tie with the admission arithmetic).
    try testing.expect(plan.chunked.releases_per_entry * REF_RECORD_JOURNAL_BYTES <= cap);

    const gpa = testing.allocator;
    var ops: std.ArrayList(RefOp) = .empty;
    defer ops.deinit(gpa);
    var i: u64 = 0;
    while (i < fits + 1) : (i += 1) try ops.append(gpa, released(.{ .vol_tag = 1, .block_idx = i, .owner_ino = 5, .block_index = @intCast(i) }));
    var cd = ChunkedDestroy.init(ops.items, plan, .{});
    var t = KvTx.init(gpa, @src());
    defer t.deinit();
    const c1 = (try cd.next(&t)).?;
    try testing.expect(!c1.carries_destroy);
    // The first entry takes up to a full entry's worth; whatever is left
    // (here nothing) rides beside the destroy.
    try testing.expectEqual(@min(plan.chunked.releases_per_entry, ops.items.len), @as(u64, c1.releases));
    var framed: u64 = journal.ENTRY_HDR_LEN;
    for (t.staged.items) |s| framed += journal.recordFrameLen(s.key.len, s.value.len);
    try testing.expect(framed <= journal.MAX_ENTRY_LEN);
    const c2 = (try cd.next(&t)).?;
    try testing.expect(c2.carries_destroy);
    try testing.expectEqual(ops.items.len, c1.releases + c2.releases);
    try testing.expectEqual(@as(?ChunkedDestroy.Chunk, null), try cd.next(&t));
    // The crash-between-chunks state: stopped after 1 entry.
    var cd2 = ChunkedDestroy.init(ops.items, plan, .{ .stop_after = 1 });
    _ = (try cd2.next(&t)).?;
    try testing.expectEqual(@as(?ChunkedDestroy.Chunk, null), try cd2.next(&t));
    try testing.expect(cd2.stopped);
}

const Judge = struct {
    owners: []const OwnerBindings,
    unknown_ino: u64 = 0,
    fn names(ctx: ?*anyopaque, owner_ino: u64, block_index: u32, block_idx: u64) Naming {
        const self: *const Judge = @ptrCast(@alignCast(ctx.?));
        if (owner_ino == self.unknown_ino) return .unknown;
        for (self.owners) |o| if (o.ino == owner_ino) {
            if (block_index == key.BLOCK_INDEX_MAP_BLOB) return if (o.map_blob != null and o.map_blob.? == block_idx) .named else .not_named;
            for (o.bindings) |bd| if (bd.block_index == block_index and bd.block_idx == block_idx) return .named;
            return .not_named;
        };
        return .not_named;
    }
    fn judge(self: *const Judge) LayoutJudge {
        return .{ .ctx = @constCast(self), .namesBlock = &names };
    }
};

test "C8 oracle: a healthy volume has no drift; the raw comparison convicts nothing; a stale record is released offline and a missing one is never fabricated; unknown refuses; online refuses" {
    const gpa = testing.allocator;
    var ram = RamStore.init(gpa);
    defer ram.deinit();
    const store = ram.store();
    var t = KvTx.init(gpa, @src());
    try stageRefs(&t, 5, &.{}, &.{ bind(0, 42), bind(1, 43) });
    try stageRefs(&t, 6, &.{}, &.{bind(0, 42)});
    try stageMapBlobRef(&t, 6, 0x10, null, 90);
    try ram.commit(&t);
    const owners = [_]OwnerBindings{
        .{ .ino = 5, .bindings = &.{ bind(0, 42), bind(1, 43) } },
        .{ .ino = 6, .bindings = &.{bind(0, 42)}, .map_blob = 90 },
    };
    var clean = try verifyRefs(gpa, store, 0x10, &owners, null);
    defer clean.deinit(gpa);
    try testing.expect(clean.clean());
    const drift_before = counters.tripwireCount("meta_kv_block_refs_drift");
    confirm(gpa, store, 0x10, clean); // clean ⇒ no verdict
    try testing.expectEqual(drift_before, counters.tripwireCount("meta_kv_block_refs_drift"));

    // Drift both ways: a stale record for ino 7 on block 43 (durable 2 vs derived 1),
    // and ino 5 names block 44 with no record (durable 0 vs derived 1).
    try ram.put(&key.blockRefKey(0x10, 43, 7, 3), &(record.BlockRefValue{ .map_blob = false, .shared = false }).encode());
    const owners2 = [_]OwnerBindings{
        .{ .ino = 5, .bindings = &.{ bind(0, 42), bind(1, 43), bind(2, 44) } },
        .{ .ino = 6, .bindings = &.{bind(0, 42)}, .map_blob = 90 },
    };
    var report = try verifyRefs(gpa, store, 0x10, &owners2, null);
    defer report.deinit(gpa);
    try testing.expectEqual(@as(usize, 2), report.rows.len);
    try testing.expectEqual(Drift{ .block_idx = 43, .durable = 2, .derived = 1 }, report.rows[0]);
    try testing.expectEqual(Drift{ .block_idx = 44, .durable = 0, .derived = 1 }, report.rows[1]);
    // Observation convicted nothing; the verdict site does, loudly.
    try testing.expectEqual(drift_before, counters.tripwireCount("meta_kv_block_refs_drift"));
    stats.meta_kv_block_refs_drift.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1 + (1 + 2) + (1 + 0)); // the tripwire; per row a header + one line per durable owner (2 on block 43, 0 on 44)
    confirm(gpa, store, 0x10, report);
    try log.testing.expectAllFired();
    try testing.expectEqual(drift_before + 1, counters.tripwireCount("meta_kv_block_refs_drift"));

    const judge_ok = Judge{ .owners = &owners2 };
    try testing.expectError(error.OnlineRepairRefused, repairPlan(gpa, store, 0x10, report, judge_ok.judge(), true));
    var plan = try repairPlan(gpa, store, 0x10, report, judge_ok.judge(), false);
    defer plan.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), plan.releases.len); // ino 7's record alone; ino 5's survives
    try testing.expectEqual(@as(u64, 7), plan.releases[0].owner_ino);
    try testing.expectEqual(@as(u64, 1), plan.unfabricated); // block 44: reported, never fabricated
    const judge_unknown = Judge{ .owners = &owners2, .unknown_ino = 5 };
    try testing.expectError(error.UnknownJustification, repairPlan(gpa, store, 0x10, report, judge_unknown.judge(), false));
    // Apply: the stale record goes; the volume verifies clean except the unfabricated row.
    var rt = KvTx.init(gpa, @src());
    const repaired_before = stats.fsck_repair_class_c8.load();
    try stageRepair(&rt, plan);
    try testing.expectEqual(repaired_before + 1, stats.fsck_repair_class_c8.load());
    try ram.commit(&rt);
    var after = try verifyRefs(gpa, store, 0x10, &owners2, null);
    defer after.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), after.rows.len);
    try testing.expectEqual(@as(u64, 44), after.rows[0].block_idx);
    // Exemptions: an in-flight owner on block 44 excuses it.
    const Ex = struct {
        fn pack(_: ?*anyopaque, _: u64) bool {
            return false;
        }
        fn inflight(_: ?*anyopaque, idx: u64) bool {
            return idx == 44;
        }
    };
    var excused = try verifyRefs(gpa, store, 0x10, &owners2, .{ .ctx = null, .isOpenPack = &Ex.pack, .isInflight = &Ex.inflight });
    defer excused.deinit(gpa);
    try testing.expect(excused.clean());
    try testing.expectEqual(@as(u64, 1), excused.inflight_exempted);
}

test "stats JSON names every family" {
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"block_ref_probes\":") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"meta_kv_block_refs_drift\":") != null);
}
