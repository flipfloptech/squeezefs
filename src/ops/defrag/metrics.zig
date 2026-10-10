//! The fragmentation model (charter §8 "Defrag axes D1 free-space
//! contiguity / D2 file locality / D3 staged-extent pressure / D4 meta
//! node occupancy, each a gauge + an independently invocable mover
//! reusing existing machinery"; spec §3.1): at 4 MiB blocks and 256 KiB
//! meta extents classical intra-file fragmentation does not exist —
//! "fragmentation" IS four measured axes. Every definition is a pure
//! function here; the gauges are permille + 1 (`encodeRatio`: raw 0 =
//! never measured ⇒ the stats JSON emits `null`), worst-volume semantics
//! for the per-volume axes. Portable.
//!
//!   D1  per volume over the ALLOCATED offset space `[0, highest)`:
//!       `contiguity = largest_free_run / total_free`, `reclaimable_tail =
//!       trailing_free_run / total_free` (both 1.0 when nothing is free).
//!       The virgin tail past `highest` deliberately does NOT count: it is
//!       already contiguous and reclaimable, and counting it masks real
//!       fragmentation on any volume not nearly full.
//!   D1' the PACK face, per pack block: `occupancy = Σ_{distinct off}
//!       max len at off / CHUNK` — same-`off` windows (the clone share,
//!       the nested prefix share) counted ONCE; VICTIM iff `live ≤
//!       pack_max_slot` (the one law's other face).
//!   D2  over logical-order striped entries a pair counts when `idx₂ =
//!       idx₁ + 1`; LOCAL when same backend AND physically ascending;
//!       `locality = local / pairs` (1.0 when no pairs).
//!   D3  `parked_extent_bytes + Σ spilled extent-record bytes`.
//!   D4  per meta volume: `dead_bset_ratio = 1 − live/indexed` over the
//!       serialized bset logs; `mergeable_leaves` = underfull leaves.

const std = @import("std");
const counters = @import("../../core/counters.zig");
const layout = @import("../../data/layout.zig");
const pack = @import("../../data/pack.zig");

pub const PERMILLE: u64 = 1000;

/// A ratio gauge's word: `permille + 1`; 0 = never measured.
pub fn encodeRatio(num: u64, den: u64) u64 {
    if (den == 0) return PERMILLE + 1;
    const p: u64 = @intCast(@min(@as(u128, num) * PERMILLE / den, PERMILLE));
    return p + 1;
}

/// The permille a gauge word carries, or null when never measured.
pub fn decodeRatio(word: u64) ?u64 {
    if (word == 0) return null;
    return word - 1;
}

pub const stats = struct {
    pub var frag_d1_contiguity: counters.Gauge = .{};
    pub var frag_d1_reclaimable_tail: counters.Gauge = .{};
    pub var frag_d2_locality: counters.Gauge = .{};
    pub var frag_d3_pressure_bytes: counters.Gauge = .{};
    pub var frag_d4_dead_bset_ratio: counters.Gauge = .{};
    /// A COUNT summed over the set.
    pub var frag_d4_mergeable_leaves: counters.Gauge = .{};
    /// The worst pack block's occupancy; the mean; the pack-block count.
    pub var frag_d1_pack_occupancy: counters.Gauge = .{};
    pub var frag_d1_pack_occupancy_mean: counters.Gauge = .{};
    pub var frag_d1_pack_blocks: counters.Gauge = .{};
    pub var pack_reclaimable_bytes: counters.Gauge = .{};
    pub var pack_blocks_below_half: counters.Gauge = .{};
    pub var defrag_blocks_moved: counters.ShardedCounter = .{};
    pub var defrag_bytes_moved: counters.ShardedCounter = .{};
    pub var defrag_folds_kicked: counters.ShardedCounter = .{};
    pub var defrag_meta_compactions_kicked: counters.ShardedCounter = .{};
    pub var defrag_meta_merges: counters.ShardedCounter = .{};
    pub var pack_compactions: counters.ShardedCounter = .{};
    pub var pack_compaction_tenants_moved: counters.ShardedCounter = .{};
    pub var pack_compaction_windows_copied: counters.ShardedCounter = .{};
    pub var pack_compaction_bytes_copied: counters.ShardedCounter = .{};
    pub var pack_compaction_bytes_reclaimed: counters.ShardedCounter = .{};
    pub var pack_compaction_blocks_freed: counters.ShardedCounter = .{};
    pub var pack_compaction_deferred: counters.ShardedCounter = .{};
    pub var pack_compaction_skipped_lone_victim: counters.ShardedCounter = .{};
    /// A structural refusal of the pre-publish raise (must-stay-0).
    pub var pack_compaction_raise_refused: counters.Tripwire = .init("pack_compaction_raise_refused");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try counters.writeNamespaceJson(stats, w);
}

