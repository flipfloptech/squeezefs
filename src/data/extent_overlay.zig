//! W2 — the extent overlay, arm F of the write admission (charter §6.2
//! item 4; `docs/porting/data_write_path.md` §6). For sub-cap patch-
//! ineligible shapes (a transformed volume, a shared block, a hole, an
//! unaligned write): a compact sorted extent list + payload slabs charged
//! to the budget as `parked_extent_bytes` — a 4 KiB write parks ~4 KiB,
//! not 4 MiB. Escalation to a full buffer at ≥ 25 % coverage (`bytes × 4 ≥
//! block`); spill = a versioned + checksummed staging extent record (NO
//! seed read at spill); the fold at the last moment seeds ONCE and applies
//! k extents (`FOLD_MAX_EXTENTS`, `FOLD_MAX_BYTES = block/4`) — amplification
//! ≈ `2048/k + 2` (k = 32 ⇒ ≈ 66×, k = 16 ⇒ ≈ 130× under the 150× gate;
//! measured lz4 rand-4k 15–17× combined, `fold_fill` mean 80.2, 411–501 →
//! 17.7–18.3 k IOPS, `design-random-small-writes.md:81,124`).
//!
//! The slabs mirror the coverage runs EXACTLY (sorted, disjoint, non-
//! abutting; an overlap rewrite is newest-wins). Q7: `FOLD_MAX_EXTENTS` is
//! DERIVED (`block / (16 × PAGE)`: one fold per 64 KiB of block, the
//! measured amortization trigger — 34× at k = 64 under the 150× gate with
//! 4× margin), never a literal with a knob on top.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const checksum = @import("../core/checksum.zig");
const knobs = @import("../core/knobs.zig");
const derive = @import("../core/derive.zig");
const cov = @import("write_coverage.zig");
const seams = @import("write_seams.zig");

pub const PAGE: u32 = seams.PAGE;

// ---------------------------------------------------------------------------
// Derivations.
// ---------------------------------------------------------------------------

/// The extent-eligible class: `len × 4 < block` (a quarter block and up
/// is accumulation's shape).
pub fn isSmall(len: u64, block_size: u64) bool {
    return len * 4 < block_size;
}

/// The escalation edge: parked bytes reach a quarter of the block.
pub fn escalationDue(parked_bytes: u64, block_size: u64) bool {
    return parked_bytes * 4 >= block_size;
}

/// `block / (16 × PAGE)` = 64 at 4 MiB (module doc), bounded by the
/// coverage table's inline capacity.
pub fn foldMaxExtentsDerived(block_size: u64) u64 {
    return @min(block_size / (16 * PAGE), cov.MAX_EXTRA_RUNS);
}

/// `block / 4` = 1 MiB at 4 MiB (the former flat 1 MiB).
pub fn foldMaxBytesDerived(block_size: u64) u64 {
    return block_size / 4;
}

pub fn resolveFoldMaxExtents(explicit: ?u64, block_size: u64) u64 {
    return @min(explicit orelse foldMaxExtentsDerived(block_size), cov.MAX_EXTRA_RUNS);
}

pub fn resolveFoldMaxBytes(explicit: ?u64, block_size: u64) u64 {
    return explicit orelse foldMaxBytesDerived(block_size);
}

pub fn foldMaxExtentsFromEnv(env: *const std.process.Environ.Map, block_size: u64) u64 {
    return resolveFoldMaxExtents(knobs.optIntKnob("SQUEEZEFS_FOLD_MAX_EXTENTS", u64, env), block_size);
}

pub fn foldMaxBytesFromEnv(env: *const std.process.Environ.Map, block_size: u64) u64 {
    return resolveFoldMaxBytes(knobs.optIntKnob("SQUEEZEFS_FOLD_MAX_BYTES", u64, env), block_size);
}

/// The amortization of one fold over k extents: `2048/k + 2` (one seed
/// read + one upload of the block per k 4 KiB-class user writes at a 4 MiB
/// block: 1024 pages read + 1024 written = 2048 page-ops, plus the 2 the
/// user writes cost themselves).
pub fn amplification(k: u64) u64 {
    assert(k != 0);
    return 2048 / k + 2;
}

