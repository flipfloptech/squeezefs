//! The immutable node snapshot: the latch-free read view of one cached node
//! (charter §5.3 "Node cache": "arc-swapped immutable snapshot (merged
//! sorted view + on-disk bset list; readers latch-free, binary search, slice
//! views)"; §5.2 "Hot fold reads serve from a per-key materialized folded
//! head in the dirty delta + a bounded 8-entry memo on the immutable
//! snapshot"; survey `docs/porting/kv_tree_cache_commit_survey.md` §2.8,
//! §7.1; the reference's `node_cache.rs` `RecordIndex`/`FoldMemo`/
//! `NodeSnapshot`).
//!
//! Two sorted sources, both immutable once published:
//!
//! - `base`: the `RecordIndex` — a k-way merge of every SERIALIZED bset
//!   image (the extent's frames oldest → newest, then every frozen delta
//!   image) into one `(key asc, seq desc, newer source first)` index of
//!   24-byte `IdxRec`s that borrow the images zero-copy. Shared across
//!   publishes by refcount; a freeze extends it INCREMENTALLY (the k-way
//!   rebuild per freeze was 37 % of the reference's serial create path).
//! - `overlay`: the open-delta records, a `(key, seq)` ascending `Run` of
//!   refcounted chunks the dirty half derives the next from by copying the
//!   chunks it touches (`RunEdit`): a publish is one reference, an apply
//!   O(records applied). (The former stable/tail split re-cloned the whole
//!   overlay every 8 applies — 15 µs of a create's apply CPU on a leaf of
//!   thousands of open records, `docs/evidence/rung-10-create-path.md`.)
//!
//! Fold precedence per key is the record SEQ across both sources, never the
//! source: the overlay usually rides above the base, but two in-flight
//! windows apply in either order (decision 0003 §AW.4) and the
//! checkpointer's freeze may run between the two applies, so an earlier
//! window's record can land in the overlay BELOW a later window's record
//! the base already holds. Every fold merges the two seq-sorted groups by
//! seq (`gather`), and a materialized head — the fold up to ITS record's
//! seq — serves only while the base holds nothing newer for the key.
//! `lookup` is the D7-slimmed read: the newest overlay record's head serves
//! with zero decodes; a bset-resident key probes the 8-cell populate-once
//! `FoldMemo`; only a cold fold runs THE algebra (`record.foldNewestFirst`)
//! — and property tests pin memo and head serves byte-equal to it over
//! histories whose apply order is not their seq order.
//!
//! Readers never allocate on the warm path (head serve, memo hit): the
//! views they hand out borrow the snapshot, valid for as long as the
//! caller holds its reference.

