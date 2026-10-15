//! The block-map TREE ("kvmap": charter §5.6 "The block-map tree"; `docs/
//! porting/block_map_tree.md`). PORTABLE: the kind-7 records' value forms as
//! one `Rec`, the READ LAW (an exact record supersedes a covering run; the
//! floor lookup is ONE bounded forward scan of `RUN_LEN_MAX` indices that
//! keeps the LAST coverer), the record-true range iteration with the
//! covering-run prepend, the WINDOW put (a publish ships only its window: the
//! inserts and their immediate neighbours are re-canonicalized into
//! POINT/RUN records — runs form at the write side when the stripe-unit
//! placement lands consecutive blocks of one file at consecutive offsets —
//! and the `+ref`/`−ref` records of exactly the window ride the same tx), the
//! crossing plan (the A1 residue DIFF of the tree against the inline map,
//! Puts leading, stale Deletes last), the sweep page (budget by COVERED
//! INDICES, the left-boundary straddler shortened at its own key, a
//! superseding point never freed twice, the resume cursor = the first
//! unprocessed record's own key), the read-side window store (the partial
//! store: `mem_budget/16` of resident map bytes, 2 spans per ino, LRU, a
//! sheddable R5 component), and the fsck C11 record walk (report-only).
//!
//! Who holds what: every mutation here stages into the caller's `KvTx`; the
//! data plane (`plane.zig`) owns the locks (the per-ino layout lock, level
//! (3.7), across a merge, a crossing train, a sweep chunk — and across
//! kv_fs's shrink / corpse-mark transactions through `Data.lockLayout`, so
//! the head has ONE authority), the head record, the terminal frees (after
//! the lock drops — RES-1) and the device. Stride = the format's CHUNK (the
//! device bytes one block occupies), never stored.

const std = @import("std");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const key = @import("../meta/key.zig");
const record = @import("../meta/record.zig");
const tree = @import("../meta/tree.zig");
const node_cache = @import("../meta/node_cache.zig");
const tx_mod = @import("../meta/tx.zig");
const journal = @import("../meta/journal.zig");
const jc = @import("../meta/journal_core.zig");
const dlm = @import("../meta/dlm.zig");
const budget = @import("../mem/budget.zig");
const layout = @import("layout.zig");
const block_refs = @import("block_refs.zig");
const seams = @import("write_seams.zig");

pub const Mapping = layout.Mapping;
pub const KvTx = tx_mod.KvTx;
pub const RUN_LEN_MAX: u32 = record.RUN_LEN_MAX;
pub const RUN_LEN_MIN: u32 = record.RUN_LEN_MIN;

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// The widest kind-7 record frame: a RUN2 value (30 B) under the 13 B key.
pub const MAP_RECORD_FRAME_BYTES: u64 = journal.recordFrameLen(key.BLOCK_MAP_KEY_LEN, record.BLOCK_MAP_RUN_STAMPED_VALUE_LEN);

/// Ops per crossing / sweep tx — DERIVED from the whole-entry cap: HALF of
/// `MAX_ENTRY_LEN` divided by the widest op (a map record frame + the
/// reference record that rides beside it), floored to a power of two. Half,
/// because the head Put, the inode Put and a straddler's shortened re-Put
/// ride the same entry and the other half is their headroom. 65,536 / (60 +
/// 50) = 595 → 512 — finding 38's `BLOCK_REF_TX_CHUNK`; 512 × 110 B ≈ 55 KiB
/// of records per chunk.
pub const CROSSING_CHUNK: u32 = floorPow2(@intCast((journal.MAX_ENTRY_LEN / 2) / (MAP_RECORD_FRAME_BYTES + block_refs.REF_RECORD_JOURNAL_BYTES)));

/// The sweep threshold in CHUNKS: a shrink removing more than `chunk × 64`
/// blocks (32,768 ≈ 128 GiB at 4 MiB) defers its record work to the
/// background sweep; below it the same chunks drain inside the SETATTR —
/// one lever (the chunk), never a second knob.
pub const SWEEP_THRESHOLD_CHUNKS: u32 = 64;

/// The floor scan's page: one leaf-span class of records per range call.
pub const SCAN_PAGE: usize = 512;

/// The read-side window store (the partial store, §3.2): per ino ≤ 2 warm
/// spans (one demand stream + one lateral), each ONE range page of 512
/// records clipped to 4,096 expanded indices (a run-heavy page must not
/// balloon a span), charged to the budget as `mem_budget / 16` — the node-
/// cache default's divisor, NO floor (a floor would admit a map the budget
/// cannot hold), sheddable (floor 0, weight 2).
pub const WINDOW_SPANS_PER_INO: usize = 2;
pub const WINDOW_FILL_RECORDS: usize = 512;
pub const WINDOW_FILL_INDICES_MAX: u32 = 4096;
pub const WINDOW_BUDGET_DIVISOR: u64 = 16;
pub const WINDOW_SHED_WEIGHT: u64 = 2;
/// Window rows: 64 inos direct-mapped (the read path's `PipelineTable`
/// shape, 0002 row 148) — a per-ino table is bounded, never grown per ino.
pub const WINDOW_ROWS: usize = 64;

/// The first map generation a head carries at its flip (0 = absent is
/// refused by the layout codec).
pub const FIRST_MAP_GEN: u64 = 1;

fn floorPow2(v: u32) u32 {
    assert(v != 0);
    return @as(u32, 1) << @intCast(std.math.log2_int(u32, v));
}

comptime {
    assert(CROSSING_CHUNK == 512);
}

/// `chunk × 64`.
pub fn sweepThresholdBlocks(chunk: u32) u64 {
    return @as(u64, chunk) * SWEEP_THRESHOLD_CHUNKS;
}

/// `mem_budget / 16`, no floor.
pub fn windowBudgetBytes(mem_budget: u64) u64 {
    return mem_budget / WINDOW_BUDGET_DIVISOR;
}

/// `SQUEEZEFS_MAP_MIGRATE_CHUNK`: the A/B lever over the derivation, read
/// once at open (the registry refuses an out-of-range value).
pub fn chunkFromEnv(env: *const std.process.Environ.Map) u32 {
    return knobs.intKnob("SQUEEZEFS_MAP_MIGRATE_CHUNK", u32, env);
}

// ---------------------------------------------------------------------------
// Stats (the spec's names; `counters` rows owned here, never statics shared
// across mounts — Q7).
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// Records staged as Put / RUN Put / Delete in layout txs.
    pub var meta_kv_block_map_puts: counters.ShardedCounter = .{};
    pub var meta_kv_block_map_run_puts: counters.ShardedCounter = .{};
    pub var meta_kv_block_map_deletes: counters.ShardedCounter = .{};
    /// The lookup split: exact / floor / range ops, and the records range
    /// pages returned (`records ÷ publishes` growing with file size = the
    /// whole-map diff running where a window should — finding 46).
    pub var meta_kv_block_map_lookup_exact: counters.ShardedCounter = .{};
    pub var meta_kv_block_map_lookup_floor: counters.ShardedCounter = .{};
    pub var meta_kv_block_map_lookup_range: counters.ShardedCounter = .{};
    pub var meta_kv_block_map_range_records: counters.ShardedCounter = .{};
    /// Key + value bytes staged by kvmap saves.
    pub var publish_map_record_bytes: counters.ShardedCounter = .{};
    /// Crossings completed (the flip landed) / ops the trains staged /
    /// crossings whose residue diff met records of an interrupted train.
    pub var kvmap_crossings: counters.ShardedCounter = .{};
    pub var map_migrate_records: counters.ShardedCounter = .{};
    pub var map_migrate_resumed: counters.ShardedCounter = .{};
    /// Window merges on a tree-mapped head (the streaming row's publishes
    /// must account here).
    pub var kvmap_window_saves: counters.ShardedCounter = .{};
    /// Sweeps planted (truncate handoffs + corpse marks) / chunk txs /
    /// records removed or shortened / sweeps regenerated at mount.
    pub var map_sweep_jobs: counters.ShardedCounter = .{};
    pub var map_sweep_chunks: counters.ShardedCounter = .{};
    pub var map_sweep_records: counters.ShardedCounter = .{};
    pub var map_sweep_resumed: counters.ShardedCounter = .{};
    /// Corpses (unlinked tree-mapped files) the sweep's terminal chunk destroyed.
    pub var kvmap_corpses_destroyed: counters.ShardedCounter = .{};
    /// The window store: resident bytes / its budget, fills, hits, LRU
    /// evictions, Red sheds.
    pub var block_map_window_bytes: counters.Gauge = .{};
    pub var kvmap_write_map_budget_bytes: counters.Gauge = .{};
    pub var block_map_window_fills: counters.ShardedCounter = .{};
    pub var block_map_window_hits: counters.ShardedCounter = .{};
    pub var block_map_window_evictions: counters.ShardedCounter = .{};
    pub var kvmap_windows_shed: counters.ShardedCounter = .{};
    /// fsck C11 (report-only): orphan records (must stay 0), heads with a
    /// size and no records, points on a FOREIGN volume inside a run's span.
    pub var fsck_map_orphan_records: counters.Gauge = .{};
    pub var fsck_map_empty_heads: counters.Gauge = .{};
    pub var fsck_map_run_foreign_shadows: counters.Gauge = .{};
    /// A record inside one ino's key range decoding to another owner — the
    /// range bounds are exact, so this cannot happen.
    pub var owner_mismatch: counters.Tripwire = .init("kvmap_owner_mismatch");
    /// A window put displaced more bindings than `MergeOutcome` holds (the
    /// excess would never be freed): one displaced per unique insert, at
    /// most `max_displaced` inserts per merge — must stay 0.
    pub var merge_displaced_overflow: counters.Tripwire = .init("kvmap_merge_displaced_overflow");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "meta_kv_block_map_puts", "meta_kv_block_map_run_puts", "meta_kv_block_map_deletes", "meta_kv_block_map_lookup_exact", "meta_kv_block_map_lookup_floor", "meta_kv_block_map_lookup_range", "meta_kv_block_map_range_records", "publish_map_record_bytes", "kvmap_crossings", "map_migrate_records", "map_migrate_resumed", "kvmap_window_saves", "map_sweep_jobs", "map_sweep_chunks", "map_sweep_records", "map_sweep_resumed", "kvmap_corpses_destroyed", "block_map_window_bytes", "kvmap_write_map_budget_bytes", "block_map_window_fills", "block_map_window_hits", "block_map_window_evictions", "kvmap_windows_shed", "fsck_map_orphan_records", "fsck_map_empty_heads", "fsck_map_run_foreign_shadows" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"meta_kv_block_map_leaf_reads\":{d},\"kvmap_owner_mismatch\":{d},\"kvmap_merge_displaced_overflow\":{d}}}", .{ node_cache.stats.meta_kv_block_map_leaf_reads.load(), counters.tripwireCount("kvmap_owner_mismatch"), counters.tripwireCount("kvmap_merge_displaced_overflow") });
}