// ---------------------------------------------------------------------------
// D1: free-space contiguity over the allocated offset space.
// ---------------------------------------------------------------------------

pub const D1 = struct {
    vol_tag: u64,
    highest: u64,
    total_free: u64,
    largest_free_run: u64,
    trailing_free_run: u64,
    used_blocks: u64,

    pub fn contiguityPermille(self: D1) u64 {
        return decodeRatio(encodeRatio(self.largest_free_run, self.total_free)).?;
    }

    pub fn reclaimableTailPermille(self: D1) u64 {
        return decodeRatio(encodeRatio(self.trailing_free_run, self.total_free)).?;
    }

    /// The compaction frontier: `used` blocks fit `[0, used)` exactly; a
    /// referenced block at or above it is a D1 task.
    pub fn frontier(self: D1) u64 {
        return self.used_blocks;
    }
};

/// `free(idx)` over `[0, highest)`: the allocator's snapshot.
pub fn measureD1(vol_tag: u64, highest: u64, ctx: anytype, comptime isFree: fn (@TypeOf(ctx), u64) bool) D1 {
    var d: D1 = .{ .vol_tag = vol_tag, .highest = highest, .total_free = 0, .largest_free_run = 0, .trailing_free_run = 0, .used_blocks = 0 };
    var run: u64 = 0;
    var i: u64 = 0;
    while (i < highest) : (i += 1) {
        if (isFree(ctx, i)) {
            run += 1;
            d.total_free += 1;
            if (run > d.largest_free_run) d.largest_free_run = run;
        } else {
            run = 0;
            d.used_blocks += 1;
        }
    }
    d.trailing_free_run = run;
    return d;
}

// ---------------------------------------------------------------------------
// D1': the pack face.
// ---------------------------------------------------------------------------

pub const PackWindow = struct { off: u32, len: u32 };

/// Occupancy of one pack block from its referencers' windows: distinct
/// `off` at the max `len`, Σ `ceil(len)` clamped to the chunk.
pub fn packLiveBytes(windows: []const PackWindow, chunk: u64) u64 {
    var live: u64 = 0;
    for (windows, 0..) |w, i| {
        var longest = w.len;
        var first = true;
        for (windows[0..i]) |p| if (p.off == w.off) {
            first = false;
        };
        if (!first) continue;
        for (windows[i + 1 ..]) |q| if (q.off == w.off and q.len > longest) {
            longest = q.len;
        };
        live += layout.ceilGrain(longest);
    }
    return @min(live, chunk);
}

pub const PackBlock = struct {
    vol_tag: u64,
    block_idx: u64,
    live_bytes: u64,
    windows: u32,
    referencers: u32,

    pub fn occupancyPermille(self: PackBlock, chunk: u64) u64 {
        return decodeRatio(encodeRatio(self.live_bytes, chunk)).?;
    }

    pub fn isVictim(self: PackBlock, max_slot: u64) bool {
        return pack.isPackVictim(self.live_bytes, max_slot);
    }
};

pub const PackSummary = struct {
    blocks: u64,
    worst_occupancy_permille: u64,
    mean_occupancy_permille: u64,
    below_half: u64,
    reclaimable_bytes: u64,
};

pub fn summarizePacks(blocks: []const PackBlock, chunk: u64, max_slot: u64) PackSummary {
    var s: PackSummary = .{ .blocks = blocks.len, .worst_occupancy_permille = PERMILLE, .mean_occupancy_permille = PERMILLE, .below_half = 0, .reclaimable_bytes = 0 };
    if (blocks.len == 0) return s;
    var sum: u64 = 0;
    for (blocks) |b| {
        const occ = b.occupancyPermille(chunk);
        sum += occ;
        if (occ < s.worst_occupancy_permille) s.worst_occupancy_permille = occ;
        if (b.isVictim(max_slot)) {
            s.below_half += 1;
            s.reclaimable_bytes += chunk - b.live_bytes;
        }
    }
    s.mean_occupancy_permille = sum / blocks.len;
    return s;
}

// ---------------------------------------------------------------------------
// D2: file locality.
// ---------------------------------------------------------------------------

/// One striped entry in LOGICAL order.
pub const Entry = struct { block_index: u32, vol_tag: u64, block_idx: u64 };

pub const D2 = struct {
    pairs: u64,
    local: u64,

    pub fn localityPermille(self: D2) u64 {
        return decodeRatio(encodeRatio(self.local, self.pairs)).?;
    }
};