const std = @import("std");
const counters = @import("../core/counters.zig");
const key = @import("key.zig");
const rec = @import("record.zig");
const bset = @import("bset.zig");
const arc_swap = @import("../sync/arc_swap.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
pub const Arc = arc_swap.Arc;
pub const RecordRef = rec.RecordRef;
pub const RecordKind = rec.RecordKind;
pub const FoldError = rec.FoldError;

pub const stats = struct {
    /// `meta_kv_fold_head_serves`: point lookups served from the overlay
    /// head (zero decodes).
    pub var meta_kv_fold_head_serves: counters.ShardedCounter = .{};
    /// `meta_kv_fold_memo_hits` / `_misses`: the snapshot memo; a miss is
    /// counted exactly where a populate follows, so `hits / (hits + misses)`
    /// reads as the D7.b effectiveness rate.
    pub var meta_kv_fold_memo_hits: counters.ShardedCounter = .{};
    pub var meta_kv_fold_memo_misses: counters.ShardedCounter = .{};
    /// `meta_kv_fold_base_above`: point lookups that found the base holding
    /// a record NEWER than the overlay's newest for the key — an earlier
    /// window's record applied after a later window's was frozen (decision
    /// 0003 §AW.4 with a freeze between the two applies); the fold merged
    /// the two sources by seq instead of serving the head.
    pub var meta_kv_fold_base_above: counters.ShardedCounter = .{};
    /// `meta_kv_fold_memo_bytes`: Σ memo bytes alive, Drop-exact; rides the
    /// node-cache budget too.
    pub var meta_kv_fold_memo_bytes: counters.Gauge = .{};
    /// `meta_kv_node_index_bytes`: Σ `RecordIndex` array bytes alive
    /// (`RecordIndex.indexBytes`), deinit-exact; rides the node-cache
    /// budget too. Every published base index lives until the last
    /// snapshot over it dies, so this counts the retired ones a reader
    /// still holds as well as the current.
    pub var meta_kv_node_index_bytes: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// Refcounted byte buffers — the zero-copy currency of every view.
// ---------------------------------------------------------------------------

/// A refcounted heap buffer. Extent images are page-aligned (the O_DIRECT
/// read posture needs the grain, charter §5.1) and freed as such; every
/// other buffer is naturally aligned.
pub const SharedBuf = struct {
    refs: std.atomic.Value(u32),
    gpa: Allocator,
    data: []u8,
    extent: bool,

    /// A zeroed `len`-byte extent buffer, `NODE_PAGE`-aligned.
    pub fn createExtent(gpa: Allocator, len: usize) Allocator.Error!*SharedBuf {
        const data = try gpa.alignedAlloc(u8, .fromByteUnits(bset.NODE_PAGE), len);
        errdefer gpa.free(data);
        @memset(data, 0);
        const self = try gpa.create(SharedBuf);
        self.* = .{ .refs = .init(1), .gpa = gpa, .data = data, .extent = true };
        return self;
    }

    /// Wrap an allocation the caller made with `gpa` (ownership moves).
    pub fn fromOwned(gpa: Allocator, data: []u8) Allocator.Error!*SharedBuf {
        const self = try gpa.create(SharedBuf);
        self.* = .{ .refs = .init(1), .gpa = gpa, .data = data, .extent = false };
        return self;
    }

    pub fn copyFrom(gpa: Allocator, bytes: []const u8) Allocator.Error!*SharedBuf {
        const data = try gpa.dupe(u8, bytes);
        errdefer gpa.free(data);
        return fromOwned(gpa, data);
    }

    pub fn retain(self: *SharedBuf) *SharedBuf {
        _ = self.refs.fetchAdd(1, .monotonic);
        return self;
    }

    /// acq_rel: the last release sees every holder's accesses before the free.
    pub fn release(self: *SharedBuf) void {
        if (self.refs.fetchSub(1, .acq_rel) != 1) return;
        if (self.extent) {
            const aligned: []align(bset.NODE_PAGE) u8 = @alignCast(self.data);
            self.gpa.free(aligned);
        } else {
            self.gpa.free(self.data);
        }
        self.gpa.destroy(self);
    }
};

/// A refcounted view `[off, off + len)` of a `SharedBuf`.
pub const Bytes = struct {
    buf: *SharedBuf,
    off: u32,
    len: u32,

    /// The whole buffer (takes the caller's reference).
    pub fn whole(buf: *SharedBuf) Bytes {
        return .{ .buf = buf, .off = 0, .len = @intCast(buf.data.len) };
    }

    pub fn copy(gpa: Allocator, bytes: []const u8) Allocator.Error!Bytes {
        return whole(try SharedBuf.copyFrom(gpa, bytes));
    }

    pub fn fromOwned(gpa: Allocator, data: []u8) Allocator.Error!Bytes {
        return whole(try SharedBuf.fromOwned(gpa, data));
    }

    /// A sub-view sharing the buffer (one more reference).
    pub fn sub(self: Bytes, off: usize, len: usize) Bytes {
        assert(off + len <= self.len);
        return .{ .buf = self.buf.retain(), .off = @intCast(self.off + off), .len = @intCast(len) };
    }

    pub fn slice(self: Bytes) []const u8 {
        return self.buf.data[self.off..][0..self.len];
    }

    pub fn clone(self: Bytes) Bytes {
        return .{ .buf = self.buf.retain(), .off = self.off, .len = self.len };
    }

    pub fn deinit(self: Bytes) void {
        self.buf.release();
    }
};

// ---------------------------------------------------------------------------
// The merged record index.
// ---------------------------------------------------------------------------

/// One record's position in the merged index. 24 bytes: ~2,000-record
/// leaves index in ~48 KiB. The value starts right after the key in the
/// record wire (`header ‖ key ‖ value`), so no value offset is stored.
pub const IdxRec = struct {
    seq: u64,
    /// Offset of the key inside the source image.
    key_off: u32,
    val_len: u32,
    /// Index into `RecordIndex.sources`.
    src: u16,
    key_len: u16,
    kind: RecordKind,
};

comptime {
    assert(@sizeOf(IdxRec) == 24);
}

pub const Range = struct { lo: usize, hi: usize };

/// The merged-sorted view over every serialized source of a node.
///
/// Its RAM — the entries array and the sources table — is charged to the
/// owning cache's gauge at build (`indexBytes`) and credited exactly at
/// `deinit`: an index outlives the freeze that superseded it for as long
/// as a reader holds a snapshot over it, and the footprint note measured
/// ≈ 5 live indexes per mapped node (a quarter of the cache's RAM) that no
/// gauge named before this charge.
pub const RecordIndex = struct {
    /// Backing bset images, oldest → newest; entries borrow them zero-copy.
    sources: []Bytes,
    /// `(key asc, seq desc, newer source first)`.
    entries: []IdxRec,
    /// The owning cache's budget gauge (null for a standalone index).
    charge: ?*counters.Gauge,
    /// Exactly what `build`/`extendWith` charged: `indexBytes(entries.len,
    /// sources.len)`; `deinit` credits it.
    charged: u64,

    pub const BuildError = bset.ParseError || error{TooManySources};

    /// The RAM an index of `entries` records over `sources` images takes:
    /// the entries array and the sources side table (the images themselves
    /// are the extent's, charged with the mapping).
    pub fn indexBytes(entries: usize, sources: usize) u64 {
        return @as(u64, entries) * @sizeOf(IdxRec) + @as(u64, sources) * @sizeOf(Bytes);
    }

    fn chargeArrays(self: *RecordIndex) void {
        self.charged = indexBytes(self.entries.len, self.sources.len);
        if (self.charge) |g| g.add(self.charged);
        stats.meta_kv_node_index_bytes.add(self.charged);
    }

    /// Merge-build from bset images, OLDEST → NEWEST (append order), taking
    /// ownership of `sources` (the slice and its references, on every
    /// outcome). Ties on `(key, seq)` order the newer source first — the
    /// `bset.merge` convention. `charge` is the owning cache's gauge
    /// (`NodeCache.cached_bytes`), inherited by every `extendWith`.
    pub fn build(gpa: Allocator, sources: []Bytes, charge: ?*counters.Gauge) BuildError!RecordIndex {
        errdefer {
            for (sources) |s| s.deinit();
            gpa.free(sources);
        }
        if (sources.len > std.math.maxInt(u16)) return error.TooManySources;
        const views = try gpa.alloc(bset.BsetView, sources.len);
        var parsed: usize = 0;
        defer {
            for (views[0..parsed]) |*v| v.deinit(gpa);
            gpa.free(views);
        }
        var total: usize = 0;
        for (sources, 0..) |s, i| {
            views[i] = try bset.BsetView.parse(gpa, s.slice());
            parsed += 1;
            total += views[i].len();
        }
        const entries = try gpa.alloc(IdxRec, total);
        errdefer gpa.free(entries);
        const cursors = try gpa.alloc(usize, sources.len);
        defer gpa.free(cursors);
        @memset(cursors, 0);
        // K-way cursor merge over per-source (key asc, seq asc) views,
        // newest source scanned first so an equal (key, seq) prefers it.
        var n: usize = 0;
        while (true) {
            var best: ?struct { si: usize, r: RecordRef } = null;
            var si = sources.len;
            while (si > 0) {
                si -= 1;
                if (cursors[si] >= views[si].len()) continue;
                const r = views[si].record(cursors[si]);
                const better = if (best) |b| switch (key.order(r.key, b.r.key)) {
                    .lt => true,
                    .gt => false,
                    .eq => r.seq > b.r.seq,
                } else true;
                if (better) best = .{ .si = si, .r = r };
            }
            const b = best orelse break;
            cursors[b.si] += 1;
            entries[n] = idxRecOf(sources[b.si], @intCast(b.si), b.r);
            n += 1;
        }
        assert(n == total);
        var idx: RecordIndex = .{ .sources = sources, .entries = entries, .charge = charge, .charged = 0 };
        idx.normalizeRuns();
        idx.chargeArrays();
        return idx;
    }

    fn idxRecOf(src: Bytes, si: u16, r: RecordRef) IdxRec {
        const base = @intFromPtr(src.slice().ptr);
        const key_off = @intFromPtr(r.key.ptr) - base;
        assert(key_off + r.key.len + r.value.len <= src.len);
        // The value follows the key in the wire; the index relies on it.
        assert(@intFromPtr(r.value.ptr) == @intFromPtr(r.key.ptr) + r.key.len);
        return .{
            .seq = r.seq,
            .key_off = @intCast(key_off),
            .val_len = @intCast(r.value.len),
            .src = si,
            .key_len = @intCast(r.key.len),
            .kind = r.kind,
        };
    }

    /// Normalize each same-key run to `seq desc` (newer source breaking
    /// exact ties). The cursor merge orders records newest-first ACROSS
    /// sources, but WITHIN one source a multi-seq key is stored `seq asc`
    /// (a frozen delta carrying `Put` then `Delete` of one key is the
    /// everyday case), and the fold's input contract is newest-first.
    fn normalizeRuns(self: *RecordIndex) void {
        var i: usize = 0;
        while (i < self.entries.len) {
            var j = i + 1;
            while (j < self.entries.len and std.mem.eql(u8, self.keyAt(self.entries[j]), self.keyAt(self.entries[i]))) j += 1;
            if (j - i > 1) std.sort.pdq(IdxRec, self.entries[i..j], {}, seqDescSrcDesc);
            i = j;
        }
    }

    fn seqDescSrcDesc(_: void, a: IdxRec, b: IdxRec) bool {
        if (a.seq != b.seq) return a.seq > b.seq;
        return a.src > b.src;
    }

    /// Extend the merged view with ONE newer bset image (taking the
    /// caller's reference to `new_source`): a two-way linear merge of the
    /// existing entries with the new source's, O(existing + new), never
    /// re-parsing the old images. Exact `(key, seq)` ties order the new
    /// source first (idempotent replay re-freezing a record that already
    /// reached a bset).
    pub fn extendWith(self: *const RecordIndex, gpa: Allocator, new_source: Bytes) BuildError!RecordIndex {
        errdefer new_source.deinit();
        if (self.sources.len + 1 > std.math.maxInt(u16)) return error.TooManySources;
        var view = try bset.BsetView.parse(gpa, new_source.slice());
        defer view.deinit(gpa);
        const si: u16 = @intCast(self.sources.len);
        const sources = try gpa.alloc(Bytes, self.sources.len + 1);
        errdefer gpa.free(sources);
        for (self.sources, 0..) |s, i| sources[i] = s.clone();
        errdefer for (sources[0..si]) |s| s.deinit();
        sources[si] = new_source;
        // The new view's entries in output order: every same-key run reversed.
        const fresh = try gpa.alloc(IdxRec, view.len());
        defer gpa.free(fresh);
        var i: usize = 0;
        var f: usize = 0;
        while (i < view.len()) {
            var j = i + 1;
            while (j < view.len() and std.mem.eql(u8, view.record(j).key, view.record(i).key)) j += 1;
            var k = j;
            while (k > i) {
                k -= 1;
                fresh[f] = idxRecOf(new_source, si, view.record(k));
                f += 1;
            }
            i = j;
        }
        const entries = try gpa.alloc(IdxRec, self.entries.len + fresh.len);
        var a: usize = 0;
        var b: usize = 0;
        var n: usize = 0;
        while (a < self.entries.len and b < fresh.len) {
            const ea = self.entries[a];
            const eb = fresh[b];
            const take_a = switch (key.order(self.keyAt(ea), keyIn(new_source, eb))) {
                .lt => true,
                .gt => false,
                // Tie ⇒ the new (higher src) first.
                .eq => ea.seq > eb.seq,
            };
            if (take_a) {
                entries[n] = ea;
                a += 1;
            } else {
                entries[n] = eb;
                b += 1;
            }
            n += 1;
        }
        @memcpy(entries[n..][0 .. self.entries.len - a], self.entries[a..]);
        n += self.entries.len - a;
        @memcpy(entries[n..][0 .. fresh.len - b], fresh[b..]);
        var idx: RecordIndex = .{ .sources = sources, .entries = entries, .charge = self.charge, .charged = 0 };
        idx.chargeArrays();
        return idx;
    }

    pub fn deinit(self: *RecordIndex, gpa: Allocator) void {
        for (self.sources) |s| s.deinit();
        gpa.free(self.sources);
        gpa.free(self.entries);
        if (self.charged > 0) {
            if (self.charge) |g| g.sub(self.charged);
            stats.meta_kv_node_index_bytes.sub(self.charged);
        }
        self.* = undefined;
    }

    fn keyIn(src: Bytes, e: IdxRec) []const u8 {
        return src.slice()[e.key_off..][0..e.key_len];
    }

    pub inline fn keyAt(self: *const RecordIndex, e: IdxRec) []const u8 {
        return keyIn(self.sources[e.src], e);
    }

    pub inline fn valueAt(self: *const RecordIndex, e: IdxRec) []const u8 {
        return self.sources[e.src].slice()[e.key_off + e.key_len ..][0..e.val_len];
    }

    pub inline fn recordRef(self: *const RecordIndex, i: usize) RecordRef {
        const e = self.entries[i];
        return .{ .key = self.keyAt(e), .seq = e.seq, .kind = e.kind, .value = self.valueAt(e) };
    }

    const KeyCtx = struct { idx: *const RecordIndex, k: []const u8 };

    fn entryKeyLess(ctx: KeyCtx, e: IdxRec) bool {
        return key.order(ctx.idx.keyAt(e), ctx.k) == .lt;
    }

    fn entryKeyLessEq(ctx: KeyCtx, e: IdxRec) bool {
        return key.order(ctx.idx.keyAt(e), ctx.k) != .gt;
    }

    /// `[lo, hi)` of entries whose key equals `k` (newest first).
    pub fn groupBounds(self: *const RecordIndex, k: []const u8) Range {
        const ctx: KeyCtx = .{ .idx = self, .k = k };
        return .{
            .lo = std.sort.partitionPoint(IdxRec, self.entries, ctx, entryKeyLess),
            .hi = std.sort.partitionPoint(IdxRec, self.entries, ctx, entryKeyLessEq),
        };
    }

    /// Index of the first entry with key ≥ `from`.
    pub fn firstAtOrAfter(self: *const RecordIndex, from: []const u8) usize {
        return std.sort.partitionPoint(IdxRec, self.entries, KeyCtx{ .idx = self, .k = from }, entryKeyLess);
    }
};

// ---------------------------------------------------------------------------
// Open-delta records and their materialized heads.
// ---------------------------------------------------------------------------

/// The materialized folded outcome riding the newest overlay record of a
/// key (D7.a): `fold(this record ∪ everything older)`, in the record's
/// reference-counted form (`OwnedRec`). A plain-`Put` head borrows the
/// record's own value by refcount; a delta-folded head owns a fresh buffer
/// (`materialized`), the part the §5.7 budget charges. Inside the run the
/// head is a `HeadRef` into the chunk's arena.
pub const Head = union(enum) {
    live: struct {
        value: Bytes,
        materialized: bool,
    },
    tombstone,
    absent,

    pub fn clone(self: Head) Head {
        return switch (self) {
            .live => |l| .{ .live = .{ .value = l.value.clone(), .materialized = l.materialized } },
            .tombstone => .tombstone,
            .absent => .absent,
        };
    }

    pub fn deinit(self: Head) void {
        switch (self) {
            .live => |l| l.value.deinit(),
            else => {},
        }
    }

    /// Heap bytes this head OWNS beyond its footprint.
    pub fn ownedBytes(self: Head) usize {
        return switch (self) {
            .live => |l| if (l.materialized) l.value.len else 0,
            else => 0,
        };
    }
};

/// Fixed per-head charge beyond the owned value buffer — the §5.7
/// accounting's overhead constant.
pub const FOLD_HEAD_OVERHEAD: usize = @sizeOf(?Head);

/// An open-delta record: the RAM-applied twin of `record.Record` with
/// refcounted payloads so published copies are refcount bumps, never
/// byte copies.
pub const OwnedRec = struct {
    key: Bytes,
    seq: u64,
    kind: RecordKind,
    value: Bytes,
    /// The D7.a head — on the NEWEST overlay record of each key only; null
    /// also means "invalidated — fall back to the from-scratch fold".
    folded: ?Head = null,
    /// The record's own §4.6 pt 2 floor contribution — its containing
    /// journal entry's START seq, stamped by `applyLocked`; carried so an
    /// SMO's leftover transfer gives each successor the EXACT floor of the
    /// records it receives (the former predecessor-floor inheritance
    /// chained an ancient floor through every compaction and pinned the
    /// tail). RAM bookkeeping only. `maxInt` = not yet stamped.
    entry_floor: u64 = std.math.maxInt(u64),

    /// A fresh record entering `applyLocked` (takes both references).
    pub fn init(k: Bytes, seq: u64, kind: RecordKind, value: Bytes) OwnedRec {
        return .{ .key = k, .seq = seq, .kind = kind, .value = value };
    }

    /// Copy a borrowed record in (the staging side's shape).
    pub fn fromRecordRef(gpa: Allocator, r: RecordRef) Allocator.Error!OwnedRec {
        const k = try Bytes.copy(gpa, r.key);
        errdefer k.deinit();
        const v = try Bytes.copy(gpa, r.value);
        return init(k, r.seq, r.kind, v);
    }

    pub fn recordRef(self: *const OwnedRec) RecordRef {
        return .{ .key = self.key.slice(), .seq = self.seq, .kind = self.kind, .value = self.value.slice() };
    }

    /// The §5.7 budget charge of this record's head (0 when none).
    pub fn headBytes(self: *const OwnedRec) usize {
        const h = self.folded orelse return 0;
        return FOLD_HEAD_OVERHEAD + h.ownedBytes();
    }

    pub fn clone(self: *const OwnedRec) OwnedRec {
        return .{
            .key = self.key.clone(),
            .seq = self.seq,
            .kind = self.kind,
            .value = self.value.clone(),
            .folded = if (self.folded) |h| h.clone() else null,
            .entry_floor = self.entry_floor,
        };
    }

    pub fn deinit(self: *OwnedRec) void {
        self.key.deinit();
        self.value.deinit();
        if (self.folded) |h| h.deinit();
        self.* = undefined;
    }

    /// Drop the head (the single-head-per-key and inversion guards).
    pub fn clearHead(self: *OwnedRec) void {
        if (self.folded) |h| h.deinit();
        self.folded = null;
    }

    /// Copy into the K1 owned record (bset build / append input).
    pub fn toRecord(self: *const OwnedRec, gpa: Allocator) Allocator.Error!rec.Record {
        return self.recordRef().toRecord(gpa);
    }
};

/// Run helpers over a `(key asc, seq asc)` overlay run.
const RunCtx = struct { k: []const u8 };

fn runKeyLess(ctx: RunCtx, r: RecView) bool {
    return key.order(r.key(), ctx.k) == .lt;
}

fn runKeyLessEq(ctx: RunCtx, r: RecView) bool {
    return key.order(r.key(), ctx.k) != .gt;
}

/// `[lo, hi)` of `run`'s records with key `k` (seq ascending).
pub fn runGroup(run: *const Run, k: []const u8) Range {
    const ctx: RunCtx = .{ .k = k };
    return .{
        .lo = run.partitionPoint(ctx, runKeyLess),
        .hi = run.partitionPoint(ctx, runKeyLessEq),
    };
}

/// The outcome of folding one key in a snapshot. `live.value` borrows the
/// snapshot (a head serve, a memo hit, a plain `Put`) unless `held` carries
/// the reference a cold delta fold materialized.
pub const Lookup = union(enum) {
    live: struct { value: []const u8, held: ?Bytes },
    /// A tombstone shadows the key — definitively absent in this node.
    tombstone,
    /// No records for the key in this node.
    absent,

    pub fn liveValue(self: Lookup) ?[]const u8 {
        return switch (self) {
            .live => |l| l.value,
            else => null,
        };
    }

    /// Whether the value is a zero-copy borrow of the snapshot.
    pub fn borrowed(self: Lookup) bool {
        return switch (self) {
            .live => |l| l.held == null,
            else => true,
        };
    }

    pub fn deinit(self: *Lookup) void {
        switch (self.*) {
            .live => |l| if (l.held) |h| h.deinit(),
            else => {},
        }
        self.* = .absent;
    }
};

// ---------------------------------------------------------------------------
// The fold memo (D7.b).
// ---------------------------------------------------------------------------

/// Fixed per-node memo capacity: populate-once cells on the immutable
/// snapshot, first-come. 8: the hot-parent case needs 1; small and fixed
/// so the per-node memory bound the budget accounting relies on holds.
pub const FOLD_MEMO_CAPACITY: usize = 8;

const cell_empty: u8 = 0;
const cell_claimed: u8 = 1;
const cell_populated: u8 = 2;

pub const MemoCell = struct {
    state: std.atomic.Value(u8) = .init(cell_empty),
    key: []u8 = &.{},
    /// The folded value (an owned buffer the cell holds a reference to;
    /// the writer-side fold-below borrows it by refcount).
    value: Bytes = undefined,
    /// The newest record seq the fold consumed — vacuously current on an
    /// immutable snapshot; the assert on every hit pins that immutability.
    horizon: u64 = 0,
};

/// Fixed per-cell charge beyond the key and the owned value buffer.
pub const MEMO_CELL_OVERHEAD: usize = @sizeOf(MemoCell);

/// The bounded populate-once memo. Probes are latch-free (an acquire load
/// per cell); populates claim the first empty cell with a CAS and publish
/// with a release store. Race-free by construction: every populate of one
/// key computes the same deterministic fold of the same immutable
/// snapshot, so even a duplicate is benign. Dies with its snapshot —
/// `deinit` subtracts exactly what `populate` charged.
pub const FoldMemo = struct {
    cells: [FOLD_MEMO_CAPACITY]MemoCell = @splat(.{}),
    bytes: std.atomic.Value(u64) = .init(0),
    /// The owning cache's budget gauge (null for a standalone snapshot).
    charge: ?*counters.Gauge,

    pub fn init(charge: ?*counters.Gauge) FoldMemo {
        return .{ .charge = charge };
    }

    /// Scan the (≤ 8) populated cells for `k`: 8 compares on ≤ 36-byte
    /// keys, no decodes, no locks, no allocation.
    pub fn probe(self: *const FoldMemo, k: []const u8) ?*const MemoCell {
        for (&self.cells) |*c| {
            // acquire: pairs with `populate`'s release store of the state.
            if (c.state.load(.acquire) != cell_populated) continue;
            if (std.mem.eql(u8, c.key, k)) return c;
        }
        return null;
    }

    /// Claim the first empty cell for `k` → `value` (the reference moves
    /// into the cell on `true`; on `false` — full, or a racer memoized this
    /// key — the caller keeps it). Only owned delta folds are memoized:
    /// plain `Put`s, tombstones and no-record probes are already free and
    /// would burn the fixed cells for zero win.
    pub fn populate(self: *FoldMemo, gpa: Allocator, k: []const u8, value: Bytes, horizon: u64) Allocator.Error!bool {
        const key_copy = try gpa.dupe(u8, k);
        for (&self.cells) |*c| {
            // acq_rel CAS paired with the release publish below and the
            // probe's acquire load.
            if (c.state.cmpxchgStrong(cell_empty, cell_claimed, .acq_rel, .acquire)) |seen| {
                if (seen == cell_populated and std.mem.eql(u8, c.key, k)) break;
                continue;
            }
            c.key = key_copy;
            c.value = value;
            c.horizon = horizon;
            c.state.store(cell_populated, .release);
            const charge: u64 = k.len + value.len + MEMO_CELL_OVERHEAD;
            _ = self.bytes.fetchAdd(charge, .monotonic);
            if (self.charge) |g| g.add(charge);
            stats.meta_kv_fold_memo_bytes.add(charge);
            return true;
        }
        gpa.free(key_copy);
        return false;
    }

    pub fn populated(self: *const FoldMemo) usize {
        var n: usize = 0;
        for (&self.cells) |*c| n += @intFromBool(c.state.load(.acquire) == cell_populated);
        return n;
    }

    pub fn chargedBytes(self: *const FoldMemo) u64 {
        return self.bytes.load(.monotonic);
    }

    pub fn deinit(self: *FoldMemo, gpa: Allocator) void {
        for (&self.cells) |*c| {
            if (c.state.load(.acquire) != cell_populated) continue;
            gpa.free(c.key);
            c.value.deinit();
        }
        const b = self.bytes.load(.monotonic);
        if (b > 0) {
            if (self.charge) |g| g.sub(b);
            stats.meta_kv_fold_memo_bytes.sub(b);
        }
        self.* = undefined;
    }
};

// ---------------------------------------------------------------------------
// The snapshot.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The overlay run: a persistent chunked `(key asc, seq asc)` sequence.
// ---------------------------------------------------------------------------

/// Records per chunk. 32: the per-apply copy is one chunk — ≤ 33 records of
/// 48 bytes and their arena (≈ 100 bytes a record on the inode and dentry
/// shapes), two memcpys of a few KiB inside one `smp_allocator` size class
/// (a block past its 32 KiB largest class maps pages, and its free is a
/// munmap with a TLB shootdown across the daemon's threads: the former
/// shape's whole-overlay clones) — and the spine of a 4,000-record overlay
/// is ≈ 170 slots.
pub const CHUNK_MAX: usize = 32;

/// The D7.a head of a chunk record. `none`: no head, or invalidated — the
/// read falls back to the from-scratch fold. A live head's bytes live in
/// the chunk's arena: a plain `Put`'s head IS the record's value
/// (`materialized` false, nothing extra charged); a delta-folded head is a
/// range of its own (`materialized` true — the §5.7 charge).
pub const HeadRef = union(enum) {
    none,
    live: struct { off: u32, len: u32, materialized: bool },
    tombstone,
    absent,
};

/// One overlay record as a chunk stores it: the wire fields as ranges of the
/// chunk's arena, the `(key, seq)` order key, the §4.6 pt 2 floor, the head.
/// 48 bytes and no references: a chunk copy is two memcpys, a chunk's death
/// two frees. (The reference-counted `OwnedRec` is the record's form on the
/// way IN — the apply's input — and on the way OUT, `Run.flatten`.)
pub const ChunkRec = struct {
    seq: u64,
    /// The record's own §4.6 pt 2 floor contribution — `OwnedRec.entry_floor`.
    entry_floor: u64,
    key_off: u32,
    val_off: u32,
    val_len: u32,
    key_len: u16,
    kind: RecordKind,
    head: HeadRef,
};

comptime {
    assert(@sizeOf(ChunkRec) == 48);
}

/// The §5.7 budget charge of a record's head (0 when none): the fixed
/// per-head overhead plus the bytes a delta-folded head owns.
pub fn headCharge(r: ChunkRec) usize {
    return switch (r.head) {
        .none => 0,
        .live => |l| FOLD_HEAD_OVERHEAD + @as(usize, if (l.materialized) l.len else 0),
        .tombstone, .absent => FOLD_HEAD_OVERHEAD,
    };
}

/// The head a record enters the run with.
pub const HeadIn = union(enum) {
    none,
    /// A plain `Put`'s head: the record's own value.
    own_value,
    /// A delta-folded value — copied into the arena, charged as materialized.
    folded: []const u8,
    tombstone,
    absent,

    /// `OwnedRec.folded` in this form (a head riding in with an SMO's
    /// leftover record; a non-materialized live head IS the record's own
    /// value — `RecView.toOwned` builds it so).
    pub fn of(h: ?Head) HeadIn {
        const head = h orelse return .none;
        return switch (head) {
            .live => |l| if (l.materialized) .{ .folded = l.value.slice() } else .own_value,
            .tombstone => .tombstone,
            .absent => .absent,
        };
    }

    fn extraBytes(self: HeadIn) usize {
        return switch (self) {
            .folded => |f| f.len,
            else => 0,
        };
    }
};

/// The served form of a head.
pub const HeadView = union(enum) { live: []const u8, tombstone, absent };

/// A borrowed view of one overlay record, valid while its run is held.
pub const RecView = struct {
    chunk: *const Chunk,
    rec: *const ChunkRec,

    pub fn key(self: RecView) []const u8 {
        return self.chunk.bytes.items[self.rec.key_off..][0..self.rec.key_len];
    }

    pub fn value(self: RecView) []const u8 {
        return self.chunk.bytes.items[self.rec.val_off..][0..self.rec.val_len];
    }

    pub fn seq(self: RecView) u64 {
        return self.rec.seq;
    }

    pub fn kind(self: RecView) RecordKind {
        return self.rec.kind;
    }

    pub fn ref(self: RecView) RecordRef {
        return .{ .key = self.key(), .seq = self.rec.seq, .kind = self.rec.kind, .value = self.value() };
    }

    /// The head, null when the record carries none.
    pub fn head(self: RecView) ?HeadView {
        return switch (self.rec.head) {
            .none => null,
            .live => |l| .{ .live = self.chunk.bytes.items[l.off..][0..l.len] },
            .tombstone => .tombstone,
            .absent => .absent,
        };
    }

    /// The record in its reference-counted form (fresh copies of its bytes).
    fn toOwned(self: RecView, gpa: Allocator) Allocator.Error!OwnedRec {
        const k = try Bytes.copy(gpa, self.key());
        errdefer k.deinit();
        const v = try Bytes.copy(gpa, self.value());
        errdefer v.deinit();
        var out = OwnedRec.init(k, self.rec.seq, self.rec.kind, v);
        out.entry_floor = self.rec.entry_floor;
        out.folded = switch (self.rec.head) {
            .none => null,
            .live => |l| if (l.materialized)
                .{ .live = .{ .value = try Bytes.copy(gpa, self.chunk.bytes.items[l.off..][0..l.len]), .materialized = true } }
            else
                .{ .live = .{ .value = v.clone(), .materialized = false } },
            .tombstone => .tombstone,
            .absent => .absent,
        };
        return out;
    }
};

/// A non-empty `(key asc, seq asc)` piece of the run: its records and the
/// arena their bytes live in. Immutable once a run holding it is published;
/// shared between runs by refcount; grown in place only by the one edit that
/// owns it alone. A copy compacts: the arena bytes of cleared heads and of
/// records a split moved out stay until then.
pub const Chunk = struct {
    recs: std.ArrayListUnmanaged(ChunkRec),
    bytes: std.ArrayListUnmanaged(u8),

    pub const empty: Chunk = .{ .recs = .empty, .bytes = .empty };

    pub fn deinit(self: *Chunk, gpa: Allocator) void {
        self.recs.deinit(gpa);
        self.bytes.deinit(gpa);
        self.* = undefined;
    }

    pub fn len(self: *const Chunk) usize {
        return self.recs.items.len;
    }

    pub fn view(self: *const Chunk, i: usize) RecView {
        return .{ .chunk = self, .rec = &self.recs.items[i] };
    }

    fn recBytes(r: ChunkRec) usize {
        const head: usize = switch (r.head) {
            .live => |l| if (l.materialized) l.len else 0,
            else => 0,
        };
        return @as(usize, r.key_len) + r.val_len + head;
    }

    /// Append `r`'s bytes and head to the arena (reserved by the caller);
    /// the record with the new offsets.
    fn placeAssumeCapacity(self: *Chunk, r: RecordRef, entry_floor: u64, head: HeadIn) ChunkRec {
        const key_off: u32 = @intCast(self.bytes.items.len);
        self.bytes.appendSliceAssumeCapacity(r.key);
        const val_off: u32 = @intCast(self.bytes.items.len);
        self.bytes.appendSliceAssumeCapacity(r.value);
        const h: HeadRef = switch (head) {
            .none => .none,
            .own_value => .{ .live = .{ .off = val_off, .len = @intCast(r.value.len), .materialized = false } },
            .folded => |f| blk: {
                const off: u32 = @intCast(self.bytes.items.len);
                self.bytes.appendSliceAssumeCapacity(f);
                break :blk .{ .live = .{ .off = off, .len = @intCast(f.len), .materialized = true } };
            },
            .tombstone => .tombstone,
            .absent => .absent,
        };
        return .{
            .seq = r.seq,
            .entry_floor = entry_floor,
            .key_off = key_off,
            .val_off = val_off,
            .val_len = @intCast(r.value.len),
            .key_len = @intCast(r.key.len),
            .kind = r.kind,
            .head = h,
        };
    }

    /// Insert `r` at `at` (the chunk is the caller's alone). Nothing changes
    /// when the reservation fails.
    fn insertRec(self: *Chunk, gpa: Allocator, at: usize, r: RecordRef, entry_floor: u64, head: HeadIn) Allocator.Error!void {
        try self.bytes.ensureUnusedCapacity(gpa, r.key.len + r.value.len + head.extraBytes());
        try self.recs.ensureUnusedCapacity(gpa, 1);
        const c = self.placeAssumeCapacity(r, entry_floor, head);
        self.recs.insertAssumeCapacity(at, c);
    }

    /// A compacted copy of records `[from, to)` with room for `extra_recs`
    /// more records and `extra_bytes` more arena bytes.
    fn copyRange(self: *const Chunk, gpa: Allocator, from: usize, to: usize, extra_recs: usize, extra_bytes: usize) Allocator.Error!Chunk {
        var out: Chunk = .empty;
        errdefer out.deinit(gpa);
        var live: usize = 0;
        for (self.recs.items[from..to]) |r| live += recBytes(r);
        try out.recs.ensureTotalCapacityPrecise(gpa, to - from + extra_recs);
        try out.bytes.ensureTotalCapacityPrecise(gpa, live + extra_bytes);
        for (self.recs.items[from..to]) |*r| {
            const rv: RecView = .{ .chunk = self, .rec = r };
            const head: HeadIn = switch (r.head) {
                .none => .none,
                .live => |l| if (l.materialized) .{ .folded = self.bytes.items[l.off..][0..l.len] } else .own_value,
                .tombstone => .tombstone,
                .absent => .absent,
            };
            out.recs.appendAssumeCapacity(out.placeAssumeCapacity(rv.ref(), r.entry_floor, head));
        }
        return out;
    }
};

/// One spine entry: a chunk and the run index of its first record.
pub const Slot = struct { chunk: *Arc(Chunk), start: u32 };

/// The open-delta overlay as readers and the dirty half share it: a spine of
/// refcounted chunks, each a non-empty `(key asc, seq asc)` piece of about
/// `CHUNK_MAX` records, immutable once published. The dirty half derives the
/// next run through `RunEdit`, which copies the chunks the records land in
/// and the spine and shares every other chunk — so an apply is O(records
/// applied), a publish one reference, and the run a reader holds is exact
/// whatever applied since (the former stable/tail split re-cloned the whole
/// overlay every 8 applies and cloned a tail per publish).
///
/// Searches are two-level — the spine by each chunk's last record, then the
/// chunk — so a lookup costs the binary search a flat array would.
pub const Run = struct {
    slots: []Slot,
    len: usize,

    pub const empty: Run = .{ .slots = &.{}, .len = 0 };

    pub fn deinit(self: *Run, gpa: Allocator) void {
        for (self.slots) |s| s.chunk.release();
        if (self.slots.len != 0) gpa.free(self.slots);
        self.* = undefined;
    }

    /// The bytes one apply of one record of `rec_bytes` wire bytes
    /// allocates for the run, at most: the chunk it lands in copied (≤
    /// `CHUNK_MAX + 1` records, their arena; a split's half or an owned
    /// chunk's in-place growth at most as much again), their boxes, the
    /// spine (chunks are at least half full after their split — the first
    /// grows from 1) and the run's box. Independent of the overlay length
    /// but for the spine's one slot per chunk.
    pub fn applyBytesBound(n: usize, rec_bytes: usize) usize {
        const chunk = (CHUNK_MAX + 1) * (@sizeOf(ChunkRec) + rec_bytes);
        const spine = (n / (CHUNK_MAX / 2) + 2) * @sizeOf(Slot);
        return 2 * chunk + 2 * @sizeOf(Arc(Chunk)) + spine + @sizeOf(Arc(Run));
    }

    /// A cursor: `ci == slots.len` is the end.
    pub const Pos = struct { ci: usize, off: usize };

    fn slotStartLessEq(i: usize, s: Slot) bool {
        return s.start <= i;
    }

    /// The cursor of run index `i` (`len` is the end).
    pub fn posOf(self: *const Run, i: usize) Pos {
        assert(i <= self.len);
        if (i == self.len) return .{ .ci = self.slots.len, .off = 0 };
        const ci = std.sort.partitionPoint(Slot, self.slots, i, slotStartLessEq) - 1;
        return .{ .ci = ci, .off = i - self.slots[ci].start };
    }

    pub fn index(self: *const Run, p: Pos) usize {
        if (p.ci == self.slots.len) return self.len;
        return self.slots[p.ci].start + p.off;
    }

    pub fn rec(self: *const Run, p: Pos) RecView {
        return self.slots[p.ci].chunk.value.view(p.off);
    }

    /// The record at run index `i` (a spine search; walks use `Pos`).
    pub fn at(self: *const Run, i: usize) RecView {
        return self.rec(self.posOf(i));
    }

    pub fn next(self: *const Run, p: Pos) Pos {
        assert(p.ci < self.slots.len);
        if (p.off + 1 < self.slots[p.ci].chunk.value.len()) return .{ .ci = p.ci, .off = p.off + 1 };
        return .{ .ci = p.ci + 1, .off = 0 };
    }

    pub fn prev(self: *const Run, p: Pos) Pos {
        if (p.off > 0) return .{ .ci = p.ci, .off = p.off - 1 };
        assert(p.ci > 0);
        return .{ .ci = p.ci - 1, .off = self.slots[p.ci - 1].chunk.value.len() - 1 };
    }

    /// The first run index whose record fails `pred` (true on a prefix of
    /// the run): the spine by each chunk's LAST record — a chunk whose
    /// last satisfies the predicate lies wholly in the prefix — then that
    /// chunk's records.
    pub fn partitionPoint(self: *const Run, context: anytype, comptime pred: fn (@TypeOf(context), RecView) bool) usize {
        var lo: usize = 0;
        var hi: usize = self.slots.len;
        while (lo < hi) {
            const mid = lo + (hi - lo) / 2;
            const c = &self.slots[mid].chunk.value;
            if (pred(context, c.view(c.len() - 1))) lo = mid + 1 else hi = mid;
        }
        if (lo == self.slots.len) return self.len;
        const s = self.slots[lo];
        const c = &s.chunk.value;
        var a: usize = 0;
        var b: usize = c.len();
        while (a < b) {
            const mid = a + (b - a) / 2;
            if (pred(context, c.view(mid))) a = mid + 1 else b = mid;
        }
        return s.start + a;
    }

    /// Every record in its reference-counted form, one owned array (the
    /// SMO's take-over and the rollback's rebuild); the run is untouched.
    pub fn flatten(self: *const Run, gpa: Allocator) Allocator.Error![]OwnedRec {
        const out = try gpa.alloc(OwnedRec, self.len);
        var n: usize = 0;
        errdefer {
            for (out[0..n]) |*r| r.deinit();
            gpa.free(out);
        }
        for (self.slots) |s| {
            const c = &s.chunk.value;
            for (0..c.len()) |i| {
                out[n] = try c.view(i).toOwned(gpa);
                n += 1;
            }
        }
        assert(n == self.len);
        return out;
    }

    /// A run over `recs`, already `(key asc, seq asc)`, in chunks of
    /// `CHUNK_MAX` (their bytes copied; the slice stays the caller's, its
    /// elements are consumed on every outcome).
    pub fn fromSorted(gpa: Allocator, recs: []OwnedRec) Allocator.Error!Run {
        defer for (recs) |*r| r.deinit();
        const n_chunks = (recs.len + CHUNK_MAX - 1) / CHUNK_MAX;
        if (n_chunks == 0) return .empty;
        var slots: std.ArrayListUnmanaged(Slot) = .empty;
        errdefer {
            for (slots.items) |s| s.chunk.release();
            slots.deinit(gpa);
        }
        try slots.ensureTotalCapacityPrecise(gpa, n_chunks);
        var done: usize = 0;
        while (done < recs.len) {
            const take = @min(CHUNK_MAX, recs.len - done);
            var chunk: Chunk = .empty;
            errdefer chunk.deinit(gpa);
            var bytes: usize = 0;
            for (recs[done..][0..take]) |*r| bytes += r.key.len + r.value.len + HeadIn.of(r.folded).extraBytes();
            try chunk.recs.ensureTotalCapacityPrecise(gpa, take);
            try chunk.bytes.ensureTotalCapacityPrecise(gpa, bytes);
            for (recs[done..][0..take]) |*r| chunk.recs.appendAssumeCapacity(chunk.placeAssumeCapacity(r.recordRef(), r.entry_floor, HeadIn.of(r.folded)));
            const c = try Arc(Chunk).create(gpa, chunk);
            chunk = .empty;
            slots.appendAssumeCapacity(.{ .chunk = c, .start = @intCast(done) });
            done += take;
        }
        return .{ .slots = try slots.toOwnedSlice(gpa), .len = recs.len };
    }
};

/// The dirty half's working copy of a run: the chunks it touched are copies
/// it owns alone (`strongCount() == 1` — the base run, which must outlive
/// the edit, keeps every shared chunk at ≥ 2), the rest are the base's.
/// `finish` yields the immutable successor; `abandon` drops the copies and
/// leaves the base as it was.
pub const RunEdit = struct {
    slots: std.ArrayListUnmanaged(Slot),
    len: usize,

    pub fn begin(gpa: Allocator, base: *const Run) Allocator.Error!RunEdit {
        var slots: std.ArrayListUnmanaged(Slot) = .empty;
        // Room for one split without a second spine allocation.
        try slots.ensureTotalCapacityPrecise(gpa, base.slots.len + 1);
        for (base.slots) |s| slots.appendAssumeCapacity(.{ .chunk = s.chunk.retain(), .start = s.start });
        return .{ .slots = slots, .len = base.len };
    }

    pub fn abandon(self: *RunEdit, gpa: Allocator) void {
        for (self.slots.items) |s| s.chunk.release();
        self.slots.deinit(gpa);
        self.* = undefined;
    }

    pub fn finish(self: *RunEdit, gpa: Allocator) Allocator.Error!Run {
        const len = self.len;
        const slots = try self.slots.toOwnedSlice(gpa);
        self.* = undefined;
        return .{ .slots = slots, .len = len };
    }

    /// A read view (searches and walks) of the edit as it stands.
    pub fn view(self: *const RunEdit) Run {
        return .{ .slots = self.slots.items, .len = self.len };
    }

    fn owned(s: Slot) bool {
        return s.chunk.strongCount() == 1;
    }

    /// Replace the chunk at `ci` with a compacted copy this edit owns alone,
    /// with room for `extra_recs` records and `extra_bytes` arena bytes.
    fn copyChunk(self: *RunEdit, gpa: Allocator, ci: usize, extra_recs: usize, extra_bytes: usize) Allocator.Error!void {
        const s = &self.slots.items[ci];
        const old = &s.chunk.value;
        var fresh = try old.copyRange(gpa, 0, old.len(), extra_recs, extra_bytes);
        const c = Arc(Chunk).create(gpa, fresh) catch |e| {
            fresh.deinit(gpa);
            return e;
        };
        s.chunk.release();
        s.chunk = c;
    }

    /// Drop the head of the record at run index `i` (in a chunk this edit
    /// owns alone — copied first when shared); the charge it carried.
    pub fn clearHead(self: *RunEdit, gpa: Allocator, i: usize) Allocator.Error!usize {
        const p = self.view().posOf(i);
        if (!owned(self.slots.items[p.ci])) try self.copyChunk(gpa, p.ci, 0, 0);
        const r = &self.slots.items[p.ci].chunk.value.recs.items[p.off];
        const freed = headCharge(r.*);
        r.head = .none;
        return freed;
    }

    /// Insert `r` with `head` at run index `pos` (the caller's partition
    /// point; the bytes are copied). The chunk holding `pos` takes it — at
    /// a chunk boundary the chunk ENDING there, so a record and its same-key
    /// predecessor share the one copy; a chunk past `CHUNK_MAX` splits in
    /// half. Returns the head's charge.
    pub fn insert(self: *RunEdit, gpa: Allocator, pos: usize, r: RecordRef, entry_floor: u64, head: HeadIn) Allocator.Error!usize {
        assert(pos <= self.len);
        const need = r.key.len + r.value.len + head.extraBytes();
        if (self.slots.items.len == 0) {
            var chunk: Chunk = .empty;
            errdefer chunk.deinit(gpa);
            try chunk.insertRec(gpa, 0, r, entry_floor, head);
            const c = try Arc(Chunk).create(gpa, chunk);
            self.slots.appendAssumeCapacity(.{ .chunk = c, .start = 0 });
            self.len = 1;
            return headCharge(c.value.recs.items[0]);
        }
        var ci: usize = undefined;
        if (pos == self.len) {
            ci = self.slots.items.len - 1;
        } else {
            const p = self.view().posOf(pos);
            ci = if (p.off == 0 and p.ci > 0) p.ci - 1 else p.ci;
        }
        if (!owned(self.slots.items[ci])) try self.copyChunk(gpa, ci, 1, need);
        const s = &self.slots.items[ci];
        const off = pos - s.start;
        try s.chunk.value.insertRec(gpa, off, r, entry_floor, head);
        const charge = headCharge(s.chunk.value.recs.items[off]);
        self.len += 1;
        for (self.slots.items[ci + 1 ..]) |*t| t.start += 1;
        // An over-full chunk splits; failing to (no memory) leaves a longer
        // chunk, never a wrong run — the next insert into it tries again.
        if (s.chunk.value.len() > CHUNK_MAX) self.split(gpa, ci) catch {};
        return charge;
    }

    /// Split the chunk at `ci` (owned alone) in half: the upper half is
    /// copied into a fresh chunk, the lower keeps its arena (compacted at
    /// its next copy). Every allocation precedes the first mutation, so a
    /// failure leaves the chunk whole.
    fn split(self: *RunEdit, gpa: Allocator, ci: usize) Allocator.Error!void {
        assert(owned(self.slots.items[ci]));
        const whole = &self.slots.items[ci].chunk.value;
        const half = whole.len() / 2;
        var upper = try whole.copyRange(gpa, half, whole.len(), 0, 0);
        const right = Arc(Chunk).create(gpa, upper) catch |e| {
            upper.deinit(gpa);
            return e;
        };
        errdefer right.release();
        try self.slots.insert(gpa, ci + 1, .{ .chunk = right, .start = @intCast(self.slots.items[ci].start + half) });
        self.slots.items[ci].chunk.value.recs.shrinkRetainingCapacity(half);
    }
};

/// `(key, kind, encoded_len)` of a record about to be applied — the
/// estimator's `extra`.
pub const Extra = struct { key: []const u8, kind: RecordKind, len: usize };

/// The estimator's answer: fold bytes and the greedy part count.
pub const Projection = struct { bytes: usize, parts: usize };

/// The durable delta-chain probe's answer.
pub const ChainProbe = struct { depth: u32, head: ?rec.LayoutVersions };

/// A live `(key, value)` from a range walk; `held` as in `Lookup`.
pub const LiveEntry = struct {
    key: []const u8,
    value: []const u8,
    held: ?Bytes,

    pub fn deinit(self: *LiveEntry) void {
        if (self.held) |h| h.deinit();
        self.* = undefined;
    }
};

pub const NodeSnapshot = struct {
    base: *Arc(RecordIndex),
    /// The open-delta overlay as of this publish.
    overlay: *Arc(Run),
    memo: FoldMemo,

    /// A snapshot of `overlay` over `base` (takes both references).
    pub fn init(base: *Arc(RecordIndex), overlay: *Arc(Run), charge: ?*counters.Gauge) NodeSnapshot {
        return .{ .base = base, .overlay = overlay, .memo = .init(charge) };
    }

    pub fn deinit(self: *NodeSnapshot, gpa: Allocator) void {
        self.memo.deinit(gpa);
        self.overlay.release();
        self.base.release();
        self.* = undefined;
    }

    /// Records in the open-delta overlay.
    pub fn overlayLen(self: *const NodeSnapshot) usize {
        return self.overlay.value.len;
    }

    /// Total indexed records across serialized sources (not folded).
    pub fn indexedLen(self: *const NodeSnapshot) usize {
        return self.base.value.entries.len;
    }

    /// The dead-record census of the SERIALIZED sources: `(total indexed,
    /// distinct keys)` — `total − distinct` is the superseded population a
    /// compaction reclaims.
    pub fn indexedRecordCensus(self: *const NodeSnapshot) struct { total: u64, distinct: u64 } {
        const idx = &self.base.value;
        var distinct: u64 = 0;
        var prev: ?[]const u8 = null;
        for (idx.entries) |e| {
            const k = idx.keyAt(e);
            if (prev == null or !std.mem.eql(u8, prev.?, k)) {
                distinct += 1;
                prev = k;
            }
        }
        return .{ .total = idx.entries.len, .distinct = distinct };
    }

    const Groups = struct { og: Range, bg: Range };

    fn groups(self: *const NodeSnapshot, k: []const u8) Groups {
        return .{
            .og = runGroup(&self.overlay.value, k),
            .bg = self.base.value.groupBounds(k),
        };
    }

    /// The newest record seq this node holds for `k` across every source,
    /// or null when the key is unknown here. Order-independent (max per
    /// group), so safe mid-replay — replay's per-key LWW gate reads it.
    pub fn newestSeqOf(self: *const NodeSnapshot, k: []const u8) ?u64 {
        const g = self.groups(k);
        var newest: ?u64 = null;
        // The run is (key, seq) ascending: a group's last is its newest.
        if (g.og.hi > g.og.lo) newest = self.overlay.value.at(g.og.hi - 1).seq();
        // Base groups are (seq desc): the group's first is its newest.
        if (g.bg.hi > g.bg.lo) {
            const b = self.base.value.entries[g.bg.lo].seq;
            newest = if (newest) |n| @max(n, b) else b;
        }
        return newest;
    }

    /// The newest overlay record of `k`, if any.
    fn newestOverlay(self: *const NodeSnapshot, g: Groups) ?RecView {
        if (g.og.hi > g.og.lo) return self.overlay.value.at(g.og.hi - 1);
        return null;
    }

    /// Whether the base holds a record of the key NEWER than `seq` — the
    /// §AW.4 inversion: an earlier window's record applied after a later
    /// window's was frozen. Base groups are seq-descending: one compare.
    fn baseAbove(self: *const NodeSnapshot, g: Groups, seq: u64) bool {
        return g.bg.hi > g.bg.lo and self.base.value.entries[g.bg.lo].seq > seq;
    }

    /// The newest record of `k` across both sources (null: unknown here).
    fn newestRef(self: *const NodeSnapshot, g: Groups) ?RecordRef {
        if (self.newestOverlay(g)) |n| {
            if (!self.baseAbove(g, n.seq())) return n.ref();
        }
        if (g.bg.hi > g.bg.lo) return self.base.value.recordRef(g.bg.lo);
        return null;
    }

    /// Walk one key's records of both sources newest-first — the overlay
    /// group from its end, the base group from its start, merged by seq
    /// (an exact tie: the overlay first, as before the merge). `visit`
    /// returns false to stop.
    fn walkNewestFirst(self: *const NodeSnapshot, g: Groups, ctx: anytype, comptime visit: fn (@TypeOf(ctx), RecordRef) bool) void {
        const run = &self.overlay.value;
        var o = g.og.hi;
        var p = run.posOf(g.og.hi);
        var b = g.bg.lo;
        while (o > g.og.lo or b < g.bg.hi) {
            const take_overlay = if (o == g.og.lo) false else if (b == g.bg.hi) true else run.rec(run.prev(p)).seq() >= self.base.value.entries[b].seq;
            const r: RecordRef = if (take_overlay) blk: {
                o -= 1;
                p = run.prev(p);
                break :blk run.rec(p).ref();
            } else blk: {
                defer b += 1;
                break :blk self.base.value.recordRef(b);
            };
            if (!visit(ctx, r)) return;
        }
    }

    /// Scratch for one key's gather: the overlay group and the base group
    /// merged newest first. Inline up to 64 records; longer chains (cold by
    /// definition — the memo serves the repeat) spill to the heap.
    const Gather = struct {
        inline_buf: [64]RecordRef = undefined,
        heap: ?[]RecordRef = null,
        len: usize = 0,

        fn slice(self: *Gather) []RecordRef {
            if (self.heap) |h| return h[0..self.len];
            return self.inline_buf[0..self.len];
        }

        fn deinit(self: *Gather, gpa: Allocator) void {
            if (self.heap) |h| gpa.free(h);
        }

        fn push(self: *Gather, r: RecordRef) bool {
            const dst = if (self.heap) |h| h else &self.inline_buf;
            dst[self.len] = r;
            self.len += 1;
            return true;
        }
    };

    fn gather(self: *const NodeSnapshot, gpa: Allocator, g: Groups, out: *Gather) Allocator.Error!void {
        const n = (g.og.hi - g.og.lo) + (g.bg.hi - g.bg.lo);
        if (n > out.inline_buf.len) out.heap = try gpa.alloc(RecordRef, n);
        self.walkNewestFirst(g, out, Gather.push);
        assert(out.len == n);
    }

    /// Fold one key — the D7-slimmed read path:
    /// 1. overlay head serve (zero decodes), else
    /// 2. memo serve for a bset-resident key (latch-free, zero decodes), else
    /// 3. the from-scratch fold; a delta-materialized outcome populates a
    ///    memo cell for the next read.
    /// `gpa` is touched only by step 3.
    pub fn lookup(self: *NodeSnapshot, gpa: Allocator, k: []const u8) FoldError!Lookup {
        const g = self.groups(k);
        const newest = self.newestOverlay(g);
        if (newest) |n| {
            // The head is the fold up to n's seq: it serves only while the
            // base holds nothing newer for the key (§AW.4: an earlier
            // window's record landing after a later window's was frozen
            // — the base above the overlay — folds cold, by seq).
            if (self.baseAbove(g, n.seq())) {
                stats.meta_kv_fold_base_above.inc();
            } else if (n.head()) |h| {
                stats.meta_kv_fold_head_serves.inc();
                return switch (h) {
                    .live => |v| .{ .live = .{ .value = v, .held = null } },
                    .tombstone => .tombstone,
                    .absent => .absent,
                };
            }
        } else if (self.memo.probe(k)) |cell| {
            stats.meta_kv_fold_memo_hits.inc();
            // Immutability pin: the memoized horizon is still this
            // snapshot's newest seq for the key.
            assert(self.newestSeqOf(k).? == cell.horizon);
            return .{ .live = .{ .value = cell.value.slice(), .held = null } };
        }
        // The newest record (by seq, across both sources) decides alone
        // unless it is a Delta (the fold stops at the first Put or Delete —
        // `rec.foldNewestFirst`): the common base-resident key folds with
        // no gather scratch, whose 64 record refs ReleaseSafe fills on
        // every from-scratch fold (13 % of the FUSE worker's cycles on
        // rand-4k, `docs/evidence/rung-10-dev-perf.md` §3.4).
        const top = self.newestRef(g) orelse return .absent;
        switch (top.kind) {
            .put => return .{ .live = .{ .value = top.value, .held = null } },
            .delete => return .tombstone,
            .delta => {},
        }
        var scratch: Gather = .{};
        defer scratch.deinit(gpa);
        try self.gather(gpa, g, &scratch);
        const folded = try rec.foldNewestFirst(gpa, scratch.slice());
        switch (folded) {
            .absent => return .absent,
            .tombstone => return .tombstone,
            .put => |p| {
                if (!p.owned) return .{ .live = .{ .value = p.value, .held = null } };
                const owned: []u8 = @constCast(p.value);
                const held = Bytes.fromOwned(gpa, owned) catch |e| {
                    gpa.free(owned);
                    return e;
                };
                if (newest == null) {
                    // A miss is counted exactly where a populate follows.
                    stats.meta_kv_fold_memo_misses.inc();
                    const horizon = self.base.value.entries[g.bg.lo].seq;
                    const cell_ref = held.clone();
                    if (!(self.memo.populate(gpa, k, cell_ref, horizon) catch |e| {
                        cell_ref.deinit();
                        held.deinit();
                        return e;
                    })) cell_ref.deinit();
                }
                return .{ .live = .{ .value = held.slice(), .held = held } };
            },
        }
    }

    /// The key's durable delta-chain probe: how many `Delta` records stack
    /// above the newest base in THIS snapshot, plus the newest link's
    /// `(base_version, version)` pair (null when the chain is empty or its
    /// head is unversioned). Counting only — no record decode.
    pub fn deltaChainProbe(self: *const NodeSnapshot, k: []const u8) ChainProbe {
        var probe: ChainProbe = .{ .depth = 0, .head = null };
        self.walkNewestFirst(self.groups(k), &probe, chainStep);
        return probe;
    }

    fn chainStep(probe: *ChainProbe, r: RecordRef) bool {
        if (r.kind != .delta) return false;
        if (probe.depth == 0) probe.head = rec.layoutDeltaVersions(r.value);
        probe.depth += 1;
        return true;
    }

    /// The next LIVE `(key, value)` with key ≥ `from` (fold-walked:
    /// tombstoned/orphaned keys are skipped) — the range-scan / interior
    /// routing primitive. `end_inclusive` bounds the walk BEFORE any fold:
    /// without it a narrow window scan would fold-walk the whole tombstone
    /// desert a create/unlink storm leaves until compaction (the ~1000×
    /// post-storm lookup cliff). Null = unbounded (interior routing wants
    /// the next live separator wherever it is).
    pub fn nextLive(self: *NodeSnapshot, gpa: Allocator, from: []const u8, end_inclusive: ?[]const u8) FoldError!?LiveEntry {
        const idx = &self.base.value;
        const run = &self.overlay.value;
        var bi = idx.firstAtOrAfter(from);
        var oi = run.partitionPoint(RunCtx{ .k = from }, runKeyLess);
        var op = run.posOf(oi);
        while (true) {
            var k: ?[]const u8 = if (bi < idx.entries.len) idx.keyAt(idx.entries[bi]) else null;
            if (oi < run.len) k = minKey(k, run.rec(op).key());
            const cand = k orelse return null;
            if (end_inclusive) |end| if (key.order(cand, end) == .gt) return null;
            var look = try self.lookup(gpa, cand);
            switch (look) {
                .live => |l| return .{ .key = cand, .value = l.value, .held = l.held },
                else => {},
            }
            look.deinit();
            // Advance every source past this key.
            while (bi < idx.entries.len and std.mem.eql(u8, idx.keyAt(idx.entries[bi]), cand)) bi += 1;
            while (oi < run.len and std.mem.eql(u8, run.rec(op).key(), cand)) {
                oi += 1;
                op = run.next(op);
            }
        }
    }

    /// The previous LIVE `(key, value)` with key strictly below `before` —
    /// `nextLive`'s mirror, the §4.6a left-sibling probe.
    pub fn prevLive(self: *NodeSnapshot, gpa: Allocator, before: []const u8) FoldError!?LiveEntry {
        const idx = &self.base.value;
        const run = &self.overlay.value;
        var bi = idx.firstAtOrAfter(before);
        var oi = run.partitionPoint(RunCtx{ .k = before }, runKeyLess);
        // `op` sits ON index `oi`; the record below is `prev(op)`.
        var op = run.posOf(oi);
        while (true) {
            var k: ?[]const u8 = if (bi > 0) idx.keyAt(idx.entries[bi - 1]) else null;
            if (oi > 0) k = maxKey(k, run.rec(run.prev(op)).key());
            const cand = k orelse return null;
            var look = try self.lookup(gpa, cand);
            switch (look) {
                .live => |l| return .{ .key = cand, .value = l.value, .held = l.held },
                else => {},
            }
            look.deinit();
            while (bi > 0 and std.mem.eql(u8, idx.keyAt(idx.entries[bi - 1]), cand)) bi -= 1;
            while (oi > 0 and std.mem.eql(u8, run.rec(run.prev(op)).key(), cand)) {
                oi -= 1;
                op = run.prev(op);
            }
        }
    }

    fn minKey(a: ?[]const u8, b: []const u8) []const u8 {
        const x = a orelse return b;
        return if (key.order(b, x) == .lt) b else x;
    }

    fn maxKey(a: ?[]const u8, b: []const u8) []const u8 {
        const x = a orelse return b;
        return if (key.order(b, x) == .gt) b else x;
    }

    fn establishes(k: RecordKind) bool {
        return k == .put or k == .delete;
    }

    fn extraKeyLess(_: void, a: Extra, b: Extra) bool {
        return key.order(a.key, b.key) == .lt;
    }

    /// An UPPER bound on the encoded bytes a compaction fold of this
    /// snapshot PLUS `extra` (records about to be applied, in apply order
    /// — newer than everything here) produces: the heap admission's
    /// compaction-vs-split projection and the §4.6a merge measure. Per
    /// distinct key, the records at-or-above the newest base-establishing
    /// record (`Put`/`Delete`) — everything below is shadowed, everything
    /// above folds INTO the output (a `Delta` chain grows its base by at
    /// most its own encoding). A pending `Delete`/`Put` shadows the key's
    /// resident records. A resident tombstone counts its own encoding
    /// unless `seq < durable_tail` — the elision rule `compactFold`
    /// applies (pass `0` to never elide). `extra` is sorted here (stable,
    /// so same-key extras keep apply order).
    ///
    /// Returns the fold bytes and the greedy parts a split packs at
    /// `part_budget` bytes each (a part closes when the next key group
    /// does not fit a non-empty part; key groups stay whole). The exact
    /// fold is `node.foldBytes`.
    pub fn foldBytesUpperWith(self: *const NodeSnapshot, extra: []Extra, part_budget: usize, durable_tail: u64) Projection {
        std.sort.block(Extra, extra, {}, extraKeyLess);
        const idx = &self.base.value;
        const run = &self.overlay.value;
        var b: usize = 0;
        var o: usize = 0;
        var op = run.posOf(0);
        var x: usize = 0;
        var total: usize = 0;
        var parts: usize = 1;
        var acc: usize = 0;
        while (true) {
            var k: ?[]const u8 = if (b < idx.entries.len) idx.keyAt(idx.entries[b]) else null;
            if (o < run.len) k = minKey(k, run.rec(op).key());
            if (x < extra.len) k = minKey(k, extra[x].key);
            const cur = k orelse break;
            var be: usize = 0;
            while (b + be < idx.entries.len and std.mem.eql(u8, idx.keyAt(idx.entries[b + be]), cur)) be += 1;
            // The overlay's group: `[op, oq)`, `oe` records.
            var oe: usize = 0;
            var oq = op;
            while (o + oe < run.len and std.mem.eql(u8, run.rec(oq).key(), cur)) {
                oe += 1;
                oq = run.next(oq);
            }
            var xe: usize = 0;
            while (x + xe < extra.len and std.mem.eql(u8, extra[x + xe].key, cur)) xe += 1;

            // The pending records are newer than every resident one: the
            // newest pending base-establisher shadows the whole resident
            // group; otherwise the resident group folds under its own
            // newest establisher and every pending Delta rides on top.
            var pending_base: ?usize = null;
            var p = xe;
            while (p > 0) {
                p -= 1;
                if (establishes(extra[x + p].kind)) {
                    pending_base = p;
                    break;
                }
            }
            var group: usize = 0;
            if (pending_base) |pb| {
                for (extra[x + pb .. x + xe]) |e| group += e.len;
            } else {
                var base_seq: u64 = 0;
                var base_kind: RecordKind = .put;
                for (idx.entries[b .. b + be]) |e| {
                    if (establishes(e.kind) and e.seq >= base_seq) {
                        base_seq = e.seq;
                        base_kind = e.kind;
                    }
                }
                var q = op;
                for (0..oe) |_| {
                    const r = run.rec(q);
                    if (establishes(r.kind()) and r.seq() >= base_seq) {
                        base_seq = r.seq();
                        base_kind = r.kind();
                    }
                    q = run.next(q);
                }
                if (base_kind == .delete and base_seq < durable_tail and xe == 0) {
                    // A covered tombstone and everything it shadows leave
                    // the compaction fold whole (orphan deltas above it
                    // fold to absent). Seq vs position, conservatively: a
                    // stamp is ≥ its position, so `seq < tail` implies
                    // covered.
                    group = 0;
                } else {
                    for (b..b + be) |i| if (idx.entries[i].seq >= base_seq) {
                        group += idx.recordRef(i).encodedLen();
                    };
                    q = op;
                    for (0..oe) |_| {
                        const r = run.rec(q);
                        if (r.seq() >= base_seq) group += r.ref().encodedLen();
                        q = run.next(q);
                    }
                    for (extra[x .. x + xe]) |e| group += e.len;
                }
            }
            total += group;
            // The SMO's greedy packer, over the same key order.
            if (acc + group > part_budget and acc > 0) {
                parts += 1;
                acc = 0;
            }
            acc += group;
            b += be;
            o += oe;
            op = oq;
            x += xe;
        }
        return .{ .bytes = total, .parts = parts };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const InodeValue = rec.InodeValue;
const InodeDelta = rec.InodeDelta;
const Record = rec.Record;

fn iv(seed: u64) InodeValue {
    return .{ .mode = 0o100644, .uid = @truncate(seed), .nlink = 1, .size = seed *% 4096, .atime = seed +% 1, .mtime = seed +% 2, .ctime = seed +% 3 };
}

fn put(gpa: Allocator, ino: u64, seq: u64) !Record {
    return Record.put(gpa, &key.inodeKey(ino), seq, &iv(seq).encode());
}

fn dtimes(gpa: Allocator, ino: u64, seq: u64, m: u64, c: u64) !Record {
    return Record.delta(gpa, &key.inodeKey(ino), seq, InodeDelta.times(m, c));
}

fn del(gpa: Allocator, ino: u64, seq: u64) !Record {
    return Record.delete(gpa, &key.inodeKey(ino), seq);
}

/// A bset image of `records` wrapped as a shared source.
fn imageOf(gpa: Allocator, records: []const Record) !Bytes {
    var refs: [128]RecordRef = undefined;
    for (records, 0..) |*r, i| refs[i] = r.ref();
    const img = try bset.buildBset(gpa, refs[0..records.len], records[records.len - 1].seq);
    return Bytes.fromOwned(gpa, img);
}

fn emptyRun(gpa: Allocator) !*Arc(Run) {
    return Arc(Run).create(gpa, Run.empty);
}

fn snapshotOver(gpa: Allocator, sources: []Bytes) !*Arc(NodeSnapshot) {
    const idx = try Arc(RecordIndex).create(gpa, try RecordIndex.build(gpa, sources, null));
    errdefer idx.release();
    const overlay = try emptyRun(gpa);
    errdefer overlay.release();
    return Arc(NodeSnapshot).create(gpa, NodeSnapshot.init(idx, overlay, null));
}

test "IdxRec is 24 bytes" {
    try testing.expectEqual(@as(usize, 24), @sizeOf(IdxRec));
}

test "RecordIndex.build merges sources (key asc, seq desc, newer source first) and normalizes same-key runs within one source" {
    const gpa = testing.allocator;
    var oldest = [_]Record{ try put(gpa, 1, 2), try put(gpa, 2, 7), try put(gpa, 4, 1) };
    defer rec.deinitRecords(gpa, &oldest);
    var mid = [_]Record{ try put(gpa, 1, 5), try dtimes(gpa, 1, 6, 1, 1), try put(gpa, 3, 6) };
    defer rec.deinitRecords(gpa, &mid);
    var newest = [_]Record{ try dtimes(gpa, 1, 9, 9, 9), try put(gpa, 2, 7) };
    defer rec.deinitRecords(gpa, &newest);
    const sources = try gpa.alloc(Bytes, 3);
    sources[0] = try imageOf(gpa, &oldest);
    sources[1] = try imageOf(gpa, &mid);
    sources[2] = try imageOf(gpa, &newest);
    var idx = try RecordIndex.build(gpa, sources, null);
    defer idx.deinit(gpa);
    const want = [_]struct { u64, u64, u16 }{ .{ 1, 9, 2 }, .{ 1, 6, 1 }, .{ 1, 5, 1 }, .{ 1, 2, 0 }, .{ 2, 7, 2 }, .{ 2, 7, 0 }, .{ 3, 6, 1 }, .{ 4, 1, 0 } };
    try testing.expectEqual(want.len, idx.entries.len);
    for (want, 0..) |w, i| {
        const r = idx.recordRef(i);
        try testing.expectEqualSlices(u8, &key.inodeKey(w[0]), r.key);
        try testing.expectEqual(w[1], r.seq);
        try testing.expectEqual(w[2], idx.entries[i].src);
    }
    try testing.expectEqual(Range{ .lo = 0, .hi = 4 }, idx.groupBounds(&key.inodeKey(1)));
    try testing.expectEqual(Range{ .lo = 6, .hi = 7 }, idx.groupBounds(&key.inodeKey(3)));
    try testing.expectEqual(Range{ .lo = 8, .hi = 8 }, idx.groupBounds(&key.inodeKey(9)));
    try testing.expectEqual(@as(usize, 4), idx.firstAtOrAfter(&key.inodeKey(2)));
    // Zero-copy: the key borrows the image.
    try testing.expectEqual(@intFromPtr(sources[2].slice().ptr) + bset.BSET_HEADER_LEN + rec.RECORD_HEADER_LEN, @intFromPtr(idx.recordRef(0).key.ptr));
}

test "RecordIndex charge tie: indexBytes = entries × @sizeOf(IdxRec) + sources × @sizeOf(Bytes); build charges the gauge and the stat, extendWith charges a second index while the first still stands, each deinit credits exactly its own" {
    const gpa = testing.allocator;
    try testing.expectEqual(@as(u64, 0), RecordIndex.indexBytes(0, 0));
    try testing.expectEqual(@as(u64, 24 * 2_000 + 16 * 3), RecordIndex.indexBytes(2_000, 3));
    try testing.expectEqual(@as(usize, 16), @sizeOf(Bytes));
    var a = [_]Record{ try put(gpa, 1, 1), try put(gpa, 3, 2), try put(gpa, 5, 3) };
    defer rec.deinitRecords(gpa, &a);
    var b = [_]Record{ try dtimes(gpa, 1, 4, 1, 1), try put(gpa, 2, 5) };
    defer rec.deinitRecords(gpa, &b);
    var gauge: counters.Gauge = .{};
    const g0 = stats.meta_kv_node_index_bytes.load();
    const s1 = try gpa.alloc(Bytes, 1);
    s1[0] = try imageOf(gpa, &a);
    var first = try RecordIndex.build(gpa, s1, &gauge);
    const first_bytes = RecordIndex.indexBytes(3, 1);
    try testing.expectEqual(first_bytes, first.charged);
    try testing.expectEqual(first_bytes, gauge.load());
    try testing.expectEqual(g0 + first_bytes, stats.meta_kv_node_index_bytes.load());
    // The incremental extension is a NEW index: both charge until the old one dies.
    var second = try first.extendWith(gpa, try imageOf(gpa, &b));
    const second_bytes = RecordIndex.indexBytes(5, 2);
    try testing.expectEqual(second_bytes, second.charged);
    try testing.expect(second.charge == first.charge);
    try testing.expectEqual(first_bytes + second_bytes, gauge.load());
    first.deinit(gpa);
    try testing.expectEqual(second_bytes, gauge.load());
    second.deinit(gpa);
    try testing.expectEqual(@as(u64, 0), gauge.load());
    try testing.expectEqual(g0, stats.meta_kv_node_index_bytes.load());
    // A standalone index (no cache) charges the stat alone.
    const s2 = try gpa.alloc(Bytes, 1);
    s2[0] = try imageOf(gpa, &b);
    var alone = try RecordIndex.build(gpa, s2, null);
    try testing.expectEqual(g0 + RecordIndex.indexBytes(2, 1), stats.meta_kv_node_index_bytes.load());
    alone.deinit(gpa);
    try testing.expectEqual(g0, stats.meta_kv_node_index_bytes.load());
}

test "RecordIndex.extendWith equals a from-scratch build over the same sources" {
    const gpa = testing.allocator;
    var a = [_]Record{ try put(gpa, 1, 1), try put(gpa, 3, 2), try put(gpa, 5, 3) };
    defer rec.deinitRecords(gpa, &a);
    var b = [_]Record{ try dtimes(gpa, 1, 4, 1, 1), try put(gpa, 2, 5), try del(gpa, 5, 6) };
    defer rec.deinitRecords(gpa, &b);
    var c = [_]Record{ try put(gpa, 1, 7), try del(gpa, 1, 8), try put(gpa, 6, 9) };
    defer rec.deinitRecords(gpa, &c);
    const ia = try imageOf(gpa, &a);
    const ib = try imageOf(gpa, &b);
    const ic = try imageOf(gpa, &c);
    // Incremental.
    const s1 = try gpa.alloc(Bytes, 1);
    s1[0] = ia.clone();
    var inc1 = try RecordIndex.build(gpa, s1, null);
    defer inc1.deinit(gpa);
    var inc2 = try inc1.extendWith(gpa, ib.clone());
    defer inc2.deinit(gpa);
    var inc3 = try inc2.extendWith(gpa, ic.clone());
    defer inc3.deinit(gpa);
    // From scratch.
    const s3 = try gpa.alloc(Bytes, 3);
    s3[0] = ia;
    s3[1] = ib;
    s3[2] = ic;
    var full = try RecordIndex.build(gpa, s3, null);
    defer full.deinit(gpa);
    try testing.expectEqual(full.entries.len, inc3.entries.len);
    for (0..full.entries.len) |i| {
        try testing.expect(full.recordRef(i).eql(inc3.recordRef(i)));
        try testing.expectEqual(full.entries[i].src, inc3.entries[i].src);
    }
}

test "snapshot lookup folds across sources; a delta chain is a memo miss then a memo hit with zero decodes; newestSeqOf; census" {
    const gpa = testing.allocator;
    var base_recs = [_]Record{ try put(gpa, 10, 1), try put(gpa, 20, 2), try put(gpa, 30, 3) };
    defer rec.deinitRecords(gpa, &base_recs);
    var later = [_]Record{ try dtimes(gpa, 20, 4, 100, 200), try del(gpa, 30, 5), try dtimes(gpa, 40, 6, 1, 1) };
    defer rec.deinitRecords(gpa, &later);
    const sources = try gpa.alloc(Bytes, 2);
    sources[0] = try imageOf(gpa, &base_recs);
    sources[1] = try imageOf(gpa, &later);
    const snap = try snapshotOver(gpa, sources);
    defer snap.release();
    const s = &snap.value;

    // Plain Put: borrowed, never memoized.
    var l10 = try s.lookup(gpa, &key.inodeKey(10));
    defer l10.deinit();
    try testing.expectEqualSlices(u8, &iv(1).encode(), l10.liveValue().?);
    try testing.expectEqual(@as(usize, 0), s.memo.populated());

    // Delta chain: first fold is a miss (populates), second a hit, zero decodes.
    var want = iv(2);
    InodeDelta.times(100, 200).apply(&want);
    const m0 = stats.meta_kv_fold_memo_misses.load();
    const h0 = stats.meta_kv_fold_memo_hits.load();
    var l20 = try s.lookup(gpa, &key.inodeKey(20));
    defer l20.deinit();
    try testing.expectEqualSlices(u8, &want.encode(), l20.liveValue().?);
    try testing.expectEqual(m0 + 1, stats.meta_kv_fold_memo_misses.load());
    try testing.expectEqual(@as(usize, 1), s.memo.populated());
    const d0 = rec.stats.meta_kv_fold_record_decodes.load();
    var l20b = try s.lookup(gpa, &key.inodeKey(20));
    defer l20b.deinit();
    try testing.expectEqualSlices(u8, &want.encode(), l20b.liveValue().?);
    try testing.expectEqual(h0 + 1, stats.meta_kv_fold_memo_hits.load());
    try testing.expectEqual(d0, rec.stats.meta_kv_fold_record_decodes.load());
    try testing.expect(l20b.borrowed());

    // Tombstone, orphan delta (absent), unknown key.
    var l30 = try s.lookup(gpa, &key.inodeKey(30));
    defer l30.deinit();
    try testing.expect(l30 == .tombstone);
    var l40 = try s.lookup(gpa, &key.inodeKey(40));
    defer l40.deinit();
    try testing.expect(l40 == .absent);
    var l99 = try s.lookup(gpa, &key.inodeKey(99));
    defer l99.deinit();
    try testing.expect(l99 == .absent);

    try testing.expectEqual(@as(?u64, 4), s.newestSeqOf(&key.inodeKey(20)));
    try testing.expectEqual(@as(?u64, 5), s.newestSeqOf(&key.inodeKey(30)));
    try testing.expectEqual(@as(?u64, null), s.newestSeqOf(&key.inodeKey(99)));
    const census = s.indexedRecordCensus();
    try testing.expectEqual(@as(u64, 6), census.total);
    try testing.expectEqual(@as(u64, 4), census.distinct);
    try testing.expectEqual(@as(usize, 0), s.overlayLen());
    try testing.expectEqual(@as(usize, 6), s.indexedLen());
    // The chain probe counts the delta above the base.
    try testing.expectEqual(@as(u32, 1), s.deltaChainProbe(&key.inodeKey(20)).depth);
    try testing.expectEqual(@as(u32, 0), s.deltaChainProbe(&key.inodeKey(10)).depth);
    try testing.expectEqual(@as(u32, 1), s.deltaChainProbe(&key.inodeKey(40)).depth);
}

test "nextLive/prevLive skip tombstones and orphans, honor the inclusive end, and walk both directions" {
    const gpa = testing.allocator;
    var base_recs = [_]Record{ try put(gpa, 1, 1), try put(gpa, 2, 2), try put(gpa, 3, 3), try put(gpa, 4, 4), try put(gpa, 5, 5) };
    defer rec.deinitRecords(gpa, &base_recs);
    var later = [_]Record{ try del(gpa, 2, 6), try dtimes(gpa, 3, 7, 1, 1), try del(gpa, 4, 8) };
    defer rec.deinitRecords(gpa, &later);
    const sources = try gpa.alloc(Bytes, 2);
    sources[0] = try imageOf(gpa, &base_recs);
    sources[1] = try imageOf(gpa, &later);
    const snap = try snapshotOver(gpa, sources);
    defer snap.release();
    const s = &snap.value;

    var e1 = (try s.nextLive(gpa, &key.inodeKey(0), null)).?;
    defer e1.deinit();
    try testing.expectEqualSlices(u8, &key.inodeKey(1), e1.key);
    var e3 = (try s.nextLive(gpa, &key.inodeKey(2), null)).?;
    defer e3.deinit();
    try testing.expectEqualSlices(u8, &key.inodeKey(3), e3.key);
    var want3 = iv(3);
    InodeDelta.times(1, 1).apply(&want3);
    try testing.expectEqualSlices(u8, &want3.encode(), e3.value);
    try testing.expectEqual(@as(?LiveEntry, null), try s.nextLive(gpa, &key.inodeKey(2), &key.inodeKey(2)));
    var e5 = (try s.nextLive(gpa, &key.inodeKey(4), null)).?;
    defer e5.deinit();
    try testing.expectEqualSlices(u8, &key.inodeKey(5), e5.key);
    try testing.expectEqual(@as(?LiveEntry, null), try s.nextLive(gpa, &key.inodeKey(6), null));

    var p3 = (try s.prevLive(gpa, &key.inodeKey(5))).?;
    defer p3.deinit();
    try testing.expectEqualSlices(u8, &key.inodeKey(3), p3.key);
    var p1 = (try s.prevLive(gpa, &key.inodeKey(3))).?;
    defer p1.deinit();
    try testing.expectEqualSlices(u8, &key.inodeKey(1), p1.key);
    try testing.expectEqual(@as(?LiveEntry, null), try s.prevLive(gpa, &key.inodeKey(1)));
}

test "foldBytesUpperWith: pending base-establishers shadow residents, deltas ride on top, covered tombstones project 0, parts pack greedily" {
    const gpa = testing.allocator;
    var base_recs = [_]Record{ try put(gpa, 1, 1), try put(gpa, 2, 2), try del(gpa, 3, 3) };
    defer rec.deinitRecords(gpa, &base_recs);
    const sources = try gpa.alloc(Bytes, 1);
    sources[0] = try imageOf(gpa, &base_recs);
    const snap = try snapshotOver(gpa, sources);
    defer snap.release();
    const s = &snap.value;
    const put_len = base_recs[0].ref().encodedLen();
    const del_len = base_recs[2].ref().encodedLen();
    // No extras, tail 0 (never elide): three groups.
    var none: [0]Extra = .{};
    try testing.expectEqual(Projection{ .bytes = 2 * put_len + del_len, .parts = 1 }, s.foldBytesUpperWith(&none, 1 << 20, 0));
    // The tombstone (seq 3) is covered by tail 4: its group projects 0.
    try testing.expectEqual(Projection{ .bytes = 2 * put_len, .parts = 1 }, s.foldBytesUpperWith(&none, 1 << 20, 4));
    // A pending Delete of key 1 shadows its resident Put; a pending Delta on key 2 rides on top.
    var extras = [_]Extra{
        .{ .key = &key.inodeKey(2), .kind = .delta, .len = 20 },
        .{ .key = &key.inodeKey(1), .kind = .delete, .len = del_len },
    };
    try testing.expectEqual(Projection{ .bytes = del_len + put_len + 20 + del_len, .parts = 1 }, s.foldBytesUpperWith(&extras, 1 << 20, 0));
    // Greedy parts: a budget below one group forces a part per group.
    try testing.expectEqual(@as(usize, 3), s.foldBytesUpperWith(&none, 1, 0).parts);
    try testing.expectEqual(@as(usize, 2), s.foldBytesUpperWith(&none, 2 * put_len, 0).parts);
}

fn ownedPut(gpa: Allocator, k: []const u8, seq: u64) !OwnedRec {
    const v = iv(seq).encode();
    return OwnedRec.fromRecordRef(gpa, .{ .key = k, .seq = seq, .kind = .put, .value = &v });
}

test "property: a run edited by random sorted inserts equals the flat sorted array — chunks split at CHUNK_MAX and stay non-empty, the spine's starts are exact, posOf/next/prev/at/partitionPoint agree with the array, a published run is untouched by the edits after it" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0x5eed_c4ea);
    const rnd = prng.random();
    var mirror: std.ArrayListUnmanaged(OwnedRec) = .empty;
    defer {
        for (mirror.items) |*r| r.deinit();
        mirror.deinit(gpa);
    }
    var cur = try Arc(Run).create(gpa, Run.empty);
    defer cur.release();
    // A reader's run from the middle of the history, checked at the end.
    var held: ?*Arc(Run) = null;
    defer if (held) |h| h.release();
    var held_len: usize = 0;
    var seq: u64 = 1;
    const N = 700;
    for (0..N) |step| {
        // Mostly fresh keys at random places, some repeats (a key's seq
        // group), seqs ascending.
        const k: [key.INODE_KEY_LEN]u8 = if (rnd.uintLessThan(u8, 4) == 0 and mirror.items.len > 0)
            mirror.items[rnd.uintLessThan(usize, mirror.items.len)].key.slice()[0..key.INODE_KEY_LEN].*
        else
            key.inodeKey(rnd.uintLessThan(u64, 10_000));
        var r = try ownedPut(gpa, &k, seq);
        defer r.deinit();
        seq += 1;
        // The mirror's sorted insert.
        const ctx: RunCtx = .{ .k = r.key.slice() };
        const pos = std.sort.partitionPoint(OwnedRec, mirror.items, ctx, mirrorKeyLessEq);
        try mirror.insert(gpa, pos, r.clone());
        var edit = try RunEdit.begin(gpa, &cur.value);
        _ = try edit.insert(gpa, pos, r.recordRef(), r.entry_floor, .own_value);
        const next = try Arc(Run).create(gpa, try edit.finish(gpa));
        cur.release();
        cur = next;
        if (step == N / 2) {
            held = cur.retain();
            held_len = cur.value.len;
        } else if (step == N / 2 + 1) {
            // One insert after the hold: one chunk copied (two if it
            // split), every other chunk shared with the held run.
            var shared: usize = 0;
            for (held.?.value.slots) |s| shared += @intFromBool(s.chunk.strongCount() > 1);
            try testing.expect(shared + 1 >= held.?.value.slots.len);
            try testing.expect(cur.value.len == held_len + 1);
        }
    }
    const run = &cur.value;
    try testing.expectEqual(mirror.items.len, run.len);
    // Shape: every chunk non-empty and ≤ CHUNK_MAX; starts exact.
    var expect_start: usize = 0;
    for (run.slots) |s| {
        const n = s.chunk.value.len();
        try testing.expect(n > 0 and n <= CHUNK_MAX);
        try testing.expectEqual(expect_start, s.start);
        expect_start += n;
    }
    try testing.expect(run.slots.len >= N / CHUNK_MAX);
    // Content and cursors.
    var p = run.posOf(0);
    for (mirror.items, 0..) |*m, i| {
        try testing.expect(m.recordRef().eql(run.at(i).ref()));
        try testing.expect(m.recordRef().eql(run.rec(p).ref()));
        try testing.expectEqual(i, run.index(p));
        try testing.expectEqual(p, run.posOf(i));
        if (i > 0) try testing.expect(mirror.items[i - 1].recordRef().eql(run.rec(run.prev(p)).ref()));
        // Every record carries its Put head: the record's own value.
        try testing.expectEqualSlices(u8, m.value.slice(), run.at(i).head().?.live);
        p = run.next(p);
    }
    try testing.expectEqual(run.len, run.index(p));
    // Groups: every key's `[lo, hi)` equals the array's.
    for (0..64) |_| {
        const k = key.inodeKey(rnd.uintLessThan(u64, 10_000));
        const ctx: RunCtx = .{ .k = &k };
        try testing.expectEqual(Range{
            .lo = std.sort.partitionPoint(OwnedRec, mirror.items, ctx, mirrorKeyLess),
            .hi = std.sort.partitionPoint(OwnedRec, mirror.items, ctx, mirrorKeyLessEq),
        }, runGroup(run, &k));
    }
    // The held run is the history's middle, exactly, whatever applied since.
    const h = &held.?.value;
    try testing.expectEqual(held_len, h.len);
    var hp = h.posOf(0);
    var mi: usize = 0;
    for (0..h.len) |_| {
        // Every held record is in the final mirror, in order.
        const hr = h.rec(hp).ref();
        while (!mirror.items[mi].recordRef().eql(hr)) mi += 1;
        mi += 1;
        hp = h.next(hp);
    }
    // Round trip through the flat forms: records and heads alike.
    const flat = try run.flatten(gpa);
    defer gpa.free(flat);
    for (flat) |*f| try testing.expect(f.folded != null and f.folded.? == .live and !f.folded.?.live.materialized);
    var rebuilt = try Run.fromSorted(gpa, flat);
    defer rebuilt.deinit(gpa);
    try testing.expectEqual(run.len, rebuilt.len);
    for (0..run.len) |i| {
        try testing.expect(run.at(i).ref().eql(rebuilt.at(i).ref()));
        try testing.expectEqualSlices(u8, run.at(i).head().?.live, rebuilt.at(i).head().?.live);
    }
}

fn mirrorKeyLess(ctx: RunCtx, r: OwnedRec) bool {
    return key.order(r.key.slice(), ctx.k) == .lt;
}

fn mirrorKeyLessEq(ctx: RunCtx, r: OwnedRec) bool {
    return key.order(r.key.slice(), ctx.k) != .gt;
}

test "RunEdit.clearHead copies a shared chunk once and mutates the copy: the published run's record keeps its head, the edit's loses it (its charge returned); an insert into a chunk the edit owns grows it in place (one chunk box per touched chunk); a delta-folded head is copied into the arena and charged as materialized" {
    const gpa = testing.allocator;
    var recs: [5]OwnedRec = undefined;
    for (&recs, 0..) |*r, i| r.* = try ownedPut(gpa, &key.inodeKey(10 * (i + 1)), i + 1);
    for (&recs) |*r| r.folded = .{ .live = .{ .value = r.value.clone(), .materialized = false } };
    const base = try Arc(Run).create(gpa, try Run.fromSorted(gpa, &recs));
    defer base.release();
    try testing.expectEqual(@as(usize, 1), base.value.slots.len);
    var edit = try RunEdit.begin(gpa, &base.value);
    // Shared: the copy.
    try testing.expectEqual(@as(u64, 2), base.value.slots[0].chunk.strongCount());
    try testing.expectEqual(FOLD_HEAD_OVERHEAD, try edit.clearHead(gpa, 2));
    try testing.expectEqual(@as(u64, 1), base.value.slots[0].chunk.strongCount());
    try testing.expectEqual(@as(u64, 1), edit.slots.items[0].chunk.strongCount());
    try testing.expect(base.value.at(2).head() != null);
    try testing.expect(edit.view().at(2).head() == null);
    try testing.expectEqual(@as(usize, 0), try edit.clearHead(gpa, 2));
    // Owned now: an insert grows the same chunk; no second box.
    const box_before = edit.slots.items[0].chunk;
    var r = try ownedPut(gpa, &key.inodeKey(25), 9);
    defer r.deinit();
    try testing.expectEqual(FOLD_HEAD_OVERHEAD + 3, try edit.insert(gpa, 2, r.recordRef(), 7, .{ .folded = "xyz" }));
    try testing.expect(edit.slots.items[0].chunk == box_before);
    try testing.expectEqual(@as(usize, 6), edit.view().len);
    try testing.expectEqualSlices(u8, &key.inodeKey(25), edit.view().at(2).key());
    try testing.expectEqualSlices(u8, "xyz", edit.view().at(2).head().?.live);
    try testing.expectEqual(@as(u64, 7), edit.view().at(2).rec.entry_floor);
    try testing.expectEqualSlices(u8, &key.inodeKey(30), edit.view().at(3).key());
    var next = try edit.finish(gpa);
    defer next.deinit(gpa);
    try testing.expectEqual(@as(usize, 5), base.value.len);
    try testing.expectEqual(@as(usize, 6), next.len);
    // The flat form carries the materialized head as its own buffer.
    const flat = try next.flatten(gpa);
    defer {
        for (flat) |*f| f.deinit();
        gpa.free(flat);
    }
    try testing.expect(flat[2].folded.?.live.materialized);
    try testing.expectEqualSlices(u8, "xyz", flat[2].folded.?.live.value.slice());
    try testing.expectEqual(FOLD_HEAD_OVERHEAD + 3, flat[2].headBytes());
    // Key 30 (now at 3) lost its head above; key 40 keeps its Put head.
    try testing.expect(flat[3].folded == null);
    try testing.expect(!flat[4].folded.?.live.materialized);
    try testing.expectEqualSlices(u8, flat[4].value.slice(), flat[4].folded.?.live.value.slice());
}

test "Run.applyBytesBound tie: two chunk copies of CHUNK_MAX + 1 records and their arena with their boxes, one slot per half-full chunk plus two, the run's box; ChunkRec is 48 bytes" {
    try testing.expectEqual(@as(usize, 48), @sizeOf(ChunkRec));
    const copy = (CHUNK_MAX + 1) * (48 + 100);
    try testing.expectEqual(2 * copy + 2 * @sizeOf(Arc(Chunk)) + 2 * @sizeOf(Slot) + @sizeOf(Arc(Run)), Run.applyBytesBound(0, 100));
    try testing.expectEqual(2 * copy + 2 * @sizeOf(Arc(Chunk)) + (4096 / (CHUNK_MAX / 2) + 2) * @sizeOf(Slot) + @sizeOf(Arc(Run)), Run.applyBytesBound(4096, 100));
    // The copy's two blocks are size-class blocks under smp_allocator (its
    // largest class is 32 KiB; past it a block maps pages).
    try testing.expect(copy < 32 * 1024);
}

test "FoldMemo: populate-once, bounded at 8 cells, charges both gauges and credits exactly on deinit" {
    const gpa = testing.allocator;
    var gauge: counters.Gauge = .{};
    var memo: FoldMemo = .init(&gauge);
    const g0 = stats.meta_kv_fold_memo_bytes.load();
    var i: u64 = 0;
    while (i < FOLD_MEMO_CAPACITY + 2) : (i += 1) {
        const k = key.inodeKey(i);
        const v = try Bytes.copy(gpa, "value");
        const stored = try memo.populate(gpa, &k, v, i);
        if (i < FOLD_MEMO_CAPACITY) {
            try testing.expect(stored);
        } else {
            // Full: the entry is dropped and the caller keeps its reference.
            try testing.expect(!stored);
            v.deinit();
        }
    }
    try testing.expectEqual(FOLD_MEMO_CAPACITY, memo.populated());
    const per_cell: u64 = key.INODE_KEY_LEN + 5 + MEMO_CELL_OVERHEAD;
    try testing.expectEqual(per_cell * FOLD_MEMO_CAPACITY, memo.chargedBytes());
    try testing.expectEqual(per_cell * FOLD_MEMO_CAPACITY, gauge.load());
    try testing.expectEqual(g0 + per_cell * FOLD_MEMO_CAPACITY, stats.meta_kv_fold_memo_bytes.load());
    try testing.expectEqualSlices(u8, "value", memo.probe(&key.inodeKey(3)).?.value.slice());
    try testing.expectEqual(@as(?*const MemoCell, null), memo.probe(&key.inodeKey(FOLD_MEMO_CAPACITY)));
    // A racer's duplicate key is refused (the winner's cell serves).
    const dup = try Bytes.copy(gpa, "other");
    defer dup.deinit();
    var fresh: FoldMemo = .init(null);
    const first = try Bytes.copy(gpa, "first");
    try testing.expect(try fresh.populate(gpa, "k", first, 1));
    try testing.expect(!try fresh.populate(gpa, "k", dup, 1));
    fresh.deinit(gpa);
    memo.deinit(gpa);
    try testing.expectEqual(@as(u64, 0), gauge.load());
    try testing.expectEqual(g0, stats.meta_kv_fold_memo_bytes.load());
}

/// An allocator that counts allocation calls (the warm-path zero-alloc pin).
const CountingAllocator = struct {
    inner: Allocator,
    allocs: usize = 0,

    fn allocator(self: *CountingAllocator) Allocator {
        return .{ .ptr = self, .vtable = &vtable };
    }

    const vtable: Allocator.VTable = .{ .alloc = alloc, .resize = resize, .remap = remap, .free = free };

    fn alloc(ctx: *anyopaque, len: usize, alignment: std.mem.Alignment, ret_addr: usize) ?[*]u8 {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        self.allocs += 1;
        return self.inner.rawAlloc(len, alignment, ret_addr);
    }
    fn resize(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, new_len: usize, ret_addr: usize) bool {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        return self.inner.rawResize(memory, alignment, new_len, ret_addr);
    }
    fn remap(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, new_len: usize, ret_addr: usize) ?[*]u8 {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        return self.inner.rawRemap(memory, alignment, new_len, ret_addr);
    }
    fn free(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, ret_addr: usize) void {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        self.inner.rawFree(memory, alignment, ret_addr);
    }
};

test "warm lookups allocate nothing: a memo hit and a plain-Put serve are 0 allocs" {
    var counting: CountingAllocator = .{ .inner = testing.allocator };
    const gpa = counting.allocator();
    var base_recs = [_]Record{ try put(gpa, 1, 1), try put(gpa, 2, 2) };
    defer rec.deinitRecords(gpa, &base_recs);
    var later = [_]Record{try dtimes(gpa, 2, 3, 7, 8)};
    defer rec.deinitRecords(gpa, &later);
    const sources = try gpa.alloc(Bytes, 2);
    sources[0] = try imageOf(gpa, &base_recs);
    sources[1] = try imageOf(gpa, &later);
    const snap = try snapshotOver(gpa, sources);
    defer snap.release();
    const s = &snap.value;
    // Cold: populates the memo (allocates).
    var cold = try s.lookup(gpa, &key.inodeKey(2));
    cold.deinit();
    const before = counting.allocs;
    var warm = try s.lookup(gpa, &key.inodeKey(2));
    defer warm.deinit();
    var plain = try s.lookup(gpa, &key.inodeKey(1));
    defer plain.deinit();
    var miss = try s.lookup(gpa, &key.inodeKey(9));
    defer miss.deinit();
    try testing.expectEqual(before, counting.allocs);
    try testing.expect(warm.borrowed() and plain.borrowed());
}

test "property: over random bset histories the memo-served repeat read equals the from-scratch fold" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0x5eed_f01d);
    const rnd = prng.random();
    var case: usize = 0;
    while (case < 48) : (case += 1) {
        // Up to 6 bsets of up to 6 records over 4 keys, seqs strictly increasing.
        var all: std.ArrayList(Record) = .empty;
        defer {
            rec.deinitRecords(gpa, all.items);
            all.deinit(gpa);
        }
        var sources: std.ArrayList(Bytes) = .empty;
        var seq: u64 = 1;
        const nb = 1 + rnd.uintLessThan(usize, 6);
        for (0..nb) |_| {
            var batch: std.ArrayList(Record) = .empty;
            defer batch.deinit(gpa);
            const n = 1 + rnd.uintLessThan(usize, 6);
            var used: [4]bool = @splat(false);
            for (0..n) |_| {
                const k = rnd.uintLessThan(u64, 4);
                // One record per key per bset keeps the image strictly (key, seq) ascending after sorting.
                if (used[k]) continue;
                used[k] = true;
                const r = switch (rnd.uintLessThan(u8, 3)) {
                    0 => try put(gpa, k, seq),
                    1 => try dtimes(gpa, k, seq, rnd.int(u64), rnd.int(u64)),
                    else => try del(gpa, k, seq),
                };
                seq += 1;
                try batch.append(gpa, r);
            }
            std.sort.pdq(Record, batch.items, {}, struct {
                fn lt(_: void, a: Record, b: Record) bool {
                    return key.lessThan(a.key, b.key);
                }
            }.lt);
            try sources.append(gpa, try imageOf(gpa, batch.items));
            try all.appendSlice(gpa, batch.items);
        }
        const snap = try snapshotOver(gpa, try sources.toOwnedSlice(gpa));
        defer snap.release();
        for (0..4) |k| {
            const kk = key.inodeKey(k);
            var newest_first: std.ArrayList(RecordRef) = .empty;
            defer newest_first.deinit(gpa);
            var i = all.items.len;
            while (i > 0) {
                i -= 1;
                if (std.mem.eql(u8, all.items[i].key, &kk)) try newest_first.append(gpa, all.items[i].ref());
            }
            var want = try rec.foldNewestFirst(gpa, newest_first.items);
            defer want.deinit(gpa);
            var got1 = try snap.value.lookup(gpa, &kk);
            defer got1.deinit();
            var got2 = try snap.value.lookup(gpa, &kk);
            defer got2.deinit();
            const w = want.liveValue();
            if (w) |wv| {
                try testing.expectEqualSlices(u8, wv, got1.liveValue().?);
                try testing.expectEqualSlices(u8, wv, got2.liveValue().?);
            } else {
                try testing.expectEqual(@as(?[]const u8, null), got1.liveValue());
                try testing.expectEqual(@as(?[]const u8, null), got2.liveValue());
            }
        }
    }
}