// ---------------------------------------------------------------------------
// The store face.
// ---------------------------------------------------------------------------

pub const Error = error{ OutOfMemory, Io, Corrupt };

/// What the map tree needs of a store: the inclusive range scan and the
/// point lookup over kind 7 — the kv_fs Store shape, bound as a vtable so
/// the portable contracts run over a RAM store.
pub const Store = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        range: *const fn (*anyopaque, Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void,
        lookup: *const fn (*anyopaque, Allocator, k: []const u8) Error!?[]u8,
    };

    pub fn from(comptime S: type, s: *S) Store {
        const Adapter = struct {
            fn rangeFn(ctx: *anyopaque, gpa: Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void {
                const st: *S = @ptrCast(@alignCast(ctx));
                st.range(gpa, key.TREE_BLOCK_MAP, start, end, max, out) catch |e| return mapStoreError(e);
            }
            fn lookupFn(ctx: *anyopaque, gpa: Allocator, k: []const u8) Error!?[]u8 {
                const st: *S = @ptrCast(@alignCast(ctx));
                return st.lookup(gpa, key.TREE_BLOCK_MAP, k) catch |e| return mapStoreError(e);
            }
            const vtable: VTable = .{ .range = &rangeFn, .lookup = &lookupFn };
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
};

// ---------------------------------------------------------------------------
// The record: one value form in RAM over the five wire kinds.
// ---------------------------------------------------------------------------

/// A kind-7 record decoded: `[index, index + len)` bound to consecutive
/// device blocks of `vol_tag` from `offset` (stride = the chunk) with
/// consecutive stamps from `incarnation` (0 = unstamped: the POINT/RUN
/// forms). `special` carries a non-whole-block binding verbatim (a DAMAGED
/// quarantine marker, a tenant window) — the port's kind 2 is the 40 B
/// `MappingWire`, never a string (0002 Q2); it is always a single index.
pub const Rec = struct {
    index: u32,
    len: u32,
    vol_tag: u64,
    offset: u64,
    incarnation: u64,
    special: ?Mapping = null,

    /// A whole-block binding at one index.
    pub fn point(index: u32, m: Mapping) Rec {
        if (!m.whole_block or m.damaged) return .{ .index = index, .len = 1, .vol_tag = m.vol_tag, .offset = m.offset, .incarnation = m.incarnation, .special = m };
        return .{ .index = index, .len = 1, .vol_tag = m.vol_tag, .offset = m.offset, .incarnation = m.incarnation };
    }

    /// Exclusive end index.
    pub fn end(self: Rec) u64 {
        return @as(u64, self.index) + self.len;
    }

    pub fn covers(self: Rec, block: u32) bool {
        return self.index <= block and block < self.end();
    }

    /// The per-index binding (`offset = start + stride × delta`, `stamp =
    /// start + delta`).
    pub fn bindingAt(self: Rec, block: u32, chunk: u64) Mapping {
        assert(self.covers(block));
        if (self.special) |m| return m;
        const delta: u64 = block - self.index;
        return Mapping.wholeBlock(self.vol_tag, self.offset + delta * chunk, if (self.incarnation == 0) 0 else self.incarnation + delta);
    }

    /// Can `next` extend this record into one run: same volume, the next
    /// index, the next device block, the next stamp (or both unstamped),
    /// whole-block bindings only, and the span stays within `RUN_LEN_MAX`.
    pub fn extends(self: Rec, next: Rec, chunk: u64) bool {
        if (self.special != null or next.special != null) return false;
        if (self.vol_tag != next.vol_tag or next.index != self.end()) return false;
        if (next.offset != self.offset + @as(u64, self.len) * chunk) return false;
        if (self.incarnation == 0) {
            if (next.incarnation != 0) return false;
        } else if (next.incarnation != self.incarnation + self.len) return false;
        return @as(u64, self.len) + next.len <= RUN_LEN_MAX;
    }

    pub fn eql(a: Rec, b: Rec) bool {
        if (a.index != b.index or a.len != b.len or a.vol_tag != b.vol_tag or a.offset != b.offset or a.incarnation != b.incarnation) return false;
        if ((a.special == null) != (b.special == null)) return false;
        if (a.special) |x| return x.eql(b.special.?);
        return true;
    }

    /// The wire form; `buf` backs the kind-2 payload.
    pub fn entryInto(self: Rec, buf: *[layout.MAPPING_WIRE_LEN]u8) record.MapEntry {
        if (self.special) |m| {
            assert(self.len == 1);
            buf.* = std.mem.toBytes(m.encode());
            return .{ .string = buf };
        }
        if (self.len == 1) {
            if (self.incarnation == 0) return .{ .point = .{ .vol_tag = self.vol_tag, .offset = self.offset } };
            return .{ .point_stamped = .{ .vol_tag = self.vol_tag, .offset = self.offset, .incarnation = self.incarnation } };
        }
        if (self.incarnation == 0) return .{ .run = .{ .vol_tag = self.vol_tag, .start_offset = self.offset, .len = self.len } };
        return .{ .run_stamped = .{ .vol_tag = self.vol_tag, .start_offset = self.offset, .len = self.len, .start_incarnation = self.incarnation } };
    }

    pub fn encodedLen(self: Rec) usize {
        var buf: [layout.MAPPING_WIRE_LEN]u8 = undefined;
        return self.entryInto(&buf).encodedLen();
    }

    /// From a decoded value at `index`; a kind-2 payload must be exactly the
    /// 40 B wire and must not spell a plain whole-block binding (one
    /// spelling per mapping — a second one churns the record diff).
    pub fn fromEntry(index: u32, e: record.MapEntry, chunk: u64) Error!Rec {
        return switch (e) {
            .point => |p| .{ .index = index, .len = 1, .vol_tag = p.vol_tag, .offset = p.offset, .incarnation = 0 },
            .point_stamped => |p| .{ .index = index, .len = 1, .vol_tag = p.vol_tag, .offset = p.offset, .incarnation = p.incarnation },
            .run => |r| .{ .index = index, .len = r.len, .vol_tag = r.vol_tag, .offset = r.start_offset, .incarnation = 0 },
            .run_stamped => |r| .{ .index = index, .len = r.len, .vol_tag = r.vol_tag, .offset = r.start_offset, .incarnation = r.start_incarnation },
            .string => |s| blk: {
                if (s.len != layout.MAPPING_WIRE_LEN) return error.Corrupt;
                const m = Mapping.decode(std.mem.bytesToValue(layout.MappingWire, s[0..layout.MAPPING_WIRE_LEN]), chunk) catch return error.Corrupt;
                if (m.whole_block and !m.damaged) return error.Corrupt;
                break :blk .{ .index = index, .len = 1, .vol_tag = m.vol_tag, .offset = m.offset, .incarnation = m.incarnation, .special = m };
            },
        };
    }

    pub fn isRun(self: Rec) bool {
        return self.len >= RUN_LEN_MIN;
    }

    /// The record shortened to `new_len` indices from its start (the sweep's
    /// coverage-exact re-Put; a 1-run collapses to the point form).
    pub fn shortened(self: Rec, new_len: u32) Rec {
        assert(new_len >= 1 and new_len < self.len);
        var r = self;
        r.len = new_len;
        return r;
    }
};

fn decodeRecord(ino: u64, kv: tree.KeyValue, chunk: u64) Error!Rec {
    const k = key.BlockMapKey.decode(kv.key) catch return error.Corrupt;
    if (k.ownerIno() != ino) {
        stats.owner_mismatch.fire("a kind-7 record inside one ino's exact range names another owner");
        return error.Corrupt;
    }
    const e = record.MapEntry.decode(kv.value) catch return error.Corrupt;
    return Rec.fromEntry(k.blockIndex(), e, chunk);
}

/// Merge adjacent extendable records of an ascending, non-overlapping list
/// into runs (in place; returns the surviving length).
pub fn canonicalize(recs: []Rec, chunk: u64) usize {
    if (recs.len == 0) return 0;
    var n: usize = 1;
    for (recs[1..]) |r| {
        if (recs[n - 1].extends(r, chunk)) {
            recs[n - 1].len += r.len;
        } else {
            recs[n] = r;
            n += 1;
        }
    }
    return n;
}

// ---------------------------------------------------------------------------
// §1.3–§1.5 The read law, the floor lookup, range iteration.
// ---------------------------------------------------------------------------

/// Is `k` a kind-7 key of `ino`'s range (the tree interleaves every ino-
/// major kind, so a cross-ino probe may meet foreign records)?
fn isMapKeyOf(ino: u64, k: []const u8) bool {
    if (k.len != key.BLOCK_MAP_KEY_LEN or k[key.INO_MAJOR_KIND_OFF] != key.TREE_BLOCK_MAP) return false;
    return std.mem.readInt(u64, k[0..8], .big) == ino;
}

/// The record keyed exactly at `block`.
pub fn lookupExact(store: Store, gpa: Allocator, ino: u64, block: u32, chunk: u64) Error!?Rec {
    stats.meta_kv_block_map_lookup_exact.inc();
    const k = key.blockMapKey(ino, block) catch return error.Corrupt;
    const v = (try store.vtable.lookup(store.ctx, gpa, &k)) orelse return null;
    defer gpa.free(v);
    const e = record.MapEntry.decode(v) catch return error.Corrupt;
    return try Rec.fromEntry(block, e, chunk);
}

/// The floor probe: ONE bounded forward scan of `[block − (RUN_LEN_MAX −
/// 1), block − 1]`, paged, keeping the LAST record that COVERS `block` —
/// not the last record plain: a superseding point inside a run's span (the
/// read law's own legal shape) sits between the run and `block`, and taking
/// it would read a covered index as absent. `RUN_LEN_MAX` bounds the scan
/// because it bounds every emitted run. Null for `block == 0`.
pub fn lookupFloor(store: Store, gpa: Allocator, ino: u64, block: u32, chunk: u64) Error!?Rec {
    if (block == 0) return null;
    stats.meta_kv_block_map_lookup_floor.inc();
    const lo: u32 = block -| (RUN_LEN_MAX - 1);
    var cursor = key.blockMapKey(ino, lo) catch return error.Corrupt;
    const end = key.blockMapKey(ino, block - 1) catch return error.Corrupt;
    var best: ?Rec = null;
    while (true) {
        var page: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &page);
        try store.vtable.range(store.ctx, gpa, &cursor, &end, SCAN_PAGE, &page);
        stats.meta_kv_block_map_range_records.add(page.items.len);
        if (page.items.len == 0) break;
        for (page.items) |kv| {
            const r = try decodeRecord(ino, kv, chunk);
            if (r.covers(block)) best = r;
        }
        if (page.items.len < SCAN_PAGE) break;
        const last = page.items[page.items.len - 1].key;
        const last_index = (key.BlockMapKey.decode(last) catch return error.Corrupt).blockIndex();
        if (last_index >= block - 1) break;
        cursor = key.blockMapKey(ino, last_index + 1) catch return error.Corrupt;
    }
    return best;
}

/// The record that resolves `block`: exact first, else the covering run.
pub fn lookupRecord(store: Store, gpa: Allocator, ino: u64, block: u32, chunk: u64) Error!?Rec {
    if (try lookupExact(store, gpa, ino, block, chunk)) |r| return r;
    return lookupFloor(store, gpa, ino, block, chunk);
}

/// `get_block_mapping`: the binding of `block` (null = hole).
pub fn lookup(store: Store, gpa: Allocator, ino: u64, block: u32, chunk: u64) Error!?Mapping {
    const r = (try lookupRecord(store, gpa, ino, block, chunk)) orelse return null;
    return r.bindingAt(block, chunk);
}

/// Record-true: up to `max` records keyed in `[from, to]` appended to
/// `out` (no prepend — the sweep / diff / C11 paging loops rely on it).
pub fn rangeBounded(store: Store, gpa: Allocator, ino: u64, from: u32, to: u32, max: usize, chunk: u64, out: *std.ArrayList(Rec)) Error!void {
    if (max == 0 or to < from) return;
    stats.meta_kv_block_map_lookup_range.inc();
    const lo = key.blockMapKey(ino, from) catch return error.Corrupt;
    // The reserved index as an inclusive END bound is exact (no record carries it).
    const hi = if (to == key.BLOCK_INDEX_MAP_BLOB) key.indexRangeFrom(ino, from).hi else key.blockMapKey(ino, to) catch return error.Corrupt;
    var page: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &page);
    try store.vtable.range(store.ctx, gpa, &lo, &hi, max, &page);
    stats.meta_kv_block_map_range_records.add(page.items.len);
    try out.ensureUnusedCapacity(gpa, page.items.len);
    for (page.items) |kv| out.appendAssumeCapacity(try decodeRecord(ino, kv, chunk));
}