/// `entries` sorted by `block_index`: a pair counts when the indices are
/// consecutive; local when same backend AND physically ascending.
pub fn measureD2(entries: []const Entry) D2 {
    var d: D2 = .{ .pairs = 0, .local = 0 };
    if (entries.len < 2) return d;
    for (entries[1..], 0..) |e, i| {
        const p = entries[i];
        if (e.block_index != p.block_index + 1) continue;
        d.pairs += 1;
        if (e.vol_tag == p.vol_tag and e.block_idx > p.block_idx) d.local += 1;
    }
    return d;
}

/// The PLURALITY backend among a file's entries (ties by the lower tag)
/// — D2's target.
pub fn pluralityBackend(entries: []const Entry, eligible: []const u64) ?u64 {
    var best: ?u64 = null;
    var best_n: usize = 0;
    for (eligible) |tag| {
        var n: usize = 0;
        for (entries) |e| if (e.vol_tag == tag) {
            n += 1;
        };
        if (n > best_n or (n == best_n and n > 0 and best != null and tag < best.?)) {
            best = tag;
            best_n = n;
        }
    }
    return best;
}

// ---------------------------------------------------------------------------
// D3 / D4.
// ---------------------------------------------------------------------------

pub fn measureD3(parked_extent_bytes: u64, spilled_record_bytes: u64) u64 {
    return parked_extent_bytes +| spilled_record_bytes;
}

pub const D4 = struct {
    indexed: u64,
    live: u64,
    mergeable_leaves: u64,

    /// `1 − live/indexed` in permille (0 when nothing is indexed).
    pub fn deadRatioPermille(self: D4) u64 {
        if (self.indexed == 0) return 0;
        return PERMILLE - decodeRatio(encodeRatio(self.live, self.indexed)).?;
    }
};

/// A leaf is mergeable when its fold is ≤ ¼ of the capacity.
pub fn leafMergeable(fold_bytes: u64, capacity_bytes: u64) bool {
    return fold_bytes * 4 <= capacity_bytes;
}

/// Publish the cheap axes (D1/D3) from a measurement; worst-volume.
pub fn publishD1(worst_contiguity_permille: u64, worst_tail_permille: u64) void {
    stats.frag_d1_contiguity.set(worst_contiguity_permille + 1);
    stats.frag_d1_reclaimable_tail.set(worst_tail_permille + 1);
}

