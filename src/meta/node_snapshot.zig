//! The immutable node snapshot: the latch-free read view of one cached node
//! (charter §5.3 "Node cache": "arc-swapped immutable snapshot (merged
//! sorted view + on-disk bset list; readers latch-free, binary search, slice
//! views)"; §5.2 "Hot fold reads serve from a per-key materialized folded
//! head in the dirty delta + a bounded 8-entry memo on the immutable
//! snapshot"; survey `docs/porting/kv_tree_cache_commit_survey.md` §2.8,
//! §7.1; the reference's `node_cache.rs` `RecordIndex`/`FoldMemo`/
//! `NodeSnapshot`).
//!
//! Three sorted sources, every one immutable once published:
//!
//! - `base`: the `RecordIndex` — a k-way merge of every SERIALIZED bset
//!   image (the extent's frames oldest → newest, then every frozen delta
//!   image) into one `(key asc, seq desc, newer source first)` index of
//!   24-byte `IdxRec`s that borrow the images zero-copy. Shared across
//!   publishes by refcount; a freeze extends it INCREMENTALLY (the k-way
//!   rebuild per freeze was 37 % of the reference's serial create path).
//! - `stable`: the open-delta records older than the last re-merge,
//!   `(key, seq)` ascending, shared across publishes by refcount.
//! - `tail`: the ≤ `OVERLAY_TAIL_MAX` open-delta records applied since —
//!   the ONLY per-publish copy (the whole-overlay clone per apply was ~44 %
//!   of the serial create path).
//!
//! Fold precedence per key: `tail` ≥ `stable` > `base`, kept by the
//! apply path's run-precedence guard (`node_cache.zig`). `lookup` is the
//! D7-slimmed read: the newest overlay record's materialized head serves
//! with zero decodes; a bset-resident key probes the 8-cell populate-once
//! `FoldMemo`; only a cold fold runs THE algebra (`record.foldNewestFirst`)
//! — and property tests pin memo and head serves byte-equal to it.
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
/// key (D7.a): `fold(this record ∪ everything older)`. A plain-`Put` head
/// borrows the record's own value by refcount; a delta-folded head owns a
/// fresh buffer (`materialized`), the part the §5.7 budget charges.
pub const Head = union(enum) {
    live: struct {
        value: Bytes,
        /// The decoded INODE value, carried across inode-delta applies so
        /// each delta costs one decode of itself, not of the base.
        decoded: ?rec.InodeValue,
        materialized: bool,
    },
    tombstone,
    absent,

    pub fn clone(self: Head) Head {
        return switch (self) {
            .live => |l| .{ .live = .{ .value = l.value.clone(), .decoded = l.decoded, .materialized = l.materialized } },
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

    /// The borrowed algebra view (`record.foldForward`'s `prev`).
    pub fn asFolded(self: *const Head) rec.FoldedHead {
        return switch (self.*) {
            .live => |l| .{ .live = .{ .value = l.value.slice(), .decoded = l.decoded, .materialized = false } },
            .tombstone => .tombstone,
            .absent => .absent,
        };
    }

    /// Adopt an algebra outcome: an owned buffer moves into a fresh
    /// `SharedBuf`; a borrowed one (a `Put`'s own bytes) is `own` retained.
    pub fn fromFolded(gpa: Allocator, folded: rec.FoldedHead, own: Bytes) Allocator.Error!Head {
        switch (folded) {
            .live => |l| {
                if (!l.materialized) return .{ .live = .{ .value = own.clone(), .decoded = l.decoded, .materialized = false } };
                const owned: []u8 = @constCast(l.value);
                errdefer gpa.free(owned);
                return .{ .live = .{ .value = try Bytes.fromOwned(gpa, owned), .decoded = l.decoded, .materialized = true } };
            },
            .tombstone => return .tombstone,
            .absent => return .absent,
        }
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

fn runKeyLess(ctx: RunCtx, r: OwnedRec) bool {
    return key.order(r.key.slice(), ctx.k) == .lt;
}

fn runKeyLessEq(ctx: RunCtx, r: OwnedRec) bool {
    return key.order(r.key.slice(), ctx.k) != .gt;
}

/// `[lo, hi)` of `run`'s records with key `k` (seq ascending).
pub fn runGroup(run: []const OwnedRec, k: []const u8) Range {
    const ctx: RunCtx = .{ .k = k };
    return .{
        .lo = std.sort.partitionPoint(OwnedRec, run, ctx, runKeyLess),
        .hi = std.sort.partitionPoint(OwnedRec, run, ctx, runKeyLessEq),
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

/// Published open-delta records newer than the last `stable` re-merge:
/// the per-apply publish clones only this run, so it stays small by
/// construction.
pub const OVERLAY_TAIL_MAX: usize = 8;

/// A `(key asc, seq asc)` run of overlay records, shared by refcount.
pub const Run = struct {
    recs: []OwnedRec,

    pub const empty: Run = .{ .recs = &.{} };

    pub fn deinit(self: *Run, gpa: Allocator) void {
        for (self.recs) |*r| r.deinit();
        if (self.recs.len != 0) gpa.free(self.recs);
        self.* = undefined;
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
    stable: *Arc(Run),
    /// ≤ `OVERLAY_TAIL_MAX`; owned by this snapshot.
    tail: []OwnedRec,
    memo: FoldMemo,

    /// A snapshot with no overlay over `base` (takes both references).
    pub fn init(base: *Arc(RecordIndex), stable: *Arc(Run), tail: []OwnedRec, charge: ?*counters.Gauge) NodeSnapshot {
        assert(tail.len <= OVERLAY_TAIL_MAX);
        return .{ .base = base, .stable = stable, .tail = tail, .memo = .init(charge) };
    }

    pub fn deinit(self: *NodeSnapshot, gpa: Allocator) void {
        self.memo.deinit(gpa);
        for (self.tail) |*r| r.deinit();
        if (self.tail.len != 0) gpa.free(self.tail);
        self.stable.release();
        self.base.release();
        self.* = undefined;
    }

    /// Records in the open-delta overlay.
    pub fn overlayLen(self: *const NodeSnapshot) usize {
        return self.stable.value.recs.len + self.tail.len;
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

    const Groups = struct { tg: Range, sg: Range, bg: Range };

    fn groups(self: *const NodeSnapshot, k: []const u8) Groups {
        return .{
            .tg = runGroup(self.tail, k),
            .sg = runGroup(self.stable.value.recs, k),
            .bg = self.base.value.groupBounds(k),
        };
    }

    /// The newest record seq this node holds for `k` across every source,
    /// or null when the key is unknown here. Order-independent (max per
    /// group), so safe mid-replay before the fold-order invariant is
    /// re-established — replay's per-key LWW gate reads it.
    pub fn newestSeqOf(self: *const NodeSnapshot, k: []const u8) ?u64 {
        const g = self.groups(k);
        var newest: ?u64 = null;
        // Runs are (key, seq) ascending: a group's last is its newest.
        if (g.tg.hi > g.tg.lo) newest = self.tail[g.tg.hi - 1].seq;
        if (g.sg.hi > g.sg.lo) {
            const s = self.stable.value.recs[g.sg.hi - 1].seq;
            newest = if (newest) |n| @max(n, s) else s;
        }
        // Base groups are (seq desc): the group's first is its newest.
        if (g.bg.hi > g.bg.lo) {
            const b = self.base.value.entries[g.bg.lo].seq;
            newest = if (newest) |n| @max(n, b) else b;
        }
        return newest;
    }

    /// The newest overlay record of `k` (tail ≥ stable per key), if any.
    fn newestOverlay(self: *const NodeSnapshot, g: Groups) ?*const OwnedRec {
        if (g.tg.hi > g.tg.lo) return &self.tail[g.tg.hi - 1];
        if (g.sg.hi > g.sg.lo) return &self.stable.value.recs[g.sg.hi - 1];
        return null;
    }

    /// Scratch for one key's gather: the overlay groups plus the base group,
    /// newest first. Inline up to 64 records; longer chains (cold by
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
    };

    fn gather(self: *const NodeSnapshot, gpa: Allocator, g: Groups, out: *Gather) Allocator.Error!void {
        const n = (g.tg.hi - g.tg.lo) + (g.sg.hi - g.sg.lo) + (g.bg.hi - g.bg.lo);
        const dst: []RecordRef = if (n <= out.inline_buf.len) out.inline_buf[0..n] else blk: {
            out.heap = try gpa.alloc(RecordRef, n);
            break :blk out.heap.?;
        };
        var i: usize = 0;
        var t = g.tg.hi;
        while (t > g.tg.lo) : (i += 1) {
            t -= 1;
            dst[i] = self.tail[t].recordRef();
        }
        var s = g.sg.hi;
        while (s > g.sg.lo) : (i += 1) {
            s -= 1;
            dst[i] = self.stable.value.recs[s].recordRef();
        }
        for (g.bg.lo..g.bg.hi) |b| {
            dst[i] = self.base.value.recordRef(b);
            i += 1;
        }
        out.len = n;
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
            if (n.folded) |h| {
                stats.meta_kv_fold_head_serves.inc();
                return switch (h) {
                    .live => |l| .{ .live = .{ .value = l.value.slice(), .held = null } },
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
        // The newest record decides alone unless it is a Delta (the fold
        // stops at the first Put or Delete — `rec.foldNewestFirst`): the
        // common base-resident key folds with no gather scratch, whose 64
        // record refs ReleaseSafe fills on every from-scratch fold (13 % of
        // the FUSE worker's cycles on rand-4k, `docs/evidence/rung-10-dev-
        // perf.md` §3.4).
        const top: RecordRef = if (newest) |n| n.recordRef() else if (g.bg.hi > g.bg.lo) self.base.value.recordRef(g.bg.lo) else return .absent;
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
        const g = self.groups(k);
        var depth: u32 = 0;
        var head: ?rec.LayoutVersions = null;
        var t = g.tg.hi;
        while (t > g.tg.lo) {
            t -= 1;
            if (!chainStep(self.tail[t].recordRef(), &depth, &head)) return .{ .depth = depth, .head = head };
        }
        var s = g.sg.hi;
        while (s > g.sg.lo) {
            s -= 1;
            if (!chainStep(self.stable.value.recs[s].recordRef(), &depth, &head)) return .{ .depth = depth, .head = head };
        }
        for (g.bg.lo..g.bg.hi) |b| {
            if (!chainStep(self.base.value.recordRef(b), &depth, &head)) break;
        }
        return .{ .depth = depth, .head = head };
    }

    fn chainStep(r: RecordRef, depth: *u32, head: *?rec.LayoutVersions) bool {
        if (r.kind != .delta) return false;
        if (depth.* == 0) head.* = rec.layoutDeltaVersions(r.value);
        depth.* += 1;
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
        const stable = self.stable.value.recs;
        var bi = idx.firstAtOrAfter(from);
        var si = std.sort.partitionPoint(OwnedRec, stable, RunCtx{ .k = from }, runKeyLess);
        var ti = std.sort.partitionPoint(OwnedRec, self.tail, RunCtx{ .k = from }, runKeyLess);
        while (true) {
            var k: ?[]const u8 = if (bi < idx.entries.len) idx.keyAt(idx.entries[bi]) else null;
            if (si < stable.len) k = minKey(k, stable[si].key.slice());
            if (ti < self.tail.len) k = minKey(k, self.tail[ti].key.slice());
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
            while (si < stable.len and std.mem.eql(u8, stable[si].key.slice(), cand)) si += 1;
            while (ti < self.tail.len and std.mem.eql(u8, self.tail[ti].key.slice(), cand)) ti += 1;
        }
    }

    /// The previous LIVE `(key, value)` with key strictly below `before` —
    /// `nextLive`'s mirror, the §4.6a left-sibling probe.
    pub fn prevLive(self: *NodeSnapshot, gpa: Allocator, before: []const u8) FoldError!?LiveEntry {
        const idx = &self.base.value;
        const stable = self.stable.value.recs;
        var bi = idx.firstAtOrAfter(before);
        var si = std.sort.partitionPoint(OwnedRec, stable, RunCtx{ .k = before }, runKeyLess);
        var ti = std.sort.partitionPoint(OwnedRec, self.tail, RunCtx{ .k = before }, runKeyLess);
        while (true) {
            var k: ?[]const u8 = if (bi > 0) idx.keyAt(idx.entries[bi - 1]) else null;
            if (si > 0) k = maxKey(k, stable[si - 1].key.slice());
            if (ti > 0) k = maxKey(k, self.tail[ti - 1].key.slice());
            const cand = k orelse return null;
            var look = try self.lookup(gpa, cand);
            switch (look) {
                .live => |l| return .{ .key = cand, .value = l.value, .held = l.held },
                else => {},
            }
            look.deinit();
            while (bi > 0 and std.mem.eql(u8, idx.keyAt(idx.entries[bi - 1]), cand)) bi -= 1;
            while (si > 0 and std.mem.eql(u8, stable[si - 1].key.slice(), cand)) si -= 1;
            while (ti > 0 and std.mem.eql(u8, self.tail[ti - 1].key.slice(), cand)) ti -= 1;
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
        const stable = self.stable.value.recs;
        var b: usize = 0;
        var s: usize = 0;
        var t: usize = 0;
        var x: usize = 0;
        var total: usize = 0;
        var parts: usize = 1;
        var acc: usize = 0;
        while (true) {
            var k: ?[]const u8 = if (b < idx.entries.len) idx.keyAt(idx.entries[b]) else null;
            if (s < stable.len) k = minKey(k, stable[s].key.slice());
            if (t < self.tail.len) k = minKey(k, self.tail[t].key.slice());
            if (x < extra.len) k = minKey(k, extra[x].key);
            const cur = k orelse break;
            var be: usize = 0;
            while (b + be < idx.entries.len and std.mem.eql(u8, idx.keyAt(idx.entries[b + be]), cur)) be += 1;
            var se: usize = 0;
            while (s + se < stable.len and std.mem.eql(u8, stable[s + se].key.slice(), cur)) se += 1;
            var te: usize = 0;
            while (t + te < self.tail.len and std.mem.eql(u8, self.tail[t + te].key.slice(), cur)) te += 1;
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
                for (stable[s .. s + se]) |*r| {
                    if (establishes(r.kind) and r.seq >= base_seq) {
                        base_seq = r.seq;
                        base_kind = r.kind;
                    }
                }
                for (self.tail[t .. t + te]) |*r| {
                    if (establishes(r.kind) and r.seq >= base_seq) {
                        base_seq = r.seq;
                        base_kind = r.kind;
                    }
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
                    for (stable[s .. s + se]) |*r| if (r.seq >= base_seq) {
                        group += r.recordRef().encodedLen();
                    };
                    for (self.tail[t .. t + te]) |*r| if (r.seq >= base_seq) {
                        group += r.recordRef().encodedLen();
                    };
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
            s += se;
            t += te;
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
    const stable = try emptyRun(gpa);
    errdefer stable.release();
    return Arc(NodeSnapshot).create(gpa, NodeSnapshot.init(idx, stable, &.{}, null));
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