/// Records keyed at or above `from`, up to `max`.
pub fn rangeFrom(store: Store, gpa: Allocator, ino: u64, from: u32, max: usize, chunk: u64, out: *std.ArrayList(Rec)) Error!void {
    return rangeBounded(store, gpa, ino, from, key.BLOCK_INDEX_MAP_BLOB, max, chunk, out);
}

/// The routed range: the run covering `from` from strictly before it is
/// PREPENDED verbatim (record-true: no clipped synthetic record exists),
/// then the records keyed at or above `from`.
pub fn rangeCovering(store: Store, gpa: Allocator, ino: u64, from: u32, max: usize, chunk: u64, out: *std.ArrayList(Rec)) Error!void {
    if (try lookupFloor(store, gpa, ino, from, chunk)) |r| {
        if (r.covers(from)) try out.append(gpa, r);
    }
    try rangeFrom(store, gpa, ino, from, max, chunk, out);
}

// ---------------------------------------------------------------------------
// §3 The window put — a publish ships ONLY its window.
// ---------------------------------------------------------------------------

/// The window's neighbourhood re-canonicalized: the inserts (ascending,
/// unique), the record resolving `first − 1` and every record keyed in
/// `[first, last + 1]` are expanded over their indices, the inserts
/// overlaid, the span re-emitted as POINT/RUN records; the diff of the old
/// records against the new is staged (Puts for new or changed keys, Deletes
/// for keys that vanished), and the references move for exactly the
/// inserted indices whose binding changed (`+new`, `−old`; an idempotent
/// re-bind of the same block moves nothing). Displaced bindings land in
/// `out` for the caller's post-commit frees (RES-1). Tree ops: one exact
/// (+ at most one bounded floor scan) for the left neighbour, one range of
/// `window + 2` records — never a function of the file.
pub fn stageWindow(store: Store, gpa: Allocator, t: *KvTx, ino: u64, inserts: []const seams.MapInsert, chunk: u64, out: *seams.MergeOutcome) Error!void {
    if (inserts.len == 0) return;
    for (inserts[1..], 0..) |ins, i| assert(ins.block > inserts[i].block);
    const first = inserts[0].block;
    const last = inserts[inserts.len - 1].block;
    var old: std.ArrayList(Rec) = .empty;
    defer old.deinit(gpa);
    if (first > 0) {
        if (try lookupRecord(store, gpa, ino, first - 1, chunk)) |r| try old.append(gpa, r);
    }
    const right: u32 = if (last == key.BLOCK_INDEX_MAP_BLOB - 1) last else last + 1;
    try rangeBounded(store, gpa, ino, first, right, @as(usize, right - first) + 1, chunk, &old);
    // The expanded span.
    var span_lo: u32 = first;
    var span_hi: u64 = @as(u64, right) + 1;
    for (old.items) |r| {
        span_lo = @min(span_lo, r.index);
        span_hi = @max(span_hi, r.end());
    }
    const n: usize = @intCast(span_hi - span_lo);
    const cells = try gpa.alloc(?Mapping, n);
    defer gpa.free(cells);
    @memset(cells, null);
    for (old.items) |r| {
        var b: u32 = r.index;
        while (b < r.end()) : (b += 1) cells[b - span_lo] = r.bindingAt(b, chunk);
    }
    // Overlay the inserts; the reference moves where the binding changed.
    var ref_ops: std.ArrayList(block_refs.RefOp) = .empty;
    defer ref_ops.deinit(gpa);
    for (inserts) |ins| {
        const cell = &cells[ins.block - span_lo];
        if (cell.*) |old_b| {
            if (old_b.sameRef(ins.binding, chunk)) {
                cell.* = ins.binding;
                continue;
            }
            try ref_ops.append(gpa, block_refs.released(refOf(ino, ins.block, old_b, chunk)));
            // One displaced per unique insert, at most `max_displaced`
            // inserts per merge (`MergeRequest.inserts`): the else cannot run.
            if (out.n_displaced < seams.MergeOutcome.max_displaced) {
                out.displaced[out.n_displaced] = old_b;
                out.n_displaced += 1;
            } else stats.merge_displaced_overflow.fire("a window put displaced more bindings than MergeOutcome holds; the excess is never freed");
        }
        try ref_ops.append(gpa, block_refs.taken(refOf(ino, ins.block, ins.binding, chunk)));
        cell.* = ins.binding;
    }
    // Re-canonicalize.
    var fresh: std.ArrayList(Rec) = .empty;
    defer fresh.deinit(gpa);
    try fresh.ensureTotalCapacity(gpa, n);
    for (cells, 0..) |c, i| {
        if (c) |m| fresh.appendAssumeCapacity(Rec.point(@intCast(span_lo + i), m));
    }
    fresh.items.len = canonicalize(fresh.items, chunk);
    try stageDiff(t, ino, old.items, fresh.items);
    try block_refs.stageOps(t, ref_ops.items);
}

fn refOf(ino: u64, block: u32, m: Mapping, chunk: u64) block_refs.Reference {
    return .{ .vol_tag = m.vol_tag, .block_idx = m.blockIdx(chunk), .owner_ino = ino, .block_index = block };
}

/// Stage `fresh` over `old` (both ascending by index): a Put for every
/// record absent from or different in `old`, a Delete for every old key
/// `fresh` no longer carries. Puts lead, Deletes last (the crash-order
/// law — every intermediate state keeps each index bound).
fn stageDiff(t: *KvTx, ino: u64, old: []const Rec, fresh: []const Rec) Error!void {
    for (fresh) |r| {
        const o = findIndex(old, r.index);
        if (o != null and o.?.eql(r)) continue;
        try stagePut(t, ino, r);
    }
    for (old) |o| {
        if (findIndex(fresh, o.index) == null) try stageDelete(t, ino, o.index);
    }
}

fn findIndex(recs: []const Rec, index: u32) ?Rec {
    for (recs) |r| if (r.index == index) return r;
    return null;
}

/// Stage one record Put (`meta_kv_block_map_puts`, `_run_puts`,
/// `publish_map_record_bytes`).
pub fn stagePut(t: *KvTx, ino: u64, r: Rec) Error!void {
    const k = key.blockMapKey(ino, r.index) catch return error.Corrupt;
    var wire: [layout.MAPPING_WIRE_LEN]u8 = undefined;
    const e = r.entryInto(&wire);
    var buf: [record.BLOCK_MAP_RUN_STAMPED_VALUE_LEN + layout.MAPPING_WIRE_LEN]u8 = undefined;
    const v = e.encodeInto(&buf);
    try t.stagePut(key.TREE_BLOCK_MAP, &k, v);
    stats.meta_kv_block_map_puts.inc();
    if (r.isRun()) stats.meta_kv_block_map_run_puts.inc();
    stats.publish_map_record_bytes.add(k.len + v.len);
}

/// Stage one record Delete by exact key.
pub fn stageDelete(t: *KvTx, ino: u64, index: u32) Error!void {
    const k = key.blockMapKey(ino, index) catch return error.Corrupt;
    try t.stageDelete(key.TREE_BLOCK_MAP, &k);
    stats.meta_kv_block_map_deletes.inc();
    stats.publish_map_record_bytes.add(k.len);
}

// ---------------------------------------------------------------------------
// §2 The crossing plan — the A1 residue DIFF.
// ---------------------------------------------------------------------------

pub const Op = union(enum) {
    put: Rec,
    delete: u32,
};

pub const CrossingPlan = struct {
    /// Puts lead, stale Deletes last.
    ops: []Op,
    /// Records the diff met before the flip: an interrupted earlier train's
    /// residue (a healthy first crossing scans one empty descent).
    preexisting: u32,
    /// The canonical records the inline map became.
    records: u32,

    pub fn deinit(self: *CrossingPlan, gpa: Allocator) void {
        gpa.free(self.ops);
        self.* = undefined;
    }
};

/// The desired tree for an inline map (`entries` ascending) diffed against
/// every record the ino already holds: a changed binding is ONE same-key
/// Put; a record no desired key names is a Delete — a crashed prior
/// crossing's residue would otherwise resurrect as a stale mapping after
/// truncate-shrink → re-cross → extend. The whole existing population is
/// paged (`SCAN_PAGE`), never materialized beyond the diff's needs.
pub fn planCrossing(store: Store, gpa: Allocator, ino: u64, entries: []const layout.MapEntryWire, chunk: u64) Error!CrossingPlan {
    var desired: std.ArrayList(Rec) = .empty;
    defer desired.deinit(gpa);
    try desired.ensureTotalCapacity(gpa, entries.len);
    for (entries) |*e| {
        const m = Mapping.decode(e.mapping, chunk) catch return error.Corrupt;
        desired.appendAssumeCapacity(Rec.point(e.block(), m));
    }
    desired.items.len = canonicalize(desired.items, chunk);
    var existing: std.ArrayList(Rec) = .empty;
    defer existing.deinit(gpa);
    var from: u32 = 0;
    while (true) {
        const before = existing.items.len;
        try rangeFrom(store, gpa, ino, from, SCAN_PAGE, chunk, &existing);
        const got = existing.items.len - before;
        if (got < SCAN_PAGE) break;
        const last = existing.items[existing.items.len - 1].index;
        if (last >= key.BLOCK_INDEX_MAP_BLOB - 1) break;
        from = last + 1;
    }
    var ops: std.ArrayList(Op) = .empty;
    errdefer ops.deinit(gpa);
    for (desired.items) |r| {
        const o = findIndex(existing.items, r.index);
        if (o != null and o.?.eql(r)) continue;
        try ops.append(gpa, .{ .put = r });
    }
    for (existing.items) |o| {
        if (findIndex(desired.items, o.index) == null) try ops.append(gpa, .{ .delete = o.index });
    }
    return .{ .ops = try ops.toOwnedSlice(gpa), .preexisting = @intCast(existing.items.len), .records = @intCast(desired.items.len) };
}

