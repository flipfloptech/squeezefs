//! `DefragReport` and `--report-only`'s bounded JSON (spec §3.2): the four
//! axes + the pack face rendered into ONE `std.Io.Writer`; `writeBounded`
//! serves the longest fitting worst-occupancy prefix of the pack rows with
//! `rows_elided` (the PK7 rig: one row per legacy block refused the whole
//! admin reply) — a bounded reply is never a refusal. `--report-only`
//! computes and publishes, moves nothing. Portable.

const std = @import("std");
const metrics = @import("metrics.zig");

pub const D1Volume = struct {
    vol_tag: u64,
    contiguity_permille: u64,
    reclaimable_tail_permille: u64,
    total_free_blocks: u64,
    highest: u64,
};

pub const D4Volume = struct {
    index: u32,
    dead_bset_permille: u64,
    mergeable_leaves: u64,
};

pub const PackRow = struct {
    vol_tag: u64,
    block_idx: u64,
    occupancy_permille: u64,
    live_bytes: u64,
    referencers: u32,
    victim: bool,
};

pub const Report = struct {
    d1: []const D1Volume,
    d2_locality_permille: u64,
    d2_pairs: u64,
    d3_pressure_bytes: u64,
    d4: []const D4Volume,
    pack: metrics.PackSummary,
    /// Sorted worst occupancy first (the bounded prefix is the worst rows).
    pack_rows: []const PackRow,

    /// Publish the gauges (worst-volume semantics; permille + 1).
    pub fn publish(self: Report) void {
        var worst_c: u64 = metrics.PERMILLE;
        var worst_t: u64 = metrics.PERMILLE;
        for (self.d1) |v| {
            worst_c = @min(worst_c, v.contiguity_permille);
            worst_t = @min(worst_t, v.reclaimable_tail_permille);
        }
        metrics.publishD1(worst_c, worst_t);
        metrics.stats.frag_d2_locality.set(self.d2_locality_permille + 1);
        metrics.stats.frag_d3_pressure_bytes.set(self.d3_pressure_bytes);
        var worst_dead: u64 = 0;
        var leaves: u64 = 0;
        for (self.d4) |v| {
            worst_dead = @max(worst_dead, v.dead_bset_permille);
            leaves += v.mergeable_leaves;
        }
        metrics.stats.frag_d4_dead_bset_ratio.set(worst_dead + 1);
        metrics.stats.frag_d4_mergeable_leaves.set(leaves);
        metrics.publishPack(self.pack);
    }
};

fn writeHead(r: Report, w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{\"d1\":[");
    for (r.d1, 0..) |v, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("{{\"volume\":\"vol-{x:0>16}\",\"contiguity_permille\":{d},\"reclaimable_tail_permille\":{d},\"free_blocks\":{d},\"highest\":{d}}}", .{ v.vol_tag, v.contiguity_permille, v.reclaimable_tail_permille, v.total_free_blocks, v.highest });
    }
    try w.print("],\"d2\":{{\"locality_permille\":{d},\"pairs\":{d}}},\"d3\":{{\"pressure_bytes\":{d}}},\"d4\":[", .{ r.d2_locality_permille, r.d2_pairs, r.d3_pressure_bytes });
    for (r.d4, 0..) |v, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("{{\"meta_volume\":{d},\"dead_bset_permille\":{d},\"mergeable_leaves\":{d}}}", .{ v.index, v.dead_bset_permille, v.mergeable_leaves });
    }
    try w.print("],\"pack\":{{\"blocks\":{d},\"worst_occupancy_permille\":{d},\"mean_occupancy_permille\":{d},\"below_half\":{d},\"reclaimable_bytes\":{d},\"rows\":[", .{ r.pack.blocks, r.pack.worst_occupancy_permille, r.pack.mean_occupancy_permille, r.pack.below_half, r.pack.reclaimable_bytes });
}

fn writeRow(row: PackRow, w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"volume\":\"vol-{x:0>16}\",\"block\":{d},\"occupancy_permille\":{d},\"live_bytes\":{d},\"referencers\":{d},\"victim\":{}}}", .{ row.vol_tag, row.block_idx, row.occupancy_permille, row.live_bytes, row.referencers, row.victim });
}

/// The whole report (unbounded).
pub fn write(r: Report, w: *std.Io.Writer) std.Io.Writer.Error!void {
    try writeHead(r, w);
    for (r.pack_rows, 0..) |row, i| {
        if (i != 0) try w.writeByte(',');
        try writeRow(row, w);
    }
    try w.writeAll("],\"rows_elided\":0}}");
}

/// The tail every bounded rendering reserves room for.
const TAIL_MAX: usize = "],\"rows_elided\":18446744073709551615}}".len;