pub fn publishPack(s: PackSummary) void {
    stats.frag_d1_pack_occupancy.set(s.worst_occupancy_permille + 1);
    stats.frag_d1_pack_occupancy_mean.set(s.mean_occupancy_permille + 1);
    stats.frag_d1_pack_blocks.set(s.blocks);
    stats.pack_reclaimable_bytes.set(s.reclaimable_bytes);
    stats.pack_blocks_below_half.set(s.below_half);
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const CHUNK: u64 = 4 << 20;

test "ratio codec round-trips; 0 is never-measured; 1.0 encodes 1001; a zero denominator reads 1.0" {
    try testing.expectEqual(@as(?u64, null), decodeRatio(0));
    try testing.expectEqual(@as(u64, 1001), encodeRatio(5, 5));
    try testing.expectEqual(@as(u64, 1001), encodeRatio(0, 0));
    try testing.expectEqual(@as(u64, 1), encodeRatio(0, 7));
    try testing.expectEqual(@as(?u64, 500), decodeRatio(encodeRatio(1, 2)));
    try testing.expectEqual(@as(u64, 1001), encodeRatio(9, 4)); // clamped
}

test "D1 measure matches an independent census: free runs over [0, highest) only; the virgin tail never counts; nothing free reads 1.0/1.0; the frontier is the used count" {
    // Bits: U F F U F F F U U F  (highest = 10): total 6, largest 3, trailing 1, used 4.
    const bits = [_]bool{ false, true, true, false, true, true, true, false, false, true };
    const F = struct {
        fn isFree(b: []const bool, i: u64) bool {
            return b[@intCast(i)];
        }
    };
    const d = measureD1(1, bits.len, @as([]const bool, &bits), F.isFree);
    try testing.expectEqual(@as(u64, 6), d.total_free);
    try testing.expectEqual(@as(u64, 3), d.largest_free_run);
    try testing.expectEqual(@as(u64, 1), d.trailing_free_run);
    try testing.expectEqual(@as(u64, 4), d.used_blocks);
    try testing.expectEqual(@as(u64, 500), d.contiguityPermille());
    try testing.expectEqual(@as(u64, 166), d.reclaimableTailPermille());
    try testing.expectEqual(@as(u64, 4), d.frontier());
    // Only the first 4 blocks allocated-space: nothing free.
    const full = measureD1(1, 4, @as([]const bool, &[_]bool{ false, false, false, false }), F.isFree);
    try testing.expectEqual(@as(u64, 1000), full.contiguityPermille());
    try testing.expectEqual(@as(u64, 1000), full.reclaimableTailPermille());
    publishD1(d.contiguityPermille(), d.reclaimableTailPermille());
    try testing.expectEqual(@as(?u64, 500), decodeRatio(stats.frag_d1_contiguity.load()));
}

test "pack occupancy is exact on a planted population and counts both share classes once: a clone share (same off, same len) and a nested prefix share (same off, longer len) each count once at the max len; the victim line is the one law's other face" {
    const windows = [_]PackWindow{
        .{ .off = 0, .len = 10_000 }, // tenant A
        .{ .off = 0, .len = 10_000 }, // A's clone: once
        .{ .off = 12_288, .len = 100 }, // tenant B
        .{ .off = 12_288, .len = 4_000 }, // the nested prefix share: the longer wins
        .{ .off = 16_384, .len = 4_096 },
    };
    const live = packLiveBytes(&windows, CHUNK);
    try testing.expectEqual(@as(u64, 12_288 + 4_096 + 4_096), live);
    const pb: PackBlock = .{ .vol_tag = 1, .block_idx = 3, .live_bytes = live, .windows = 3, .referencers = 5 };
    try testing.expect(pb.isVictim(CHUNK / 2));
    const above: PackBlock = .{ .vol_tag = 1, .block_idx = 4, .live_bytes = CHUNK / 2 + 4096, .windows = 1, .referencers = 1 };
    try testing.expect(!above.isVictim(CHUNK / 2));
    const s = summarizePacks(&.{ pb, above }, CHUNK, CHUNK / 2);
    try testing.expectEqual(@as(u64, 2), s.blocks);
    try testing.expectEqual(@as(u64, 1), s.below_half);
    try testing.expectEqual(CHUNK - live, s.reclaimable_bytes);
    try testing.expectEqual(pb.occupancyPermille(CHUNK), s.worst_occupancy_permille);
    try testing.expectEqual(@as(u64, 4096), packLiveBytes(&.{.{ .off = 0, .len = 1 }}, CHUNK));
    try testing.expectEqual(CHUNK, packLiveBytes(&.{.{ .off = 0, .len = @intCast(CHUNK + 1) }}, CHUNK));
    publishPack(s);
    try testing.expectEqual(@as(u64, 1), stats.pack_blocks_below_half.load());
}

test "D2 locality: consecutive logical pairs only; local iff same backend and physically ascending; the plurality backend ties low; no pairs reads 1.0" {
    const entries = [_]Entry{
        .{ .block_index = 0, .vol_tag = 1, .block_idx = 10 },
        .{ .block_index = 1, .vol_tag = 1, .block_idx = 11 }, // local
        .{ .block_index = 2, .vol_tag = 2, .block_idx = 5 }, // backend change
        .{ .block_index = 3, .vol_tag = 2, .block_idx = 4 }, // descending
        .{ .block_index = 5, .vol_tag = 2, .block_idx = 9 }, // a hole: no pair
    };
    const d = measureD2(&entries);
    try testing.expectEqual(@as(u64, 3), d.pairs);
    try testing.expectEqual(@as(u64, 1), d.local);
    try testing.expectEqual(@as(u64, 333), d.localityPermille());
    try testing.expectEqual(@as(u64, 1000), measureD2(&.{}).localityPermille());
    try testing.expectEqual(@as(?u64, 2), pluralityBackend(&entries, &.{ 1, 2 }));
    try testing.expectEqual(@as(?u64, 1), pluralityBackend(&entries, &.{1}));
    try testing.expectEqual(@as(?u64, null), pluralityBackend(&entries, &.{}));
    const tie = [_]Entry{ .{ .block_index = 0, .vol_tag = 9, .block_idx = 0 }, .{ .block_index = 1, .vol_tag = 3, .block_idx = 0 } };
    try testing.expectEqual(@as(?u64, 3), pluralityBackend(&tie, &.{ 9, 3 }));
}

test "D3 is parked + spilled bytes; D4's dead ratio is 1 − live/indexed and a leaf is mergeable at ≤ ¼; the family renders" {
    try testing.expectEqual(@as(u64, 300), measureD3(100, 200));
    const d4: D4 = .{ .indexed = 1000, .live = 250, .mergeable_leaves = 3 };
    try testing.expectEqual(@as(u64, 750), d4.deadRatioPermille());
    try testing.expectEqual(@as(u64, 0), (D4{ .indexed = 0, .live = 0, .mergeable_leaves = 0 }).deadRatioPermille());
    try testing.expect(leafMergeable(1000, 4000) and !leafMergeable(1001, 4000));
    var buf: [8192]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"frag_d4_mergeable_leaves\"") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"pack_compaction_raise_refused\"") != null);
}