pub fn stageOp(t: *KvTx, ino: u64, op: Op) Error!void {
    switch (op) {
        .put => |r| try stagePut(t, ino, r),
        .delete => |i| try stageDelete(t, ino, i),
    }
}

// ---------------------------------------------------------------------------
// §4.3 The sweep page.
// ---------------------------------------------------------------------------

/// One freed index: the −ref's `(block_index, binding)`.
pub const Freed = seams.MapInsert;

pub const SweepPage = struct {
    /// Records deleted (exact keys) and re-Put shortened (coverage-exact).
    deletes: std.ArrayList(u32) = .empty,
    puts: std.ArrayList(Rec) = .empty,
    /// Every index whose binding this page releases.
    freed: std.ArrayList(Freed) = .empty,
    /// Where the next chunk resumes; null = nothing at or above `start`
    /// remains (the terminal chunk).
    next_cursor: ?u32 = null,
    /// Covered indices consumed (the budget's unit).
    indices: u32 = 0,

    pub fn deinit(self: *SweepPage, gpa: Allocator) void {
        self.deletes.deinit(gpa);
        self.puts.deinit(gpa);
        self.freed.deinit(gpa);
        self.* = undefined;
    }

    pub fn recordOps(self: *const SweepPage) u32 {
        return @intCast(self.deletes.items.len + self.puts.items.len);
    }

    pub fn isEmpty(self: *const SweepPage) bool {
        return self.recordOps() == 0;
    }
};

/// Plan ONE sweep chunk over the records keyed in `[start, end)` under a
/// budget of covered INDICES (a 4,096-index run's one-shot release measured
/// ~178 KiB against the 128 KiB whole-entry cap, so records are not the
/// unit): the left-boundary STRADDLER first (a run keyed below `start`
/// covering it keeps its head `[s, start)` and its tail is consumed
/// right-to-left), then the records keyed at or above `start` in order; a
/// run the budget cannot finish is re-Put shortened at its own key; an
/// index inside a run that has its own superseding record is never freed
/// through the run (its own record frees it when processed); the resume
/// cursor is the first unprocessed record's own key; `next_cursor == null`
/// = nothing keyed in `[start, end)` remains (`end = u32::MAX` = the whole
/// tail — the terminal chunk).
pub fn planSweepPage(store: Store, gpa: Allocator, ino: u64, start: u32, end: u32, budget_indices: u32, chunk: u64) Error!SweepPage {
    var page: SweepPage = .{};
    errdefer page.deinit(gpa);
    var left: u32 = budget_indices;
    // 1. The straddler.
    if (try lookupFloor(store, gpa, ino, start, chunk)) |r| {
        if (r.covers(start)) {
            const tail: u32 = @intCast(r.end() - start);
            const take = @min(tail, left);
            const a: u32 = @intCast(r.end() - take);
            try consumeSpan(store, gpa, ino, r, a, @intCast(r.end()), chunk, &page);
            try page.puts.append(gpa, r.shortened(@intCast(a - r.index)));
            left -= take;
            if (take < tail or left == 0) {
                // The straddler still straddles, or the budget is spent
                // before any record at/above `start` was seen.
                page.next_cursor = start;
                return page;
            }
        }
    }
    // 2. The records at or above `start`: every record covers ≥ 1 index,
    //    so `left + 1` records decide whether anything remains past the
    //    budget.
    var recs: std.ArrayList(Rec) = .empty;
    defer recs.deinit(gpa);
    const asked: usize = @as(usize, left) + 1;
    if (end > start) try rangeBounded(store, gpa, ino, start, end - 1, asked, chunk, &recs);
    for (recs.items) |r| {
        if (left == 0) {
            page.next_cursor = r.index;
            return page;
        }
        if (r.len <= left) {
            try consumeSpan(store, gpa, ino, r, r.index, @intCast(r.end()), chunk, &page);
            try page.deletes.append(gpa, r.index);
            left -= r.len;
            continue;
        }
        // A run the budget cannot finish: its tail, shortened at its key.
        const a: u32 = @intCast(r.end() - left);
        try consumeSpan(store, gpa, ino, r, a, @intCast(r.end()), chunk, &page);
        try page.puts.append(gpa, r.shortened(a - r.index));
        page.next_cursor = r.index;
        return page;
    }
    // Fewer records than asked were there: nothing remains at or above
    // `start` once these are gone — the terminal page.
    assert(recs.items.len < asked);
    page.next_cursor = null;
    return page;
}

/// Free the indices `[a, b)` of `r` except those a superseding record of
/// their own covers (one bounded range over the span).
fn consumeSpan(store: Store, gpa: Allocator, ino: u64, r: Rec, a: u32, b: u32, chunk: u64, page: *SweepPage) Error!void {
    assert(r.index <= a and b <= r.end());
    page.indices += b - a;
    var own: std.ArrayList(Rec) = .empty;
    defer own.deinit(gpa);
    if (r.isRun() and b > a) {
        // Records keyed strictly inside the run's span supersede it.
        const lo = @max(a, r.index + 1);
        if (lo < b) try rangeBounded(store, gpa, ino, lo, b - 1, @as(usize, b - lo), chunk, &own);
    }
    var i: u32 = a;
    while (i < b) : (i += 1) {
        if (findIndex(own.items, i) != null) continue;
        try page.freed.append(gpa, .{ .block = i, .binding = r.bindingAt(i, chunk) });
    }
}

/// Stage a planned page's record ops and the `−ref` of every freed index.
pub fn stageSweepPage(t: *KvTx, ino: u64, page: *const SweepPage, chunk: u64) Error!void {
    for (page.puts.items) |r| try stagePut(t, ino, r);
    for (page.deletes.items) |i| try stageDelete(t, ino, i);
    for (page.freed.items) |f| try block_refs.stageOps(t, &.{block_refs.released(refOf(ino, f.block, f.binding, chunk))});
    stats.map_sweep_records.add(page.recordOps());
}

// ---------------------------------------------------------------------------
// §5.3 fsck C11 (report-only): the record walk.
// ---------------------------------------------------------------------------

pub const VerifyReport = struct {
    records: u64 = 0,
    /// Expanded indices the records cover (runs counted by length).
    indices: u64 = 0,
    /// Points on a DIFFERENT volume strictly inside a covering run's span
    /// (same-volume points inside runs are the read law's legal shape).
    run_foreign_shadows: u64 = 0,
};

/// Walk every record of `ino` (paged) and judge arm (c); the caller judges
/// (b) from `records == 0 ∧ size > 0` off the head.
pub fn verifyBlockMap(store: Store, gpa: Allocator, ino: u64, chunk: u64) Error!VerifyReport {
    var rep: VerifyReport = .{};
    var from: u32 = 0;
    var covering: ?Rec = null;
    while (true) {
        var recs: std.ArrayList(Rec) = .empty;
        defer recs.deinit(gpa);
        try rangeFrom(store, gpa, ino, from, SCAN_PAGE, chunk, &recs);
        for (recs.items) |r| {
            rep.records += 1;
            rep.indices += r.len;
            if (covering) |c| {
                if (c.covers(r.index) and r.index > c.index and !r.isRun() and r.vol_tag != c.vol_tag) rep.run_foreign_shadows += 1;
            }
            if (r.isRun()) covering = r;
        }
        if (recs.items.len < SCAN_PAGE) break;
        const last = recs.items[recs.items.len - 1].index;
        if (last >= key.BLOCK_INDEX_MAP_BLOB - 1) break;
        from = last + 1;
    }
    return rep;
}

/// The owner skip-scan: the first ino ≥ `from` with any kind-7 record (null
/// when none) — one bounded probe per distinct owner, then the jump to
/// `(owner + 1, 0)`; a probe landing on a foreign kind jumps past it. The
/// whole ino-major space: the native locals, then every guest keyspace
/// (under the slot plane every file mints into a guest slot — the native
/// slot holds the control records and `/`'s dentries alone).
pub fn nextOwner(store: Store, gpa: Allocator, from: u64) Error!?u64 {
    var ino = from;
    const end = key.indexRangeFrom(std.math.maxInt(u64), 0).hi;
    while (true) {
        const r = key.indexRangeFrom(ino, 0);
        var page: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &page);
        try store.vtable.range(store.ctx, gpa, &r.lo, &end, 1, &page);
        if (page.items.len == 0) return null;
        const k = page.items[0].key;
        if (k.len <= key.INO_MAJOR_KIND_OFF) return error.Corrupt;
        const k_ino = std.mem.readInt(u64, k[0..8], .big);
        if (isMapKeyOf(k_ino, k)) return k_ino;
        const kind = k[key.INO_MAJOR_KIND_OFF];
        if (kind >= key.TREE_BLOCK_MAP and k_ino == std.math.maxInt(u64)) return null;
        ino = if (kind < key.TREE_BLOCK_MAP) k_ino else k_ino + 1;
    }
}

/// Every binding a tree-mapped ino names (the C8 oracle's derived half):
/// every record's indices, residue above a sweep cursor INCLUDED (its
/// references are released only as the sweep deletes it).
pub fn deriveBindings(store: Store, gpa: Allocator, ino: u64, chunk: u64, out: *std.ArrayList(block_refs.Binding)) Error!void {
    var from: u32 = 0;
    while (true) {
        var recs: std.ArrayList(Rec) = .empty;
        defer recs.deinit(gpa);
        try rangeFrom(store, gpa, ino, from, SCAN_PAGE, chunk, &recs);
        for (recs.items) |r| {
            var b: u32 = r.index;
            while (b < r.end()) : (b += 1) {
                const m = r.bindingAt(b, chunk);
                try out.append(gpa, .{ .block_index = b, .vol_tag = m.vol_tag, .block_idx = m.blockIdx(chunk) });
            }
        }
        if (recs.items.len < SCAN_PAGE) break;
        const last = recs.items[recs.items.len - 1].index;
        if (last >= key.BLOCK_INDEX_MAP_BLOB - 1) break;
        from = last + 1;
    }
}

