//! The shard plan of the project test runner (`tests/test_runner.zig`): which
//! tests of ONE compiled test binary a shard runs, so `build.zig` can run N
//! processes of the same binary side by side (docs/evidence/rung-10-gate-speed.md).
//!
//! THE PLAN. Every process of a shard set computes the same partition from the
//! same three inputs — the binary's test names (index order), the committed
//! cost table, the shard count — and never from a clock, so a crashed shard the
//! build runner respawns resumes the same list and two runs partition alike:
//!
//!   - `[venue-exclusive]` in a test's name puts it in the EXCLUSIVE class: no
//!     numbered shard runs it; the `exclusive` spec runs exactly those, in a
//!     step `build.zig` orders after every other step that uses the venue.
//!   - The rest go by greedy LPT (longest processing time first): sort by cost
//!     descending (index ascending on ties), give each test to the least-loaded
//!     shard (the lowest-numbered on ties). A test without a table row costs
//!     the median of the rows this binary matches (no rows at all: every test
//!     costs alike and LPT degenerates to round-robin by index).
//!   - Each shard runs its tests in index order: a shard's sequence is a
//!     subsequence of the unsharded run's.
//!
//! THE TABLE. One row per test, `<name>\t<wall µs>\t<cpu µs>`, what the
//! runner measured (`--costs-out`); `zig build costs` regenerates it from the
//! gate's own shard runs. The partition balances by the wall time; the CPU
//! time (the test process and the children it reaped) is evidence — a row
//! whose CPU is a sliver of its wall waits on something. A name may hold any
//! byte but a newline; the two numbers are its last two tab-separated fields.
//!
//! THE STALENESS RAIL. The partition is only as good as the table: a renamed
//! heavy test leaves its cost on a row no test matches and runs at the median
//! guess; a wave of new tests runs on guesses. `staleness` measures both
//! against the binary, deterministically (names only, never a timing), and
//! the runner refuses a stale table by name (`Staleness.isStale`).

const std = @import("std");
const Allocator = std.mem.Allocator;

/// The marker that declares a test venue-exclusive (module doc).
pub const exclusive_marker = "[venue-exclusive]";

pub fn isExclusive(name: []const u8) bool {
    return std.mem.indexOf(u8, name, exclusive_marker) != null;
}

pub const Spec = union(enum) {
    /// Every test, exclusive ones included (no `--shard`: a filtered dev run).
    all,
    /// Shard `index` (0-based) of `count` over the non-exclusive tests.
    part: struct { index: u32, count: u32 },
    /// The exclusive class alone.
    exclusive,

    /// `I/N` (1-based, `1 ≤ I ≤ N`) or `exclusive`.
    pub fn parse(text: []const u8) error{InvalidShard}!Spec {
        if (std.mem.eql(u8, text, "exclusive")) return .exclusive;
        const slash = std.mem.indexOfScalar(u8, text, '/') orelse return error.InvalidShard;
        const i = std.fmt.parseUnsigned(u32, text[0..slash], 10) catch return error.InvalidShard;
        const n = std.fmt.parseUnsigned(u32, text[slash + 1 ..], 10) catch return error.InvalidShard;
        if (n == 0 or i == 0 or i > n) return error.InvalidShard;
        return .{ .part = .{ .index = i - 1, .count = n } };
    }
};

pub const Costs = struct {
    /// Test name → measured wall time in microseconds. Keys borrow the text.
    map: std.StringHashMapUnmanaged(u64) = .empty,
    total_us: u64 = 0,

    /// Parse the table text (borrowed for the map's keys). A malformed row
    /// is an error, never a silent guess.
    pub fn parse(gpa: Allocator, text: []const u8) error{ OutOfMemory, MalformedCostRow, DuplicateCostRow }!Costs {
        var costs: Costs = .{};
        errdefer costs.map.deinit(gpa);
        var it = std.mem.splitScalar(u8, text, '\n');
        while (it.next()) |line| {
            if (line.len == 0) continue;
            const cpu_tab = std.mem.lastIndexOfScalar(u8, line, '\t') orelse return error.MalformedCostRow;
            _ = std.fmt.parseUnsigned(u64, line[cpu_tab + 1 ..], 10) catch return error.MalformedCostRow;
            const tab = std.mem.lastIndexOfScalar(u8, line[0..cpu_tab], '\t') orelse return error.MalformedCostRow;
            const us = std.fmt.parseUnsigned(u64, line[tab + 1 .. cpu_tab], 10) catch return error.MalformedCostRow;
            const gop = try costs.map.getOrPut(gpa, line[0..tab]);
            if (gop.found_existing) return error.DuplicateCostRow;
            gop.value_ptr.* = us;
            costs.total_us += us;
        }
        return costs;
    }

    pub fn deinit(costs: *Costs, gpa: Allocator) void {
        costs.map.deinit(gpa);
    }
};

