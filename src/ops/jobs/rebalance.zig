//! `plan_rebalance` (charter §8 "auto-rebalance to within 10 pp of mean
//! fill"; spec §2.4): rows = placement-eligible volumes with a capacity
//! bound; `mean = Σused / Σcap`. UNDER = rows with `fill < mean − 10 pp`,
//! each with `deficit = (mean − 10 pp) × cap − used`, largest first; OVER =
//! rows with `fill > mean`, `surplus = used − mean × cap`, fullest first.
//! Legs are greedy: for each under-filled destination take `min(want,
//! have)` from the current over-filled source, advancing sources as they
//! exhaust — never pushing a source below the mean. ONE bounded pass then
//! Completed; continuous drift prevention is the router's balance penalty.
//! Integer ppm arithmetic (`router.fillPpm`). Portable.

const std = @import("std");
const router = @import("../../data/router.zig");

const PPM = router.PPM;

/// The charter's "within 10 pp of mean fill".
pub const REBALANCE_BAND_PPM: u64 = 100_000;
/// The conservative auto-pass `add-data` schedules.
pub const REBALANCE_DEFAULT_THROTTLE_PCT: u8 = 25;

pub const Row = struct {
    vol_tag: u64,
    used_bytes: u64,
    capacity_bytes: u64,
};

/// `blocks` from `src` to `dst`.
pub const Leg = struct {
    src: u64,
    dst: u64,
    blocks: u64,
};

const Side = struct { vol_tag: u64, blocks: u64 };

fn moreBlocks(_: void, a: Side, b: Side) bool {
    if (a.blocks != b.blocks) return a.blocks > b.blocks;
    return a.vol_tag < b.vol_tag;
}

/// `< 2` rows or `Σcap = 0` ⇒ no legs. Output owned by `gpa`.
pub fn planLegs(gpa: std.mem.Allocator, rows: []const Row, block_size: u64) std.mem.Allocator.Error![]Leg {
    std.debug.assert(block_size > 0);
    var sum_used: u128 = 0;
    var sum_cap: u128 = 0;
    var bounded: usize = 0;
    for (rows) |r| {
        if (r.capacity_bytes == 0) continue;
        bounded += 1;
        sum_used += r.used_bytes;
        sum_cap += r.capacity_bytes;
    }
    if (bounded < 2 or sum_cap == 0) return &.{};
    const mean_ppm: u64 = @intCast(@min(sum_used * PPM / sum_cap, PPM));
    const floor_ppm = mean_ppm -| REBALANCE_BAND_PPM;

    var under: std.ArrayList(Side) = .empty;
    defer under.deinit(gpa);
    var over: std.ArrayList(Side) = .empty;
    defer over.deinit(gpa);
    for (rows) |r| {
        if (r.capacity_bytes == 0) continue;
        const fill = router.fillPpm(r.used_bytes, r.capacity_bytes);
        if (fill < floor_ppm) {
            const target: u64 = @intCast(@as(u128, r.capacity_bytes) * floor_ppm / PPM);
            const deficit = (target -| r.used_bytes) / block_size;
            if (deficit > 0) try under.append(gpa, .{ .vol_tag = r.vol_tag, .blocks = deficit });
        } else if (fill > mean_ppm) {
            const at_mean: u64 = @intCast(@as(u128, r.capacity_bytes) * mean_ppm / PPM);
            const surplus = (r.used_bytes -| at_mean) / block_size;
            if (surplus > 0) try over.append(gpa, .{ .vol_tag = r.vol_tag, .blocks = surplus });
        }
    }
    std.mem.sort(Side, under.items, {}, moreBlocks);
    std.mem.sort(Side, over.items, {}, moreBlocks);

    var legs: std.ArrayList(Leg) = .empty;
    errdefer legs.deinit(gpa);
    var si: usize = 0;
    for (under.items) |d| {
        var want = d.blocks;
        while (want > 0 and si < over.items.len) {
            const s = &over.items[si];
            if (s.blocks == 0) {
                si += 1;
                continue;
            }
            const take = @min(want, s.blocks);
            try legs.append(gpa, .{ .src = s.vol_tag, .dst = d.vol_tag, .blocks = take });
            s.blocks -= take;
            want -= take;
        }
    }
    return legs.toOwnedSlice(gpa);
}

/// The per-source block budget the census is clipped to (`bound_rebalance_
/// census`): Σ legs from `src`.
pub fn sourceBudget(legs: []const Leg, src: u64) u64 {
    var n: u64 = 0;
    for (legs) |l| if (l.src == src) {
        n += l.blocks;
    };
    return n;
}