// ---------------------------------------------------------------------------
// §3.2 The window store — the read side's partial store.
// ---------------------------------------------------------------------------

pub const Probe = union(enum) {
    /// Inside a filled span, no record covers the index: a definitive hole.
    hole,
    mapping: Mapping,
};

/// A direct-mapped per-ino table of warm spans (records of one range page,
/// expanded on probe), charged to the budget, LRU within a row and across
/// rows, dropped whole on a local mutation of the ino and on a Red shed.
pub const WindowCache = struct {
    gpa: Allocator,
    rows: []Row,
    budget_bytes: u64,
    chunk: u64,
    tick: std.atomic.Value(u64) = .init(1),

    const Span = struct {
        lo: u32,
        /// Inclusive; `u32::MAX` = the tree is exhausted above `lo`.
        hi: u32,
        last_use: u64,
        recs: []Rec,

        fn bytes(self: Span) u64 {
            return self.recs.len * @sizeOf(Rec);
        }
    };

    const Row = struct {
        lock: jc.SpinLock = .{},
        ino: u64 = 0,
        spans: [WINDOW_SPANS_PER_INO]?Span = @splat(null),
    };

    pub fn init(gpa: Allocator, budget_bytes: u64, chunk: u64) Allocator.Error!WindowCache {
        const rows = try gpa.alloc(Row, WINDOW_ROWS);
        @memset(rows, .{});
        stats.kvmap_write_map_budget_bytes.set(budget_bytes);
        return .{ .gpa = gpa, .rows = rows, .budget_bytes = budget_bytes, .chunk = chunk };
    }

    pub fn deinit(self: *WindowCache) void {
        for (self.rows) |*r| for (&r.spans) |*s| self.dropSpan(s);
        self.gpa.free(self.rows);
        self.* = undefined;
    }

    fn rowOf(self: *WindowCache, ino: u64) *Row {
        return &self.rows[@intCast(dlm.splitmix64(ino) & (WINDOW_ROWS - 1))];
    }

    fn dropSpan(self: *WindowCache, s: *?Span) void {
        const span = s.* orelse return;
        stats.block_map_window_bytes.sub(span.bytes());
        self.gpa.free(span.recs);
        s.* = null;
    }

    /// The span verdict for `block`, or null on a miss.
    pub fn probe(self: *WindowCache, ino: u64, block: u32) ?Probe {
        const row = self.rowOf(ino);
        row.lock.lock();
        defer row.lock.unlock();
        if (row.ino != ino) return null;
        for (&row.spans) |*s| {
            const span = &(s.* orelse continue);
            if (block < span.lo or block > span.hi) continue;
            span.last_use = self.tick.fetchAdd(1, .monotonic);
            stats.block_map_window_hits.inc();
            // The last record keyed ≤ block.
            var lo: usize = 0;
            var hi: usize = span.recs.len;
            while (lo < hi) {
                const mid = lo + (hi - lo) / 2;
                if (span.recs[mid].index <= block) lo = mid + 1 else hi = mid;
            }
            if (lo == 0) return .hole;
            const r = span.recs[lo - 1];
            if (r.covers(block)) return .{ .mapping = r.bindingAt(block, self.chunk) };
            return .hole;
        }
        return null;
    }

    /// Install the page `recs` (`rangeCovering(block, WINDOW_FILL_RECORDS)`
    /// — ascending, the first may be the covering run keyed below `block`)
    /// as a span of `ino` starting at `block`; `short` = the page returned
    /// fewer than it asked (the tree is exhausted above it). Clipped to
    /// `WINDOW_FILL_INDICES_MAX` expanded indices. Takes ownership of
    /// nothing: `recs` is copied.
    pub fn fill(self: *WindowCache, ino: u64, block: u32, recs: []const Rec, short: bool) Allocator.Error!void {
        var n: usize = recs.len;
        var hi: u32 = if (short) std.math.maxInt(u32) else 0;
        const limit: u64 = @as(u64, block) + WINDOW_FILL_INDICES_MAX;
        for (recs, 0..) |r, i| {
            if (r.index >= limit) {
                n = i;
                hi = @intCast(limit - 1);
                break;
            }
            if (!short) hi = @intCast(@min(r.end(), @as(u64, std.math.maxInt(u32)) + 1) - 1);
        }
        if (n == 0) hi = if (short) std.math.maxInt(u32) else block;
        const copy = try self.gpa.dupe(Rec, recs[0..n]);
        const span: Span = .{ .lo = block, .hi = @max(hi, block), .last_use = self.tick.fetchAdd(1, .monotonic), .recs = copy };
        const row = self.rowOf(ino);
        {
            row.lock.lock();
            defer row.lock.unlock();
            if (row.ino != ino) {
                for (&row.spans) |*s| self.dropSpan(s);
                row.ino = ino;
            }
            var victim: *?Span = &row.spans[0];
            for (&row.spans) |*s| {
                if (s.* == null) {
                    victim = s;
                    break;
                }
                if (s.*.?.last_use < victim.*.?.last_use) victim = s;
            }
            if (victim.* != null) stats.block_map_window_evictions.inc();
            self.dropSpan(victim);
            victim.* = span;
            stats.block_map_window_bytes.add(span.bytes());
        }
        stats.block_map_window_fills.inc();
        self.enforceBudget();
    }

    /// Evict the globally least-recently-used span while over budget.
    fn enforceBudget(self: *WindowCache) void {
        while (stats.block_map_window_bytes.load() > self.budget_bytes) {
            var best_row: ?*Row = null;
            var best_slot: usize = 0;
            var best_use: u64 = std.math.maxInt(u64);
            for (self.rows) |*r| {
                r.lock.lock();
                defer r.lock.unlock();
                for (&r.spans, 0..) |*s, i| if (s.*) |sp| if (sp.last_use < best_use) {
                    best_use = sp.last_use;
                    best_row = r;
                    best_slot = i;
                };
            }
            const r = best_row orelse return;
            r.lock.lock();
            defer r.lock.unlock();
            if (r.spans[best_slot]) |sp| if (sp.last_use == best_use) {
                self.dropSpan(&r.spans[best_slot]);
                stats.block_map_window_evictions.inc();
                continue;
            };
            return;
        }
    }

    /// A local mutation of `ino`'s map: its spans go.
    pub fn invalidate(self: *WindowCache, ino: u64) void {
        const row = self.rowOf(ino);
        row.lock.lock();
        defer row.lock.unlock();
        if (row.ino != ino) return;
        for (&row.spans) |*s| self.dropSpan(s);
    }

    /// Red: drop everything (the R5 `block_map_window` component's shed).
    pub fn shedAll(self: *WindowCache) void {
        for (self.rows) |*r| {
            r.lock.lock();
            defer r.lock.unlock();
            for (&r.spans) |*s| self.dropSpan(s);
        }
        stats.kvmap_windows_shed.inc();
    }

    fn shedFn(ctx: *anyopaque, _: u64) void {
        const self: *WindowCache = @ptrCast(@alignCast(ctx));
        self.shedAll();
    }

    pub fn component(self: *WindowCache) budget.Component {
        return budget.Component.init("block_map_window", 0, WINDOW_SHED_WEIGHT, .{ .gauge = &stats.block_map_window_bytes }, .{ .ctx = self, .call = &shedFn });
    }
};

// ---------------------------------------------------------------------------
// Tests — a RAM store with the kv_fs Store shape over (kind, key).
// ---------------------------------------------------------------------------

const testing = std.testing;

pub const TestStore = struct {
    gpa: Allocator,
    rows: std.ArrayList(Row) = .empty,
    pub const StoreError = error{ OutOfMemory, Io };
    const Row = struct { kind: u8, k: []u8, v: []u8 };

    pub fn init(gpa: Allocator) TestStore {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *TestStore) void {
        for (self.rows.items) |r| {
            self.gpa.free(r.k);
            self.gpa.free(r.v);
        }
        self.rows.deinit(self.gpa);
    }

    fn order(kind: u8, k: []const u8, r: Row) std.math.Order {
        if (kind != r.kind) return std.math.order(kind, r.kind);
        return key.order(k, r.k);
    }

    fn find(self: *TestStore, kind: u8, k: []const u8) struct { idx: usize, hit: bool } {
        var lo: usize = 0;
        var hi: usize = self.rows.items.len;
        while (lo < hi) {
            const mid = lo + (hi - lo) / 2;
            switch (order(kind, k, self.rows.items[mid])) {
                .lt => hi = mid,
                .gt => lo = mid + 1,
                .eq => return .{ .idx = mid, .hit = true },
            }
        }
        return .{ .idx = lo, .hit = false };
    }

    pub fn put(self: *TestStore, kind: u8, k: []const u8, v: []const u8) Allocator.Error!void {
        const f = self.find(kind, k);
        const vd = try self.gpa.dupe(u8, v);
        if (f.hit) {
            self.gpa.free(self.rows.items[f.idx].v);
            self.rows.items[f.idx].v = vd;
            return;
        }
        errdefer self.gpa.free(vd);
        const kd = try self.gpa.dupe(u8, k);
        errdefer self.gpa.free(kd);
        try self.rows.insert(self.gpa, f.idx, .{ .kind = kind, .k = kd, .v = vd });
    }

    pub fn remove(self: *TestStore, kind: u8, k: []const u8) void {
        const f = self.find(kind, k);
        if (!f.hit) return;
        const r = self.rows.orderedRemove(f.idx);
        self.gpa.free(r.k);
        self.gpa.free(r.v);
    }

    /// Apply a staged tx (Put / Delete) and free it.
    pub fn commit(self: *TestStore, t: *KvTx) StoreError!void {
        defer t.deinit();
        for (t.staged.items) |s| switch (s.kind) {
            .put => try self.put(s.tree_id, s.key, s.value),
            .delete => self.remove(s.tree_id, s.key),
            .delta => return error.Io,
        };
    }

    pub fn lookup(self: *TestStore, gpa: Allocator, kind: u8, k: []const u8) StoreError!?[]u8 {
        const f = self.find(kind, k);
        if (!f.hit) return null;
        return try gpa.dupe(u8, self.rows.items[f.idx].v);
    }

    pub fn range(self: *TestStore, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) StoreError!void {
        var i = self.find(kind, start).idx;
        var n: usize = 0;
        while (i < self.rows.items.len and n < max) : (i += 1) {
            const r = self.rows.items[i];
            if (r.kind != kind or key.lessThan(end, r.k)) break;
            try out.append(gpa, .{ .key = try gpa.dupe(u8, r.k), .value = try gpa.dupe(u8, r.v) });
            n += 1;
        }
    }

    pub fn countKind(self: *TestStore, kind: u8) usize {
        var n: usize = 0;
        for (self.rows.items) |r| n += @intFromBool(r.kind == kind);
        return n;
    }

    pub fn store(self: *TestStore) Store {
        return Store.from(TestStore, self);
    }
};