pub const Staleness = struct {
    /// Tests of the binary with no row.
    unknown: usize,
    tests: usize,
    /// Cost on rows that name no test of the binary.
    orphan_us: u64,
    total_us: u64,

    /// A tenth: past it, the plan rests on guesses for more than one test in
    /// ten, or a tenth of the measured cost sits on tests that no longer exist
    /// (a renamed heavy row) and lands wherever the median guess puts it.
    pub const max_fraction_den = 10;

    pub fn isStale(s: Staleness) bool {
        return s.unknown * max_fraction_den > s.tests or s.orphan_us * max_fraction_den > s.total_us;
    }
};

pub fn staleness(gpa: Allocator, names: []const []const u8, costs: *const Costs) Allocator.Error!Staleness {
    var present: std.StringHashMapUnmanaged(void) = .empty;
    defer present.deinit(gpa);
    try present.ensureTotalCapacity(gpa, @intCast(names.len));
    var unknown: usize = 0;
    for (names) |n| {
        present.putAssumeCapacity(n, {});
        if (!costs.map.contains(n)) unknown += 1;
    }
    var orphan_us: u64 = 0;
    var it = costs.map.iterator();
    while (it.next()) |e| {
        if (!present.contains(e.key_ptr.*)) orphan_us += e.value_ptr.*;
    }
    return .{ .unknown = unknown, .tests = names.len, .orphan_us = orphan_us, .total_us = costs.total_us };
}

pub const Plan = struct {
    /// Indices into the binary's tests, ascending.
    indices: []u32,
    /// The plan's predicted wall time of this process (µs), from the table.
    predicted_us: u64,

    pub fn deinit(p: *Plan, gpa: Allocator) void {
        gpa.free(p.indices);
    }
};

/// The tests `spec` runs out of `names` (the binary's tests, index order).
pub fn plan(gpa: Allocator, names: []const []const u8, costs: ?*const Costs, spec: Spec) Allocator.Error!Plan {
    const cost = try testCosts(gpa, names, costs);
    defer gpa.free(cost);

    var picked: std.ArrayList(u32) = .empty;
    errdefer picked.deinit(gpa);
    var predicted: u64 = 0;
    switch (spec) {
        .all => for (names, 0..) |_, i| {
            try picked.append(gpa, @intCast(i));
            predicted += cost[i];
        },
        .exclusive => for (names, 0..) |n, i| if (isExclusive(n)) {
            try picked.append(gpa, @intCast(i));
            predicted += cost[i];
        },
        .part => |p| {
            var order: std.ArrayList(u32) = .empty;
            defer order.deinit(gpa);
            for (names, 0..) |n, i| if (!isExclusive(n)) try order.append(gpa, @intCast(i));
            std.mem.sort(u32, order.items, cost, struct {
                fn heavierFirst(c: []const u64, a: u32, b: u32) bool {
                    return c[a] > c[b] or (c[a] == c[b] and a < b);
                }
            }.heavierFirst);
            const loads = try gpa.alloc(u64, p.count);
            defer gpa.free(loads);
            @memset(loads, 0);
            for (order.items) |i| {
                const s = std.mem.indexOfMin(u64, loads);
                loads[s] += cost[i];
                if (s == p.index) try picked.append(gpa, i);
            }
            std.mem.sort(u32, picked.items, {}, std.sort.asc(u32));
            predicted = loads[p.index];
        },
    }
    return .{ .indices = try picked.toOwnedSlice(gpa), .predicted_us = predicted };
}

/// Each test's cost: its row, or the lower median of the rows this binary
/// matches (the module doc's fallback); 1 for every test when nothing
/// matches (all alike: LPT's index tie-break is then round-robin).
fn testCosts(gpa: Allocator, names: []const []const u8, costs: ?*const Costs) Allocator.Error![]u64 {
    const out = try gpa.alloc(u64, names.len);
    errdefer gpa.free(out);
    var known: std.ArrayList(u64) = .empty;
    defer known.deinit(gpa);
    for (names) |n| if (costs) |t| if (t.map.get(n)) |us| try known.append(gpa, us);
    std.mem.sort(u64, known.items, {}, std.sort.asc(u64));
    const fallback: u64 = if (known.items.len == 0) 1 else known.items[(known.items.len - 1) / 2];
    for (names, out) |n, *c| c.* = if (costs) |t| t.map.get(n) orelse fallback else fallback;
    return out;
}