/// The `dest_hint` for a task sourced at `src`: the first leg with
/// remaining budget (the caller decrements as tasks are tagged).
pub fn destHint(legs: []const Leg, src: u64) ?u64 {
    for (legs) |l| if (l.src == src and l.blocks > 0) return l.dst;
    return null;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "plan_rebalance tie test: a 70/40/10 % set at equal capacities yields legs from the 70 % volume to the 10 % one of exactly (mean − 0.10) × cap − used blocks and none to the 40 % one" {
    const gpa = testing.allocator;
    const block: u64 = 4 << 20;
    const cap: u64 = 1000 * block;
    const rows = [_]Row{
        .{ .vol_tag = 70, .used_bytes = 700 * block, .capacity_bytes = cap },
        .{ .vol_tag = 40, .used_bytes = 400 * block, .capacity_bytes = cap },
        .{ .vol_tag = 10, .used_bytes = 100 * block, .capacity_bytes = cap },
    };
    const legs = try planLegs(gpa, &rows, block);
    defer gpa.free(legs);
    // mean = 40 %; floor = 30 %; deficit(10 %) = 300 − 100 = 200 blocks;
    // surplus(70 %) = 700 − 400 = 300 ≥ 200: one leg of 200.
    try testing.expectEqual(@as(usize, 1), legs.len);
    try testing.expectEqual(Leg{ .src = 70, .dst = 10, .blocks = 200 }, legs[0]);
    try testing.expectEqual(@as(u64, 200), sourceBudget(legs, 70));
    try testing.expectEqual(@as(?u64, 10), destHint(legs, 70));
    try testing.expectEqual(@as(?u64, null), destHint(legs, 40));
}

test "legs are greedy across sources and never push a source below the mean; < 2 bounded rows or Σcap 0 plan nothing; a set already within the band plans nothing" {
    const gpa = testing.allocator;
    const block: u64 = 1 << 20;
    const cap: u64 = 1000 * block;
    // Two over (90, 60), two under (0, 10): mean 40 %, floor 30 %.
    const rows = [_]Row{
        .{ .vol_tag = 1, .used_bytes = 900 * block, .capacity_bytes = cap },
        .{ .vol_tag = 2, .used_bytes = 600 * block, .capacity_bytes = cap },
        .{ .vol_tag = 3, .used_bytes = 0, .capacity_bytes = cap },
        .{ .vol_tag = 4, .used_bytes = 100 * block, .capacity_bytes = cap },
        .{ .vol_tag = 5, .used_bytes = 0, .capacity_bytes = 0 }, // unbounded: ignored
    };
    const legs = try planLegs(gpa, &rows, block);
    defer gpa.free(legs);
    // deficits: v3 300, v4 200; surpluses: v1 500, v2 200.
    try testing.expectEqual(@as(usize, 2), legs.len);
    try testing.expectEqual(Leg{ .src = 1, .dst = 3, .blocks = 300 }, legs[0]);
    try testing.expectEqual(Leg{ .src = 1, .dst = 4, .blocks = 200 }, legs[1]);
    var moved_from_1: u64 = 0;
    for (legs) |l| if (l.src == 1) {
        moved_from_1 += l.blocks;
    };
    try testing.expect(900 - moved_from_1 >= 400); // never below the mean
    // Exhausting the first source advances to the second.
    const rows2 = [_]Row{
        .{ .vol_tag = 1, .used_bytes = 500 * block, .capacity_bytes = cap },
        .{ .vol_tag = 2, .used_bytes = 500 * block, .capacity_bytes = cap },
        .{ .vol_tag = 3, .used_bytes = 0, .capacity_bytes = cap },
    };
    const legs2 = try planLegs(gpa, &rows2, block);
    defer gpa.free(legs2);
    // mean 33.3 %; floor 23.3 %: deficit 233; surplus each 500 − 333 = 166 (floor).
    try testing.expectEqual(@as(usize, 2), legs2.len);
    try testing.expectEqual(@as(u64, 1), legs2[0].src);
    try testing.expectEqual(@as(u64, 2), legs2[1].src);
    try testing.expectEqual(@as(u64, 233), legs2[0].blocks + legs2[1].blocks);
    const one = [_]Row{.{ .vol_tag = 1, .used_bytes = 10, .capacity_bytes = cap }};
    try testing.expectEqual(@as(usize, 0), (try planLegs(gpa, &one, block)).len);
    const balanced = [_]Row{
        .{ .vol_tag = 1, .used_bytes = 450 * block, .capacity_bytes = cap },
        .{ .vol_tag = 2, .used_bytes = 350 * block, .capacity_bytes = cap },
    };
    try testing.expectEqual(@as(usize, 0), (try planLegs(gpa, &balanced, block)).len);
    try testing.expectEqual(@as(u8, 25), REBALANCE_DEFAULT_THROTTLE_PCT);
    try testing.expectEqual(@as(u64, 100_000), REBALANCE_BAND_PPM);
}