const CHUNK: u64 = 4 * derive.MiB;
const INO: u64 = 77;

fn wb(vol: u64, block_idx: u64, stamp: u64) Mapping {
    return Mapping.wholeBlock(vol, block_idx * CHUNK, stamp);
}

/// Put `rec` straight into the store.
fn seed(ts: *TestStore, r: Rec) !void {
    return seedFor(ts, INO, r);
}

fn seedFor(ts: *TestStore, ino: u64, r: Rec) !void {
    var t = KvTx.init(testing.allocator, @src());
    try stagePut(&t, ino, r);
    try ts.commit(&t);
}

fn run(index: u32, len: u32, vol: u64, block_idx: u64, stamp: u64) Rec {
    return .{ .index = index, .len = len, .vol_tag = vol, .offset = block_idx * CHUNK, .incarnation = stamp };
}

test "ties: CROSSING_CHUNK = floor_pow2((128 KiB / 2) / (map frame + ref frame)) = 512 and the registry default parses to it; the threshold = chunk × 64 = 32,768; the window budget = mem_budget / 16 with no floor" {
    try testing.expectEqual(@as(u64, 60), MAP_RECORD_FRAME_BYTES);
    try testing.expectEqual(@as(u64, 50), block_refs.REF_RECORD_JOURNAL_BYTES);
    try testing.expectEqual(@as(u32, 512), CROSSING_CHUNK);
    try testing.expectEqual(CROSSING_CHUNK, knobs.intDefault("SQUEEZEFS_MAP_MIGRATE_CHUNK", u32));
    try testing.expectEqual(@as(i128, 64), knobs.row("SQUEEZEFS_MAP_MIGRATE_CHUNK").kind.int.lo);
    try testing.expectEqual(@as(i128, 1024), knobs.row("SQUEEZEFS_MAP_MIGRATE_CHUNK").kind.int.hi);
    try testing.expectEqual(@as(u64, 32_768), sweepThresholdBlocks(CROSSING_CHUNK));
    try testing.expectEqual(@as(u64, 1 * derive.GiB), windowBudgetBytes(16 * derive.GiB));
    try testing.expectEqual(@as(u64, 0), windowBudgetBytes(15));
    // 512 ops × the widest op stays far inside the whole-entry cap with the head and the inode Put beside.
    try testing.expect(@as(u64, CROSSING_CHUNK) * (MAP_RECORD_FRAME_BYTES + block_refs.REF_RECORD_JOURNAL_BYTES) + 2 * 256 <= journal.MAX_ENTRY_LEN);
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(@as(u32, 512), chunkFromEnv(&env));
}

test "Rec: the five wire kinds round-trip through one value form; a kind-2 payload is the 40 B wire, never a plain whole-block spelling; bindingAt derives offset and stamp by delta; extends needs volume, index, offset and stamp continuity under RUN_LEN_MAX" {
    const shapes = [_]Rec{
        run(5, 1, 0x11, 9, 0),
        run(5, 1, 0x11, 9, 1 << 40 | 3),
        run(5, 7, 0x11, 9, 0),
        run(5, 7, 0x11, 9, 1 << 40 | 3),
        Rec.point(5, Mapping.tenant(0x11, 9 * CHUNK, 2, 4096, 100)),
        Rec.point(5, .{ .vol_tag = 0x11, .offset = 9 * CHUNK, .incarnation = 4, .whole_block = true, .damaged = true }),
    };
    for (shapes) |r| {
        var wire: [layout.MAPPING_WIRE_LEN]u8 = undefined;
        const e = r.entryInto(&wire);
        var buf: [64]u8 = undefined;
        const enc = e.encodeInto(&buf);
        try testing.expectEqual(r.encodedLen(), enc.len);
        const back = try Rec.fromEntry(5, try record.MapEntry.decode(enc), CHUNK);
        try testing.expect(back.eql(r));
    }
    // A plain whole-block binding spelled as kind 2 refuses.
    var wire: [layout.MAPPING_WIRE_LEN]u8 = std.mem.toBytes(wb(1, 2, 3).encode());
    try testing.expectError(error.Corrupt, Rec.fromEntry(0, .{ .string = &wire }, CHUNK));
    try testing.expectError(error.Corrupt, Rec.fromEntry(0, .{ .string = wire[0..39] }, CHUNK));
    // Derivation by delta.
    const r = run(10, 4, 0x11, 100, 1 << 40 | 50);
    try testing.expect(r.covers(10) and r.covers(13) and !r.covers(14) and !r.covers(9));
    const b = r.bindingAt(12, CHUNK);
    try testing.expectEqual(@as(u64, 102 * CHUNK), b.offset);
    try testing.expectEqual(@as(u64, 1 << 40 | 52), b.incarnation);
    try testing.expect(b.whole_block);
    // Continuity.
    try testing.expect(r.extends(run(14, 1, 0x11, 104, 1 << 40 | 54), CHUNK));
    try testing.expect(!r.extends(run(15, 1, 0x11, 105, 1 << 40 | 55), CHUNK));
    try testing.expect(!r.extends(run(14, 1, 0x11, 105, 1 << 40 | 54), CHUNK));
    try testing.expect(!r.extends(run(14, 1, 0x11, 104, 1 << 40 | 55), CHUNK));
    try testing.expect(!r.extends(run(14, 1, 0x22, 104, 1 << 40 | 54), CHUNK));
    try testing.expect(!run(0, 4000, 1, 0, 0).extends(run(4000, 97, 1, 4000, 0), CHUNK));
    try testing.expect(run(0, 4000, 1, 0, 0).extends(run(4000, 96, 1, 4000, 0), CHUNK));
    try testing.expect(!run(0, 1, 1, 0, 0).extends(run(1, 1, 1, 1, 5), CHUNK));
    // canonicalize: 1 + 2 + 3 → one run of 6; a break stays.
    var recs = [_]Rec{ run(0, 1, 1, 0, 1), run(1, 2, 1, 1, 2), run(3, 3, 1, 3, 4), run(9, 1, 1, 9, 10), run(10, 1, 2, 10, 11) };
    const n = canonicalize(&recs, CHUNK);
    try testing.expectEqual(@as(usize, 3), n);
    try testing.expectEqual(@as(u32, 6), recs[0].len);
    try testing.expectEqual(@as(u32, 9), recs[1].index);
}