/// The hang watchdog's limit for one test (`tests/test_runner.zig`
/// `hangwatch`): `k` × its row, clamped to [floor, cap]; a test without a row
/// gets the cap — its cost is unknown, and `zig build costs`, the step that
/// would measure it, runs under the same watchdog. The constants come from
/// 43,114 measured test runs on kamrui (every `--costs-out` file in its gate
/// cache, 2026-10-15/16; docs/evidence/rung-10-hangwatch.md).
pub const hang = struct {
    /// 3 runs passed 10× their row and 30 s; the only 2 past 20× were one stalled run (1019 s wall on 5 s CPU).
    pub const k = 20;
    /// No green gate-train run showed a row under 6 s past 45 s (meta.backend's D4 faces, a 5.15 s row): 2.7× margin.
    pub const floor_us = 120 * std.time.us_per_s;
    /// 3.7× the slowest row (the jobs matrix, 241 s); still fails a hung step in 15 min, not at the gate's 65.
    pub const cap_us = 900 * std.time.us_per_s;

    pub fn limitUs(row_us: ?u64) u64 {
        const row = row_us orelse return cap_us;
        return std.math.clamp(row *| k, floor_us, cap_us);
    }
};

const testing = std.testing;

test "hang limit: k × the row, never under the floor nor over the cap; no row is the cap" {
    const s = std.time.us_per_s;
    try testing.expectEqual(hang.floor_us, hang.limitUs(0));
    try testing.expectEqual(hang.floor_us, hang.limitUs(400_000)); // a 0.4 s row: 8 s < floor
    try testing.expectEqual(20 * 10 * s, hang.limitUs(10 * s)); // the 10 s storms: 200 s
    try testing.expectEqual(20 * 28 * s, hang.limitUs(28 * s)); // the slowest unit row: 560 s
    try testing.expectEqual(hang.cap_us, hang.limitUs(241 * s)); // the jobs matrix: 4820 s → cap
    try testing.expectEqual(hang.cap_us, hang.limitUs(std.math.maxInt(u64)));
    try testing.expectEqual(hang.cap_us, hang.limitUs(null));
}

test "spec: I/N is 1-based, exclusive is a word, anything else refuses" {
    try testing.expectEqual(Spec{ .part = .{ .index = 2, .count = 6 } }, try Spec.parse("3/6"));
    try testing.expectEqual(Spec.exclusive, try Spec.parse("exclusive"));
    for ([_][]const u8{ "0/6", "7/6", "1/0", "3", "a/b", "", "/", "3/6/1" }) |bad| {
        try testing.expectError(error.InvalidShard, Spec.parse(bad));
    }
}

test "costs: wall and cpu are the last two fields; a malformed or duplicate row refuses" {
    const gpa = testing.allocator;
    var c = try Costs.parse(gpa, "a\tb\t12\t7\nc\t3\t0\n\n");
    defer c.deinit(gpa);
    try testing.expectEqual(@as(?u64, 12), c.map.get("a\tb"));
    try testing.expectEqual(@as(?u64, 3), c.map.get("c"));
    try testing.expectEqual(@as(u64, 15), c.total_us);
    try testing.expectError(error.MalformedCostRow, Costs.parse(gpa, "one-tab\t0\n"));
    try testing.expectError(error.MalformedCostRow, Costs.parse(gpa, "x\t12ms\t0\n"));
    try testing.expectError(error.MalformedCostRow, Costs.parse(gpa, "x\t12\t-\n"));
    try testing.expectError(error.DuplicateCostRow, Costs.parse(gpa, "x\t1\t0\nx\t2\t0\n"));
}