/// At most `max_bytes`: the head, then pack rows while they fit, then
/// `rows_elided`. Returns the bytes written; the head itself is bounded
/// by the volume count (≈ 120 B per volume).
pub fn writeBounded(r: Report, out: []u8) !usize {
    var w: std.Io.Writer = .fixed(out);
    try writeHead(r, &w);
    var kept: usize = 0;
    var row_buf: [192]u8 = undefined;
    for (r.pack_rows) |row| {
        var rw: std.Io.Writer = .fixed(&row_buf);
        try writeRow(row, &rw);
        const need = rw.buffered().len + @as(usize, if (kept != 0) 1 else 0);
        if (w.buffered().len + need + TAIL_MAX > out.len) break;
        if (kept != 0) try w.writeByte(',');
        try w.writeAll(rw.buffered());
        kept += 1;
    }
    try w.print("],\"rows_elided\":{d}}}}}", .{r.pack_rows.len - kept});
    return w.buffered().len;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn fixtureRows(gpa: std.mem.Allocator, n: usize) ![]PackRow {
    const rows = try gpa.alloc(PackRow, n);
    for (rows, 0..) |*r, i| r.* = .{ .vol_tag = 0xABC, .block_idx = i, .occupancy_permille = @intCast(i * 7 % 1000), .live_bytes = i * 4096, .referencers = @intCast(i % 5), .victim = i % 2 == 0 };
    return rows;
}

test "the report renders every axis and the pack rows; report-only publishes the gauges with worst-volume semantics (permille + 1)" {
    const gpa = testing.allocator;
    const rows = try fixtureRows(gpa, 3);
    defer gpa.free(rows);
    const r: Report = .{
        .d1 = &.{ .{ .vol_tag = 1, .contiguity_permille = 900, .reclaimable_tail_permille = 950, .total_free_blocks = 10, .highest = 20 }, .{ .vol_tag = 2, .contiguity_permille = 300, .reclaimable_tail_permille = 100, .total_free_blocks = 5, .highest = 9 } },
        .d2_locality_permille = 750,
        .d2_pairs = 40,
        .d3_pressure_bytes = 1 << 20,
        .d4 = &.{ .{ .index = 0, .dead_bset_permille = 120, .mergeable_leaves = 2 }, .{ .index = 1, .dead_bset_permille = 400, .mergeable_leaves = 5 } },
        .pack = .{ .blocks = 3, .worst_occupancy_permille = 0, .mean_occupancy_permille = 7, .below_half = 2, .reclaimable_bytes = 8 << 20 },
        .pack_rows = rows,
    };
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try write(r, &w);
    const out = w.buffered();
    try testing.expect(std.mem.indexOf(u8, out, "\"d1\":[{\"volume\":\"vol-0000000000000001\"") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"locality_permille\":750") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"rows_elided\":0}") != null);
    try testing.expect(std.mem.count(u8, out, "\"block\":") == 3);
    r.publish();
    try testing.expectEqual(@as(?u64, 300), metrics.decodeRatio(metrics.stats.frag_d1_contiguity.load()));
    try testing.expectEqual(@as(?u64, 100), metrics.decodeRatio(metrics.stats.frag_d1_reclaimable_tail.load()));
    try testing.expectEqual(@as(?u64, 750), metrics.decodeRatio(metrics.stats.frag_d2_locality.load()));
    try testing.expectEqual(@as(?u64, 400), metrics.decodeRatio(metrics.stats.frag_d4_dead_bset_ratio.load()));
    try testing.expectEqual(@as(u64, 7), metrics.stats.frag_d4_mergeable_leaves.load());
    try testing.expectEqual(@as(u64, 2), metrics.stats.pack_blocks_below_half.load());
}

test "defrag report serves a bounded view under the admin body cap: the longest fitting prefix of the worst rows with rows_elided; a cap below one row serves zero rows; the unbounded render fits with rows_elided 0" {
    const gpa = testing.allocator;
    const rows = try fixtureRows(gpa, 500);
    defer gpa.free(rows);
    const r: Report = .{ .d1 = &.{}, .d2_locality_permille = 1000, .d2_pairs = 0, .d3_pressure_bytes = 0, .d4 = &.{}, .pack = .{ .blocks = 500, .worst_occupancy_permille = 0, .mean_occupancy_permille = 500, .below_half = 250, .reclaimable_bytes = 0 }, .pack_rows = rows };
    var small: [1024]u8 = undefined;
    const n = try writeBounded(r, &small);
    try testing.expect(n <= small.len);
    const kept = std.mem.count(u8, small[0..n], "\"block\":");
    try testing.expect(kept > 0 and kept < 500);
    var expect_buf: [32]u8 = undefined;
    const tail = try std.fmt.bufPrint(&expect_buf, "\"rows_elided\":{d}}}}}", .{500 - kept});
    try testing.expect(std.mem.endsWith(u8, small[0..n], tail));
    var tiny: [300]u8 = undefined;
    const t = try writeBounded(r, &tiny);
    try testing.expectEqual(@as(usize, 0), std.mem.count(u8, tiny[0..t], "\"block\":"));
    try testing.expect(std.mem.endsWith(u8, tiny[0..t], "\"rows_elided\":500}}"));
    const big = try gpa.alloc(u8, 128 << 10);
    defer gpa.free(big);
    const b = try writeBounded(r, big);
    try testing.expect(std.mem.endsWith(u8, big[0..b], "\"rows_elided\":0}}"));
    try testing.expectEqual(@as(usize, 500), std.mem.count(u8, big[0..b], "\"block\":"));
}