test "the read law: an exact point supersedes a covering run; the floor probe keeps the LAST coverer (a superseding point between the run and N is not it); block 0 has no floor; a run past RUN_LEN_MAX is unreachable by construction" {
    const gpa = testing.allocator;
    var ts = TestStore.init(gpa);
    defer ts.deinit();
    const s = ts.store();
    // A run [100, 200) on vol 1, a superseding point at 150 on vol 2.
    try seed(&ts, run(100, 100, 1, 1000, 1 << 40 | 1));
    try seed(&ts, Rec.point(150, wb(2, 7, 1 << 40 | 99)));
    const at150 = (try lookup(s, gpa, INO, 150, CHUNK)).?;
    try testing.expectEqual(@as(u64, 2), at150.vol_tag);
    const at151 = (try lookup(s, gpa, INO, 151, CHUNK)).?;
    try testing.expectEqual(@as(u64, 1), at151.vol_tag);
    try testing.expectEqual(@as(u64, 1051 * CHUNK), at151.offset);
    try testing.expectEqual(@as(u64, 1 << 40 | 52), at151.incarnation);
    try testing.expectEqual(@as(?Mapping, null), try lookup(s, gpa, INO, 200, CHUNK));
    try testing.expectEqual(@as(?Mapping, null), try lookup(s, gpa, INO, 99, CHUNK));
    try testing.expectEqual(@as(?Mapping, null), try lookup(s, gpa, INO, 0, CHUNK));
    // The floor at 199 sees the point at 150 AFTER the run but keeps the run (the coverer).
    const fl = (try lookupFloor(s, gpa, INO, 199, CHUNK)).?;
    try testing.expectEqual(@as(u32, 100), fl.index);
    // Two runs transiently overlapping (the mid-train shape): the later start is the newer truth.
    try seed(&ts, run(120, 100, 3, 5000, 0));
    const fl2 = (try lookupFloor(s, gpa, INO, 180, CHUNK)).?;
    try testing.expectEqual(@as(u32, 120), fl2.index);
    // The bounded scan: a record 4096 below is invisible; 4095 below is seen.
    var ts2 = TestStore.init(gpa);
    defer ts2.deinit();
    try seed(&ts2, run(0, 4096, 1, 0, 0));
    try testing.expect((try lookup(ts2.store(), gpa, INO, 4095, CHUNK)) != null);
    try testing.expectEqual(@as(?Mapping, null), try lookup(ts2.store(), gpa, INO, 4096, CHUNK));
    // Range with the covering-run prepend; a cross-ino record never leaks
    // into the ino's exact bounds.
    try seedFor(&ts, INO + 1, Rec.point(0, wb(9, 1, 0)));
    var out: std.ArrayList(Rec) = .empty;
    defer out.deinit(gpa);
    try rangeCovering(s, gpa, INO, 130, 16, CHUNK, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    try testing.expectEqual(@as(u32, 120), out.items[0].index);
    try testing.expectEqual(@as(u32, 150), out.items[1].index);
    out.clearRetainingCapacity();
    try rangeFrom(s, gpa, INO, 0, 100, CHUNK, &out);
    try testing.expectEqual(@as(usize, 3), out.items.len);
}

test "the window put displaces at most one binding per (unique) insert, so a merge of at most max_displaced inserts fills MergeOutcome exactly and never past it; a window past the bound is loud (kvmap_merge_displaced_overflow), never a silent drop" {
    const gpa = testing.allocator;
    var ts = TestStore.init(gpa);
    defer ts.deinit();
    const s = ts.store();
    const max = seams.MergeOutcome.max_displaced;
    const S0: u64 = 1 << 40 | 1;
    var first: [max + 1]seams.MapInsert = undefined;
    var over: [max + 1]seams.MapInsert = undefined;
    for (&first, &over, 0..) |*a, *b, i| {
        const blk: u32 = @intCast(i);
        a.* = .{ .block = blk, .binding = wb(1, 500 + blk, S0 + blk) };
        b.* = .{ .block = blk, .binding = wb(2, 900 + blk, S0 + 1000 + blk) };
    }
    {
        var t = KvTx.init(gpa, @src());
        var out: seams.MergeOutcome = .{};
        try stageWindow(s, gpa, &t, INO, &first, CHUNK, &out);
        try testing.expectEqual(@as(u8, 0), out.n_displaced);
        try ts.commit(&t);
    }
    const site = "kvmap_merge_displaced_overflow";
    const fired0 = counters.tripwireCount(site);
    {
        var t = KvTx.init(gpa, @src());
        defer t.deinit();
        var out: seams.MergeOutcome = .{};
        try stageWindow(s, gpa, &t, INO, over[0..max], CHUNK, &out);
        try testing.expectEqual(@as(u8, max), out.n_displaced);
        try testing.expectEqual(fired0, counters.tripwireCount(site));
    }
    stats.merge_displaced_overflow.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    var t = KvTx.init(gpa, @src());
    defer t.deinit();
    var out: seams.MergeOutcome = .{};
    try stageWindow(s, gpa, &t, INO, &over, CHUNK, &out);
    try log.testing.expectAllFired();
    try testing.expectEqual(fired0 + 1, counters.tripwireCount(site));
}

test "the window put: sequential per-block merges grow ONE run at the left edge (no point per block); an overwrite inside a run supersedes with the −ref of the run-derived block; a write at a run's own key keeps the tail covered; an idempotent re-bind moves no reference; tree ops stay O(window)" {
    const gpa = testing.allocator;
    var ts = TestStore.init(gpa);
    defer ts.deinit();
    const s = ts.store();
    const S0: u64 = 1 << 40 | 1;
    // 32 blocks, one at a time, consecutive offsets and stamps on vol 1.
    var b: u32 = 0;
    while (b < 32) : (b += 1) {
        var t = KvTx.init(gpa, @src());
        var out: seams.MergeOutcome = .{};
        try stageWindow(s, gpa, &t, INO, &.{.{ .block = b, .binding = wb(1, 500 + b, S0 + b) }}, CHUNK, &out);
        try testing.expectEqual(@as(u8, 0), out.n_displaced);
        try ts.commit(&t);
    }
    try testing.expectEqual(@as(usize, 1), ts.countKind(key.TREE_BLOCK_MAP));
    try testing.expectEqual(@as(usize, 32), ts.countKind(key.TREE_BLOCK_REFS));
    const r0 = (try lookupExact(s, gpa, INO, 0, CHUNK)).?;
    try testing.expectEqual(@as(u32, 32), r0.len);
    try testing.expect((try lookup(s, gpa, INO, 31, CHUNK)).?.eql(wb(1, 531, S0 + 31)));
    // A block out of order (the pipeline raced): 40 before 33..39 — a point, later absorbed when 39 arrives? No: 33..38 extend the run, 39 bridges to 40's point.
    {
        var t = KvTx.init(gpa, @src());
        var out: seams.MergeOutcome = .{};
        try stageWindow(s, gpa, &t, INO, &.{.{ .block = 40, .binding = wb(1, 540, S0 + 40) }}, CHUNK, &out);
        try ts.commit(&t);
    }
    b = 32;
    while (b < 40) : (b += 1) {
        var t = KvTx.init(gpa, @src());
        var out: seams.MergeOutcome = .{};
        try stageWindow(s, gpa, &t, INO, &.{.{ .block = b, .binding = wb(1, 500 + b, S0 + b) }}, CHUNK, &out);
        try ts.commit(&t);
    }
    try testing.expectEqual(@as(usize, 1), ts.countKind(key.TREE_BLOCK_MAP));
    try testing.expectEqual(@as(u32, 41), (try lookupExact(s, gpa, INO, 0, CHUNK)).?.len);
    // Overwrite inside the run (block 10 → vol 2): the run-derived block is displaced and released; the run splits coverage-exact.
    {
        var t = KvTx.init(gpa, @src());
        var out: seams.MergeOutcome = .{};
        const released0 = block_refs.stats.meta_kv_block_refs_released.load();
        try stageWindow(s, gpa, &t, INO, &.{.{ .block = 10, .binding = wb(2, 9, 1 << 40 | 700) }}, CHUNK, &out);
        try testing.expectEqual(@as(u8, 1), out.n_displaced);
        try testing.expect(out.displaced[0].eql(wb(1, 510, S0 + 10)));
        try testing.expectEqual(released0 + 1, block_refs.stats.meta_kv_block_refs_released.load());
        try ts.commit(&t);
    }
    try testing.expect((try lookup(s, gpa, INO, 10, CHUNK)).?.eql(wb(2, 9, 1 << 40 | 700)));
    try testing.expect((try lookup(s, gpa, INO, 9, CHUNK)).?.eql(wb(1, 509, S0 + 9)));
    try testing.expect((try lookup(s, gpa, INO, 11, CHUNK)).?.eql(wb(1, 511, S0 + 11)));
    try testing.expect((try lookup(s, gpa, INO, 40, CHUNK)).?.eql(wb(1, 540, S0 + 40)));
    try testing.expectEqual(@as(usize, 41), ts.countKind(key.TREE_BLOCK_REFS));
    // A write AT a run's own key (block 11, the head of the right part): the tail [12, 41) stays covered.
    {
        var t = KvTx.init(gpa, @src());
        var out: seams.MergeOutcome = .{};
        try stageWindow(s, gpa, &t, INO, &.{.{ .block = 11, .binding = wb(2, 10, 1 << 40 | 701) }}, CHUNK, &out);
        try testing.expectEqual(@as(u8, 1), out.n_displaced);
        try ts.commit(&t);
    }
    try testing.expect((try lookup(s, gpa, INO, 12, CHUNK)).?.eql(wb(1, 512, S0 + 12)));
    try testing.expect((try lookup(s, gpa, INO, 11, CHUNK)).?.eql(wb(2, 10, 1 << 40 | 701)));
    try testing.expect((try lookup(s, gpa, INO, 40, CHUNK)).?.eql(wb(1, 540, S0 + 40)));
    // Idempotent re-bind: the same block again moves no reference and displaces nothing.
    {
        var t = KvTx.init(gpa, @src());
        var out: seams.MergeOutcome = .{};
        const staged0 = block_refs.stats.meta_kv_block_refs_staged.load();
        try stageWindow(s, gpa, &t, INO, &.{.{ .block = 12, .binding = wb(1, 512, S0 + 12) }}, CHUNK, &out);
        try testing.expectEqual(@as(u8, 0), out.n_displaced);
        try testing.expectEqual(staged0, block_refs.stats.meta_kv_block_refs_staged.load());
        try testing.expect(t.isEmpty());
        try ts.commit(&t);
    }
    // Every index 0..40 resolves to its own binding (the expansion equals the emission at every stride).
    b = 0;
    while (b <= 40) : (b += 1) {
        const want = if (b == 10) wb(2, 9, 1 << 40 | 700) else if (b == 11) wb(2, 10, 1 << 40 | 701) else wb(1, 500 + b, S0 + b);
        try testing.expect((try lookup(s, gpa, INO, b, CHUNK)).?.eql(want));
    }
    // The window law: one merge of a fresh index costs O(1) tree reads whatever the file holds.
    const exact0 = stats.meta_kv_block_map_lookup_exact.load();
    const range0 = stats.meta_kv_block_map_lookup_range.load();
    {
        var t = KvTx.init(gpa, @src());
        var out: seams.MergeOutcome = .{};
        try stageWindow(s, gpa, &t, INO, &.{.{ .block = 41, .binding = wb(1, 541, S0 + 41) }}, CHUNK, &out);
        try ts.commit(&t);
    }
    try testing.expectEqual(exact0 + 1, stats.meta_kv_block_map_lookup_exact.load());
    try testing.expectEqual(range0 + 1, stats.meta_kv_block_map_lookup_range.load());
}

test "the crossing plan: a healthy first crossing is Puts only over an empty descent; an interrupted train's residue is reconciled — a changed binding is one same-key Put, a record beyond the final map is deleted, Puts lead and Deletes trail" {
    const gpa = testing.allocator;
    var ts = TestStore.init(gpa);
    defer ts.deinit();
    const s = ts.store();
    // The inline map: 0..=9 consecutive on vol 1 (one run), 20 on vol 2, 21 on vol 2 at a non-consecutive offset.
    var ents: std.ArrayList(layout.MapEntry) = .empty;
    defer ents.deinit(gpa);
    var b: u32 = 0;
    while (b < 10) : (b += 1) try ents.append(gpa, .{ .block = b, .mapping = wb(1, 100 + b, (1 << 40) | @as(u64, 10 + b)) });
    try ents.append(gpa, .{ .block = 20, .mapping = wb(2, 7, 1 << 40 | 90) });
    try ents.append(gpa, .{ .block = 21, .mapping = wb(2, 70, 1 << 40 | 91) });
    const wire = try layout.sortedEntries(gpa, ents.items);
    defer gpa.free(wire);
    var plan = try planCrossing(s, gpa, INO, wire, CHUNK);
    defer plan.deinit(gpa);
    try testing.expectEqual(@as(u32, 0), plan.preexisting);
    try testing.expectEqual(@as(u32, 3), plan.records);
    try testing.expectEqual(@as(usize, 3), plan.ops.len);
    try testing.expectEqual(@as(u32, 10), plan.ops[0].put.len);
    // Residue: a stale binding at 20, a stale record at 500 (beyond the map), the run already right.
    try seed(&ts, run(0, 10, 1, 100, 1 << 40 | 10));
    try seed(&ts, Rec.point(20, wb(9, 9, 1 << 40 | 9)));
    try seed(&ts, Rec.point(500, wb(9, 10, 1 << 40 | 11)));
    var plan2 = try planCrossing(s, gpa, INO, wire, CHUNK);
    defer plan2.deinit(gpa);
    try testing.expectEqual(@as(u32, 3), plan2.preexisting);
    try testing.expectEqual(@as(usize, 3), plan2.ops.len);
    try testing.expect(plan2.ops[0] == .put and plan2.ops[0].put.index == 20);
    try testing.expect(plan2.ops[1] == .put and plan2.ops[1].put.index == 21);
    try testing.expect(plan2.ops[2] == .delete and plan2.ops[2].delete == 500);
    var t = KvTx.init(gpa, @src());
    for (plan2.ops) |op| try stageOp(&t, INO, op);
    try ts.commit(&t);
    try testing.expectEqual(@as(usize, 3), ts.countKind(key.TREE_BLOCK_MAP));
    for (ents.items) |e| try testing.expect((try lookup(s, gpa, INO, e.block, CHUNK)).?.eql(e.mapping));
    try testing.expectEqual(@as(?Mapping, null), try lookup(s, gpa, INO, 500, CHUNK));
}

test "the sweep page: budget by covered indices; the straddler keeps its head and consumes its tail right-to-left, re-Put shortened; a superseding point inside a deleted run is never freed through the run; the resume cursor is the first unprocessed record's own key; chunks sum exactly to the residue; the terminal page clears" {
    const gpa = testing.allocator;
    var ts = TestStore.init(gpa);
    defer ts.deinit();
    const s = ts.store();
    // [0, 100) run on vol 1; a point at 60 on vol 2 superseding; [100, 103) run; point 200.
    try seed(&ts, run(0, 100, 1, 1000, 1 << 40 | 1));
    try seed(&ts, Rec.point(60, wb(2, 5, 1 << 40 | 500)));
    try seed(&ts, run(100, 3, 1, 1100, 1 << 40 | 101));
    try seed(&ts, Rec.point(200, wb(3, 6, 1 << 40 | 600)));
    // Truncate to 50 blocks: start = 50. Budget 30: the straddler's tail [50, 100) is 50 indices → consume [70, 100), shortened to len 70, cursor stays 50.
    var freed_total: u64 = 0;
    var p1 = try planSweepPage(s, gpa, INO, 50, std.math.maxInt(u32), 30, CHUNK);
    defer p1.deinit(gpa);
    try testing.expectEqual(@as(u32, 30), p1.indices);
    try testing.expectEqual(@as(usize, 1), p1.puts.items.len);
    try testing.expectEqual(@as(u32, 70), p1.puts.items[0].len);
    try testing.expectEqual(@as(usize, 30), p1.freed.items.len);
    try testing.expectEqual(@as(?u32, 50), p1.next_cursor);
    try testing.expect(p1.freed.items[0].binding.eql(wb(1, 1070, 1 << 40 | 71)));
    freed_total += p1.freed.items.len;
    {
        var t = KvTx.init(gpa, @src());
        try stageSweepPage(&t, INO, &p1, CHUNK);
        try ts.commit(&t);
    }
    // Chunk 2, budget 30: the tail [50, 70) = 20 indices, of which 60 is superseded (not freed through the run); then the point at 60 (1), then the run [100,103) (3), then 200 (1): 25 indices, all records consumed → terminal.
    var p2 = try planSweepPage(s, gpa, INO, 50, std.math.maxInt(u32), 30, CHUNK);
    defer p2.deinit(gpa);
    try testing.expectEqual(@as(u32, 25), p2.indices);
    try testing.expectEqual(@as(usize, 1), p2.puts.items.len);
    try testing.expectEqual(@as(u32, 50), p2.puts.items[0].len);
    try testing.expectEqual(@as(usize, 3), p2.deletes.items.len);
    try testing.expectEqual(@as(usize, 19 + 1 + 3 + 1), p2.freed.items.len);
    try testing.expectEqual(@as(?u32, null), p2.next_cursor);
    var saw60_vol2 = false;
    for (p2.freed.items) |f| {
        if (f.block == 60) {
            try testing.expectEqual(@as(u64, 2), f.binding.vol_tag);
            saw60_vol2 = true;
        }
    }
    try testing.expect(saw60_vol2);
    freed_total += p2.freed.items.len;
    {
        var t = KvTx.init(gpa, @src());
        try stageSweepPage(&t, INO, &p2, CHUNK);
        try ts.commit(&t);
    }
    // Exactly the residue: 49 run-derived indices (60's was displaced when
    // its point was written) + the point + 3 + 1 = 54 frees; the kept head
    // [0, 50) stands.
    try testing.expectEqual(@as(u64, 54), freed_total);
    try testing.expectEqual(@as(usize, 1), ts.countKind(key.TREE_BLOCK_MAP));
    try testing.expectEqual(@as(u32, 50), (try lookupExact(s, gpa, INO, 0, CHUNK)).?.len);
    try testing.expectEqual(@as(?Mapping, null), try lookup(s, gpa, INO, 50, CHUNK));
    var p3 = try planSweepPage(s, gpa, INO, 50, std.math.maxInt(u32), 30, CHUNK);
    defer p3.deinit(gpa);
    try testing.expect(p3.isEmpty() and p3.next_cursor == null);
    // A full page of points: the cursor is last Delete + 1.
    var b: u32 = 300;
    while (b < 310) : (b += 1) try seed(&ts, Rec.point(b, wb(4, b, (1 << 40) | @as(u64, b))));
    var p4 = try planSweepPage(s, gpa, INO, 300, std.math.maxInt(u32), 4, CHUNK);
    defer p4.deinit(gpa);
    try testing.expectEqual(@as(usize, 4), p4.deletes.items.len);
    try testing.expectEqual(@as(?u32, 304), p4.next_cursor);
    // A run longer than the budget keyed at/after start: shortened at its key, cursor = its key.
    try seed(&ts, run(400, 10, 5, 4000, 0));
    var p5 = try planSweepPage(s, gpa, INO, 400, std.math.maxInt(u32), 4, CHUNK);
    defer p5.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), p5.puts.items.len);
    try testing.expectEqual(@as(u32, 6), p5.puts.items[0].len);
    try testing.expectEqual(@as(?u32, 400), p5.next_cursor);
    try testing.expectEqual(@as(u32, 4), p5.indices);
    // The bounded window (the extend barrier's shape): [304, 308) sees the
    // four points there and nothing of 308..309 or the run at 400.
    var p6 = try planSweepPage(s, gpa, INO, 304, 308, 64, CHUNK);
    defer p6.deinit(gpa);
    try testing.expectEqual(@as(usize, 4), p6.deletes.items.len);
    try testing.expectEqual(@as(?u32, null), p6.next_cursor);
}