test "plan: the shards partition the non-exclusive tests; the exclusive spec runs exactly the marked ones; all runs everything" {
    const gpa = testing.allocator;
    const names = [_][]const u8{ "a", "b [venue-exclusive]", "c", "d", "e", "f", "g" };
    var c = try Costs.parse(gpa, "a\t50\t0\nc\t40\t0\nd\t30\t0\ne\t20\t0\nf\t10\t0\ng\t10\t0\nb [venue-exclusive]\t99\t0\n");
    defer c.deinit(gpa);
    for ([_]u32{ 1, 2, 3, 7 }) |n| {
        var seen: [names.len]u8 = @splat(0);
        var sum: u64 = 0;
        for (0..n) |i| {
            var p = try plan(gpa, &names, &c, .{ .part = .{ .index = @intCast(i), .count = n } });
            defer p.deinit(gpa);
            for (p.indices, 0..) |t, k| {
                seen[t] += 1;
                if (k > 0) try testing.expect(p.indices[k - 1] < t);
            }
            sum += p.predicted_us;
        }
        try testing.expectEqualSlices(u8, &.{ 1, 0, 1, 1, 1, 1, 1 }, &seen);
        try testing.expectEqual(@as(u64, 160), sum);
    }
    var x = try plan(gpa, &names, &c, .exclusive);
    defer x.deinit(gpa);
    try testing.expectEqualSlices(u32, &.{1}, x.indices);
    var all = try plan(gpa, &names, &c, .all);
    defer all.deinit(gpa);
    try testing.expectEqual(names.len, all.indices.len);
}

test "plan: LPT — heaviest first onto the least-loaded shard, index order within a shard" {
    const gpa = testing.allocator;
    const names = [_][]const u8{ "a", "b", "c", "d", "e" };
    var c = try Costs.parse(gpa, "a\t10\t0\nb\t70\t0\nc\t30\t0\nd\t30\t0\ne\t20\t0\n");
    defer c.deinit(gpa);
    // b(70)→0 · c(30)→1 · d(30)→1 (30 < 70) · e(20)→1 (60 < 70) · a(10)→0 (70 < 80).
    var p0 = try plan(gpa, &names, &c, .{ .part = .{ .index = 0, .count = 2 } });
    defer p0.deinit(gpa);
    var p1 = try plan(gpa, &names, &c, .{ .part = .{ .index = 1, .count = 2 } });
    defer p1.deinit(gpa);
    try testing.expectEqualSlices(u32, &.{ 0, 1 }, p0.indices);
    try testing.expectEqualSlices(u32, &.{ 2, 3, 4 }, p1.indices);
    try testing.expectEqual(@as(u64, 80), p0.predicted_us);
    try testing.expectEqual(@as(u64, 80), p1.predicted_us);
}

test "plan: a test without a row costs the median of the matched rows; no table at all is round-robin by index" {
    const gpa = testing.allocator;
    const names = [_][]const u8{ "a", "b", "c", "new" };
    var c = try Costs.parse(gpa, "a\t5\t0\nb\t7\t0\nc\t100\t0\n");
    defer c.deinit(gpa);
    var p = try plan(gpa, &names, &c, .all);
    defer p.deinit(gpa);
    try testing.expectEqual(@as(u64, 5 + 7 + 100 + 7), p.predicted_us);
    for (0..3) |i| {
        var r = try plan(gpa, &names, null, .{ .part = .{ .index = @intCast(i), .count = 3 } });
        defer r.deinit(gpa);
        const want: []const u32 = switch (i) {
            0 => &.{ 0, 3 },
            1 => &.{1},
            else => &.{2},
        };
        try testing.expectEqualSlices(u32, want, r.indices);
    }
}

test "staleness: more than a tenth of the tests unknown, or of the cost orphaned, is stale" {
    const gpa = testing.allocator;
    var c = try Costs.parse(gpa, "t0\t10\t0\nt1\t10\t0\nt2\t10\t0\nt3\t10\t0\nt4\t10\t0\nt5\t10\t0\nt6\t10\t0\nt7\t10\t0\nt8\t10\t0\ngone\t10\t0\n");
    defer c.deinit(gpa);
    const ten = [_][]const u8{ "t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8", "t9" };
    const s = try staleness(gpa, &ten, &c);
    try testing.expectEqual(@as(usize, 1), s.unknown);
    try testing.expectEqual(@as(u64, 10), s.orphan_us);
    try testing.expect(!s.isStale()); // exactly a tenth each: still fresh
    const nine = ten[0..9];
    var heavy = try Costs.parse(gpa, "t0\t10\t0\nt1\t10\t0\nt2\t10\t0\nt3\t10\t0\nt4\t10\t0\nt5\t10\t0\nt6\t10\t0\nt7\t10\t0\nt8\t10\t0\ngone\t200\t0\n");
    defer heavy.deinit(gpa);
    try testing.expect((try staleness(gpa, nine, &heavy)).isStale()); // a renamed heavy row
    const many = [_][]const u8{ "t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8", "n1", "n2" };
    try testing.expect((try staleness(gpa, &many, &c)).isStale()); // two in eleven unknown
}