// ---------------------------------------------------------------------------
// Stats.
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// Sheddable via the fold.
    pub var parked_extent_bytes: counters.Gauge = .{};
    pub var extent_parks: counters.ShardedCounter = .{};
    pub var extent_escalations: counters.ShardedCounter = .{};
    /// An escalation due at the parked-buffer cap, left to the block's next
    /// write or its flush (the extent stays parked; `write_admit`'s gate).
    pub var extent_escalations_deferred: counters.ShardedCounter = .{};
    /// Legacy whole-image routes; designed routes keep it 0.
    pub var extent_implicit_escalations: counters.ShardedCounter = .{};
    pub var extent_record_absorbs: counters.ShardedCounter = .{};
    pub var extent_spills: counters.ShardedCounter = .{};
    pub var extent_spill_bytes: counters.ShardedCounter = .{};
    pub var extent_records_recovered: counters.ShardedCounter = .{};
    pub var extent_records_stale_discarded: counters.ShardedCounter = .{};
    pub var extent_records_torn_discarded: counters.ShardedCounter = .{};
    pub var extent_records_future_refused: counters.ShardedCounter = .{};
    pub var extent_records_foreign_scope: counters.ShardedCounter = .{};
    pub var fold_passes: counters.ShardedCounter = .{};
    pub var fold_fill: counters.ShardedLatencyHistogram = .{};
    pub var fold_seed_reads: counters.ShardedCounter = .{};
    pub var fold_extents_folded: counters.ShardedCounter = .{};
    pub var extent_overlay_completed_coverage: counters.Tripwire = .init("extent_overlay_completed_coverage");
    pub var small_first_write_completed_block: counters.Tripwire = .init("small_first_write_completed_block");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"parked_extent_bytes\":{d},", .{stats.parked_extent_bytes.load()});
    inline for (.{ "extent_parks", "extent_escalations", "extent_escalations_deferred", "extent_implicit_escalations", "extent_record_absorbs", "extent_spills", "extent_spill_bytes", "extent_records_recovered", "extent_records_stale_discarded", "extent_records_torn_discarded", "extent_records_future_refused", "extent_records_foreign_scope", "fold_passes", "fold_seed_reads", "fold_extents_folded" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.writeAll("\"fold_fill\":");
    try stats.fold_fill.snapshot().writeJson(w);
    try w.print(",\"extent_overlay_completed_coverage\":{d},\"small_first_write_completed_block\":{d}}}", .{ counters.tripwireCount("extent_overlay_completed_coverage"), counters.tripwireCount("small_first_write_completed_block") });
}

// ---------------------------------------------------------------------------
// The extent list.
// ---------------------------------------------------------------------------

pub const Slab = struct {
    start: u32,
    data: []u8,

    pub fn end(self: Slab) u32 {
        return self.start + @as(u32, @intCast(self.data.len));
    }
};

pub const ExtentOverlay = struct {
    /// Sorted by `start`, pairwise disjoint and non-abutting.
    slabs: std.ArrayList(Slab) = .empty,
    /// Item B: existing device data under the extents (the fold seeds).
    base_deferred: bool,
    bytes: u64 = 0,

    pub fn init(base_deferred: bool) ExtentOverlay {
        return .{ .base_deferred = base_deferred };
    }

    pub fn deinit(self: *ExtentOverlay, gpa: Allocator) void {
        for (self.slabs.items) |s| gpa.free(s.data);
        self.slabs.deinit(gpa);
        self.* = undefined;
    }

    pub fn count(self: *const ExtentOverlay) usize {
        return self.slabs.items.len;
    }

    /// Newest-wins merge of `[rel, rel + data.len)`: every slab it overlaps
    /// or abuts is coalesced into ONE slab (older bytes under the new
    /// window are replaced). The payload bytes charge `parked_extent_bytes`.
    pub fn merge(self: *ExtentOverlay, gpa: Allocator, rel: u32, data: []const u8) Allocator.Error!void {
        const new_end: u32 = rel + @as(u32, @intCast(data.len));
        // Find the touched span [lo, hi) of slabs.
        var lo: usize = 0;
        while (lo < self.slabs.items.len and self.slabs.items[lo].end() < rel) : (lo += 1) {}
        var hi: usize = lo;
        while (hi < self.slabs.items.len and self.slabs.items[hi].start <= new_end) : (hi += 1) {}
        if (lo == hi) {
            const copy = try gpa.dupe(u8, data);
            try self.slabs.insert(gpa, lo, .{ .start = rel, .data = copy });
            self.charge(data.len);
            return;
        }
        const start = @min(rel, self.slabs.items[lo].start);
        const end = @max(new_end, self.slabs.items[hi - 1].end());
        const merged = try gpa.alloc(u8, end - start);
        var old_bytes: u64 = 0;
        for (self.slabs.items[lo..hi]) |s| {
            @memcpy(merged[s.start - start .. s.end() - start], s.data);
            old_bytes += s.data.len;
        }
        @memcpy(merged[rel - start .. new_end - start], data);
        for (self.slabs.items[lo..hi]) |s| gpa.free(s.data);
        self.slabs.replaceRangeAssumeCapacity(lo, hi - lo, &.{.{ .start = start, .data = merged }});
        // The charge is the NET slab bytes.
        self.bytes -= old_bytes;
        stats.parked_extent_bytes.sub(old_bytes);
        self.charge(merged.len);
    }

    fn charge(self: *ExtentOverlay, n: usize) void {
        self.bytes += n;
        stats.parked_extent_bytes.add(n);
    }

    /// Lay every slab into a block buffer (the escalation and the fold).
    pub fn layInto(self: *const ExtentOverlay, dst: []u8) void {
        for (self.slabs.items) |s| @memcpy(dst[s.start..s.end()], s.data);
    }

    /// `[rel, rel + out.len)`: slab bytes where covered, zeros between.
    pub fn coveredRead(self: *const ExtentOverlay, rel: u32, out: []u8) void {
        @memset(out, 0);
        const end: u32 = rel + @as(u32, @intCast(out.len));
        for (self.slabs.items) |s| {
            const a = @max(s.start, rel);
            const b = @min(s.end(), end);
            if (a < b) @memcpy(out[a - rel .. b - rel], s.data[a - s.start .. b - s.start]);
        }
    }

    /// `assert_slabs_mirror_runs`: the slabs are exactly the union's runs.
    pub fn mirrors(self: *const ExtentOverlay, u: *const cov.CoverageUnion) bool {
        var runs: [cov.CoverageUnion.max_runs]cov.Run = undefined;
        const rs = u.runsSorted(&runs);
        if (rs.len != self.slabs.items.len) return false;
        for (rs, self.slabs.items) |r, s| if (r.start != s.start or r.end != s.end()) return false;
        return true;
    }
};

// ---------------------------------------------------------------------------
// The spill record (versioned + checksummed; NO seed read at spill).
// ---------------------------------------------------------------------------

pub const RECORD_MAGIC = "SQZX".*;
pub const EXTENT_RECORD_VERSION: u32 = 2;
pub const RECORD_FLAG_BASE_DEFERRED: u32 = 1 << 0;

/// 40 B little-endian header; then `count` × `(start u32, len u32)`, then the
/// payloads concatenated. The xxh3 covers header (field zeroed) ‖ table ‖
/// payloads.
pub const ExtentRecordWire = extern struct {
    magic: [4]u8,
    version_le: [4]u8,
    fencing_le: [8]u8,
    block_le: [4]u8,
    flags_le: [4]u8,
    count_le: [4]u8,
    reserved_le: [4]u8,
    xxh3_le: [8]u8,
};

pub const RECORD_HEADER_LEN: usize = @sizeOf(ExtentRecordWire);
pub const RECORD_ENTRY_LEN: usize = 8;

comptime {
    assert(RECORD_HEADER_LEN == 40);
    assert(@offsetOf(ExtentRecordWire, "xxh3_le") == 32);
}

pub const DecodeError = error{ Torn, FutureVersion, Malformed };

pub const Decoded = struct {
    fencing: u64,
    block: u32,
    base_deferred: bool,
    /// `(start, len)` ascending (a view into the record bytes).
    table: []align(1) const [2]u32,
    payloads: []const u8,

    /// Apply the record's extents onto `dst` (older than the RAM slabs).
    pub fn applyTo(self: Decoded, dst: []u8) void {
        var pos: usize = 0;
        for (self.table) |e| {
            @memcpy(dst[e[0] .. e[0] + e[1]], self.payloads[pos .. pos + e[1]]);
            pos += e[1];
        }
    }
};

pub fn encodedLen(x: *const ExtentOverlay) usize {
    return RECORD_HEADER_LEN + x.slabs.items.len * RECORD_ENTRY_LEN + @as(usize, @intCast(x.bytes));
}

/// Encode the overlay as a spill record into `out` (`≥ encodedLen`).
pub fn encode(x: *const ExtentOverlay, fencing: u64, block: u32, out: []u8) []u8 {
    const n = encodedLen(x);
    assert(out.len >= n);
    var h: ExtentRecordWire = undefined;
    h.magic = RECORD_MAGIC;
    std.mem.writeInt(u32, &h.version_le, EXTENT_RECORD_VERSION, .little);
    std.mem.writeInt(u64, &h.fencing_le, fencing, .little);
    std.mem.writeInt(u32, &h.block_le, block, .little);
    std.mem.writeInt(u32, &h.flags_le, if (x.base_deferred) RECORD_FLAG_BASE_DEFERRED else 0, .little);
    std.mem.writeInt(u32, &h.count_le, @intCast(x.slabs.items.len), .little);
    h.reserved_le = @splat(0);
    h.xxh3_le = @splat(0);
    @memcpy(out[0..RECORD_HEADER_LEN], std.mem.asBytes(&h));
    var pos: usize = RECORD_HEADER_LEN;
    for (x.slabs.items) |s| {
        std.mem.writeInt(u32, out[pos..][0..4], s.start, .little);
        std.mem.writeInt(u32, out[pos + 4 ..][0..4], @intCast(s.data.len), .little);
        pos += RECORD_ENTRY_LEN;
    }
    for (x.slabs.items) |s| {
        @memcpy(out[pos .. pos + s.data.len], s.data);
        pos += s.data.len;
    }
    checksum.stamp(out[0..n], @offsetOf(ExtentRecordWire, "xxh3_le"));
    stats.extent_spills.inc();
    stats.extent_spill_bytes.add(x.bytes);
    return out[0..n];
}

/// Total decode: torn (bad digest, short, geometry) / future-version /
/// malformed (unsorted or overlapping extents, an extent past `block_size`),
/// each counted and ignored loudly by the recovery scan.
pub fn decode(bytes: []const u8, block_size: u32) DecodeError!Decoded {
    if (bytes.len < RECORD_HEADER_LEN) return torn();
    const h: *align(1) const ExtentRecordWire = @ptrCast(bytes.ptr);
    if (!std.mem.eql(u8, &h.magic, &RECORD_MAGIC)) return torn();
    const version = std.mem.readInt(u32, &h.version_le, .little);
    if (version > EXTENT_RECORD_VERSION) {
        stats.extent_records_future_refused.inc();
        return error.FutureVersion;
    }
    if (version != EXTENT_RECORD_VERSION) return torn();
    const count = std.mem.readInt(u32, &h.count_le, .little);
    if (count > cov.MAX_EXTRA_RUNS + 1) return torn();
    const table_len = @as(usize, count) * RECORD_ENTRY_LEN;
    if (bytes.len < RECORD_HEADER_LEN + table_len) return torn();
    var payload_len: u64 = 0;
    var prev_end: u32 = 0;
    var i: usize = 0;
    while (i < count) : (i += 1) {
        const off = RECORD_HEADER_LEN + i * RECORD_ENTRY_LEN;
        const start = std.mem.readInt(u32, bytes[off..][0..4], .little);
        const len = std.mem.readInt(u32, bytes[off + 4 ..][0..4], .little);
        if (len == 0 or start < prev_end or (i != 0 and start == prev_end) or @as(u64, start) + len > block_size) {
            stats.extent_records_torn_discarded.inc();
            return error.Malformed;
        }
        prev_end = start + len;
        payload_len += len;
    }
    const total = RECORD_HEADER_LEN + table_len + payload_len;
    if (bytes.len < total) return torn();
    if (!checksum.verify(bytes[0..@intCast(total)], @offsetOf(ExtentRecordWire, "xxh3_le"))) return torn();
    const flags = std.mem.readInt(u32, &h.flags_le, .little);
    if (flags & ~RECORD_FLAG_BASE_DEFERRED != 0 or !std.mem.allEqual(u8, &h.reserved_le, 0)) return torn();
    stats.extent_records_recovered.inc();
    return .{
        .fencing = std.mem.readInt(u64, &h.fencing_le, .little),
        .block = std.mem.readInt(u32, &h.block_le, .little),
        .base_deferred = flags & RECORD_FLAG_BASE_DEFERRED != 0,
        .table = @as([*]align(1) const [2]u32, @ptrCast(bytes.ptr + RECORD_HEADER_LEN))[0..count],
        .payloads = bytes[RECORD_HEADER_LEN + table_len .. @intCast(total)],
    };
}

fn torn() DecodeError {
    stats.extent_records_torn_discarded.inc();
    return error.Torn;
}

// ---------------------------------------------------------------------------
// The fold.
// ---------------------------------------------------------------------------

/// Seed base in authority order: the caller resolved it (a staged FULL
/// sibling > the durable block image — only when the complement is owed —
/// > zeros for a hole, NO seed read); apply the record's extents (older)
/// then the RAM slabs (newest) into `out` (the whole block). Returns k.
pub fn fold(seed: ?[]const u8, record: ?Decoded, x: *const ExtentOverlay, out: []u8) u64 {
    if (seed) |s| {
        @memcpy(out, s[0..out.len]);
        stats.fold_seed_reads.inc();
    } else {
        @memset(out, 0);
    }
    var k: u64 = 0;
    if (record) |r| {
        r.applyTo(out);
        k += r.table.len;
    }
    x.layInto(out);
    k += x.slabs.items.len;
    stats.fold_passes.inc();
    stats.fold_fill.record(k);
    stats.fold_extents_folded.add(k);
    return k;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const BS: u32 = 64 * PAGE;

test "derivations: small = len×4 < bs; escalation at 25 %; FOLD_MAX_EXTENTS = bs/(16×PAGE) = 64 at 4 MiB; FOLD_MAX_BYTES = bs/4; amplification 2048/k + 2 (k = 32 ⇒ 66, k = 16 ⇒ 130 ≤ 150, k = 64 ⇒ 34)" {
    try testing.expect(isSmall(PAGE, BS));
    try testing.expect(!isSmall(BS / 4, BS));
    try testing.expect(!escalationDue(BS / 4 - 1, BS));
    try testing.expect(escalationDue(BS / 4, BS));
    try testing.expectEqual(@as(u64, 64), foldMaxExtentsDerived(4 << 20));
    try testing.expectEqual(@as(u64, 1 << 20), foldMaxBytesDerived(4 << 20));
    try testing.expectEqual(@as(u64, 64), resolveFoldMaxExtents(1000, 4 << 20)); // bounded by the table
    try testing.expectEqual(@as(u64, 7), resolveFoldMaxExtents(7, 4 << 20));
    try testing.expectEqual(@as(u64, 66), amplification(32));
    try testing.expectEqual(@as(u64, 130), amplification(16));
    try testing.expectEqual(@as(u64, 34), amplification(64));
}

test "small_nonadjacent_write_parks_compactly: slabs mirror the coverage runs exactly; an overlap rewrite within the overlay is newest-wins; the charge is net slab bytes; coveredRead zero-fills" {
    var x = ExtentOverlay.init(false);
    defer x.deinit(testing.allocator);
    var u: cov.CoverageUnion = .empty;
    var a: [PAGE]u8 = @splat(1);
    var b: [PAGE]u8 = @splat(2);
    try x.merge(testing.allocator, 10 * PAGE, &a);
    _ = try u.record(10 * PAGE, 11 * PAGE, BS);
    try x.merge(testing.allocator, 2 * PAGE, &b);
    _ = try u.record(2 * PAGE, 3 * PAGE, BS);
    try testing.expect(x.mirrors(&u));
    try testing.expectEqual(@as(u64, 2 * PAGE), x.bytes);
    try testing.expectEqual(@as(u64, 2 * PAGE), stats.parked_extent_bytes.load());
    // Abutting: coalesces into one slab.
    try x.merge(testing.allocator, 3 * PAGE, &a);
    _ = try u.record(3 * PAGE, 4 * PAGE, BS);
    try testing.expectEqual(@as(usize, 2), x.count());
    try testing.expect(x.mirrors(&u));
    // Overlap rewrite spanning both: newest wins over the whole window.
    var wide: [9 * PAGE]u8 = @splat(9);
    try x.merge(testing.allocator, 2 * PAGE + 100, &wide);
    _ = try u.record(2 * PAGE + 100, 11 * PAGE + 100, BS);
    try testing.expectEqual(@as(usize, 1), x.count());
    try testing.expect(x.mirrors(&u));
    try testing.expectEqual(@as(u64, 9 * PAGE + 100), x.bytes);
    var out: [12 * PAGE]u8 = undefined;
    x.coveredRead(0, &out);
    try testing.expect(std.mem.allEqual(u8, out[0 .. 2 * PAGE], 0));
    try testing.expectEqual(@as(u8, 2), out[2 * PAGE + 50]);
    try testing.expectEqual(@as(u8, 9), out[2 * PAGE + 100]);
    try testing.expectEqual(@as(u8, 9), out[11 * PAGE + 50]);
    try testing.expectEqual(@as(u8, 0), out[11 * PAGE + 100]);
    var laid: [BS]u8 = @splat(0xEE);
    x.layInto(&laid);
    try testing.expectEqual(@as(u8, 0xEE), laid[0]);
    try testing.expectEqual(@as(u8, 9), laid[5 * PAGE]);
}

test "spill_writes_extent_records_without_seed_reads: the record round-trips, every refusal class is detected (torn, future, malformed), the fold seeds once and applies record extents then slabs newest-wins" {
    var x = ExtentOverlay.init(true);
    defer x.deinit(testing.allocator);
    var a: [PAGE]u8 = @splat(0xA1);
    var b: [2 * PAGE]u8 = @splat(0xB2);
    try x.merge(testing.allocator, 5 * PAGE, &a);
    try x.merge(testing.allocator, 20 * PAGE, &b);
    const buf = try testing.allocator.alloc(u8, encodedLen(&x));
    defer testing.allocator.free(buf);
    const rec = encode(&x, 77, 3, buf);
    try testing.expectEqual(RECORD_HEADER_LEN + 2 * RECORD_ENTRY_LEN + 3 * PAGE, rec.len);
    const d = try decode(rec, BS);
    try testing.expectEqual(@as(u64, 77), d.fencing);
    try testing.expectEqual(@as(u32, 3), d.block);
    try testing.expect(d.base_deferred);
    try testing.expectEqual(@as(usize, 2), d.table.len);
    try testing.expectEqual([2]u32{ 5 * PAGE, PAGE }, d.table[0]);
    // Torn: a flipped payload byte.
    rec[rec.len - 1] ^= 1;
    try testing.expectError(error.Torn, decode(rec, BS));
    rec[rec.len - 1] ^= 1;
    // Future version refuses loud.
    std.mem.writeInt(u32, rec[4..8], EXTENT_RECORD_VERSION + 1, .little);
    try testing.expectError(error.FutureVersion, decode(rec, BS));
    std.mem.writeInt(u32, rec[4..8], EXTENT_RECORD_VERSION, .little);
    // Malformed: an extent past the block (the checksum is re-stamped so geometry is what refuses).
    std.mem.writeInt(u32, rec[RECORD_HEADER_LEN..][0..4], BS - 10, .little);
    checksum.stamp(rec, 32);
    try testing.expectError(error.Malformed, decode(rec, BS));
    try testing.expectError(error.Torn, decode(rec[0..20], BS));
    // The fold: seed once (an old image), record extents (older), then RAM slabs (newest).
    std.mem.writeInt(u32, rec[RECORD_HEADER_LEN..][0..4], 5 * PAGE, .little);
    checksum.stamp(rec, 32);
    const d2 = try decode(rec, BS);
    var newer = ExtentOverlay.init(true);
    defer newer.deinit(testing.allocator);
    var c: [PAGE]u8 = @splat(0xC3);
    try newer.merge(testing.allocator, 5 * PAGE + 100, &c); // overlaps the record's first extent: newest wins
    var seed: [BS]u8 = @splat(0x00);
    @memset(seed[0..], 0x11);
    var out: [BS]u8 = undefined;
    const reads0 = stats.fold_seed_reads.load();
    const k = fold(&seed, d2, &newer, &out);
    try testing.expectEqual(@as(u64, 3), k);
    try testing.expectEqual(reads0 + 1, stats.fold_seed_reads.load());
    try testing.expectEqual(@as(u8, 0x11), out[0]);
    try testing.expectEqual(@as(u8, 0xA1), out[5 * PAGE + 50]);
    try testing.expectEqual(@as(u8, 0xC3), out[5 * PAGE + 100]);
    try testing.expectEqual(@as(u8, 0xC3), out[6 * PAGE + 99]);
    try testing.expectEqual(@as(u8, 0x11), out[6 * PAGE + 100]);
    try testing.expectEqual(@as(u8, 0xB2), out[21 * PAGE]);
    // A hole folds with NO seed read (G-RW6).
    const k2 = fold(null, null, &newer, &out);
    try testing.expectEqual(@as(u64, 1), k2);
    try testing.expectEqual(reads0 + 1, stats.fold_seed_reads.load());
    try testing.expectEqual(@as(u8, 0), out[0]);
}

fn decodeTotalFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [RECORD_HEADER_LEN + 8 * RECORD_ENTRY_LEN + 4 * PAGE]u8 = undefined;
    const len = smith.slice(&buf);
    _ = decode(buf[0..len], BS) catch return;
}

test "fuzz: the spill record decoder never panics on arbitrary bytes" {
    try fuzzkit.fuzz({}, decodeTotalFuzz, .{});
}