test "C11 walk: records and indices counted; a point on a FOREIGN volume inside a run's span is a run-foreign shadow, a same-volume one is legal; the owner skip-scan finds every owner and jumps past foreign kinds; deriveBindings expands every record" {
    const gpa = testing.allocator;
    var ts = TestStore.init(gpa);
    defer ts.deinit();
    const s = ts.store();
    try seed(&ts, run(0, 10, 1, 0, 0));
    try seed(&ts, Rec.point(3, wb(1, 50, 0)));
    try seed(&ts, Rec.point(5, wb(2, 50, 0)));
    const rep = try verifyBlockMap(s, gpa, INO, CHUNK);
    try testing.expectEqual(@as(u64, 3), rep.records);
    try testing.expectEqual(@as(u64, 12), rep.indices);
    try testing.expectEqual(@as(u64, 1), rep.run_foreign_shadows);
    try testing.expectEqual(@as(u64, 0), (try verifyBlockMap(s, gpa, INO + 5, CHUNK)).records);
    // Owners: INO and INO + 9; an inode record of INO + 3 sits between.
    try seedFor(&ts, INO + 9, run(4, 1, 1, 1, 0));
    try ts.put(key.TREE_INODES, &key.inodeKey(INO + 3), "x");
    try testing.expectEqual(@as(?u64, INO), try nextOwner(s, gpa, 0));
    try testing.expectEqual(@as(?u64, INO + 9), try nextOwner(s, gpa, INO + 1));
    try testing.expectEqual(@as(?u64, null), try nextOwner(s, gpa, INO + 10));
    var binds: std.ArrayList(block_refs.Binding) = .empty;
    defer binds.deinit(gpa);
    try deriveBindings(s, gpa, INO, CHUNK, &binds);
    try testing.expectEqual(@as(usize, 12), binds.items.len);
}

test "the window store: a fill answers hits inside its span (mapping or a definitive hole), misses outside; two spans per ino, LRU within the row; invalidate drops the ino; the budget evicts the oldest span; shed drops everything and counts; the component is sheddable (floor 0, weight 2)" {
    const gpa = testing.allocator;
    var wc = try WindowCache.init(gpa, 3 * @sizeOf(Rec), CHUNK);
    defer wc.deinit();
    try testing.expectEqual(@as(u64, 3 * @sizeOf(Rec)), stats.kvmap_write_map_budget_bytes.load());
    const recs = [_]Rec{ run(100, 10, 1, 1000, 0), run(120, 1, 2, 5, 0) };
    try testing.expect(wc.probe(INO, 105) == null);
    try wc.fill(INO, 100, &recs, true);
    try testing.expectEqual(Probe{ .mapping = wb(1, 1005, 0) }, wc.probe(INO, 105).?);
    try testing.expectEqual(Probe.hole, wc.probe(INO, 115).?);
    try testing.expectEqual(Probe{ .mapping = wb(2, 5, 0) }, wc.probe(INO, 120).?);
    try testing.expectEqual(Probe.hole, wc.probe(INO, 4000).?); // short page: exhausted above
    try testing.expect(wc.probe(INO, 99) == null);
    try testing.expect(wc.probe(INO + 1, 105) == null);
    // A second span of the same ino; a third evicts the LRU (the first).
    try wc.fill(INO, 10, &.{run(10, 2, 3, 30, 0)}, false);
    try testing.expectEqual(Probe{ .mapping = wb(3, 31, 0) }, wc.probe(INO, 11).?);
    try testing.expect(wc.probe(INO, 12) == null); // hi = the last record's end − 1
    try testing.expectEqual(Probe{ .mapping = wb(1, 1005, 0) }, wc.probe(INO, 105).?);
    const ev0 = stats.block_map_window_evictions.load();
    try wc.fill(INO, 50, &.{run(50, 1, 4, 40, 0)}, false);
    try testing.expect(stats.block_map_window_evictions.load() >= ev0 + 1);
    // The budget: 3 recs of room; a 2-record span + two 1-record spans = 4 → the oldest goes.
    try testing.expect(stats.block_map_window_bytes.load() <= 3 * @sizeOf(Rec));
    wc.invalidate(INO);
    try testing.expect(wc.probe(INO, 50) == null);
    try testing.expectEqual(@as(u64, 0), stats.block_map_window_bytes.load());
    try wc.fill(INO + 2, 0, &.{run(0, 1, 1, 1, 0)}, true);
    const shed0 = stats.kvmap_windows_shed.load();
    wc.shedAll();
    try testing.expectEqual(shed0 + 1, stats.kvmap_windows_shed.load());
    try testing.expect(wc.probe(INO + 2, 0) == null);
    const c = wc.component();
    try testing.expectEqual(@as(u64, 0), c.floor);
    try testing.expectEqual(WINDOW_SHED_WEIGHT, c.weight);
    try testing.expect(c.shed != null);
    // The clip: a run-heavy page never balloons a span past 4,096 indices.
    try wc.fill(INO + 3, 0, &.{ run(0, 4096, 1, 0, 0), run(4096, 4096, 1, 4096, 0) }, false);
    try testing.expect(wc.probe(INO + 3, 4095) != null);
    try testing.expect(wc.probe(INO + 3, 4096) == null);
}

test "stats JSON names every instrument" {
    var buf: [2048]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const out = w.buffered();
    inline for (.{ "\"meta_kv_block_map_puts\":", "\"meta_kv_block_map_lookup_floor\":", "\"meta_kv_block_map_range_records\":", "\"kvmap_crossings\":", "\"map_migrate_resumed\":", "\"kvmap_window_saves\":", "\"map_sweep_chunks\":", "\"map_sweep_resumed\":", "\"block_map_window_bytes\":", "\"fsck_map_orphan_records\":", "\"kvmap_owner_mismatch\":", "\"kvmap_merge_displaced_overflow\":" }) |k| {
        try testing.expect(std.mem.indexOf(u8, out, k) != null);
    }
    try testing.expect(std.mem.endsWith(u8, out, "}"));
}
