//! The census planner (spec §2.5.2): walk every live inode's layout
//! entries, keep those whose mapping names a SOURCE volume, group by
//! distinct base block `(vol_tag, block_idx)` → one `Task` per block with
//! every referencer (`refs` sorted `(ino, block_index)` ascending). A
//! SHARED block is one task with many refs — it moves ONCE (charter §8).
//! `distinct_blocks` is the preflight's `needed` numerator and the
//! convergence ledger's denominator (`moved + superseded + remaining ≡
//! distinct_blocks` at each re-plan). The walk is a seam the plane binds
//! (its inode-plane walk is the one fsck C8 needs); tests bind a RAM map.
//! Portable.

const std = @import("std");
const Allocator = std.mem.Allocator;
const layout = @import("../../data/layout.zig");

pub const Mapping = layout.Mapping;

/// One referencer of a base block: the mapping VERBATIM (the decoration
/// `rel_off/len` is preserved by the republish — the "same mapping shape"
/// law).
pub const Ref = struct {
    ino: u64,
    block_index: u32,
    mapping: Mapping,
};

/// One distinct base block and everything that references it.
pub const Task = struct {
    vol_tag: u64,
    block_idx: u64,
    /// The base's offset and incarnation (from the first referencer).
    offset: u64,
    incarnation: u64,
    refs: []Ref,
    /// The rebalance planner's destination hint (0 = none).
    dest_hint: u64 = 0,

    pub fn base(self: Task) Mapping {
        return Mapping.wholeBlock(self.vol_tag, self.offset, self.incarnation);
    }

    pub fn isShared(self: Task) bool {
        return self.refs.len > 1;
    }
};

pub const Plan = struct {
    tasks: []Task,
    distinct_blocks: u64,

    pub fn deinit(self: *Plan, gpa: Allocator) void {
        for (self.tasks) |t| gpa.free(t.refs);
        gpa.free(self.tasks);
        self.* = undefined;
    }
};

/// The layout walk: every `(ino, block_index, mapping)` of every live
/// inode, in any order; `visit` returns false to stop.
pub const LayoutWalk = struct {
    ctx: *anyopaque,
    walk: *const fn (ctx: *anyopaque, visitor: Visitor) error{ Io, OutOfMemory }!void,

    pub const Visitor = struct {
        ctx: *anyopaque,
        visit: *const fn (ctx: *anyopaque, r: Ref) error{OutOfMemory}!void,
    };
};

const Key = struct { vol_tag: u64, block_idx: u64 };

const Collector = struct {
    gpa: Allocator,
    sources: []const u64,
    chunk: u64,
    groups: std.AutoArrayHashMapUnmanaged(Key, std.ArrayList(Ref)) = .empty,

    fn isSource(self: *const Collector, vol_tag: u64) bool {
        for (self.sources) |s| if (s == vol_tag) return true;
        return false;
    }

    fn visit(ctx: *anyopaque, r: Ref) error{OutOfMemory}!void {
        const self: *Collector = @ptrCast(@alignCast(ctx));
        if (r.mapping.damaged or !self.isSource(r.mapping.vol_tag)) return;
        const k: Key = .{ .vol_tag = r.mapping.vol_tag, .block_idx = r.mapping.blockIdx(self.chunk) };
        const gop = try self.groups.getOrPut(self.gpa, k);
        if (!gop.found_existing) gop.value_ptr.* = .empty;
        try gop.value_ptr.append(self.gpa, r);
    }
};

fn refLess(_: void, a: Ref, b: Ref) bool {
    if (a.ino != b.ino) return a.ino < b.ino;
    return a.block_index < b.block_index;
}

fn taskLess(_: void, a: Task, b: Task) bool {
    if (a.vol_tag != b.vol_tag) return a.vol_tag < b.vol_tag;
    return a.block_idx < b.block_idx;
}

/// The census over `sources`: tasks sorted `(vol_tag, block_idx)`, refs
/// sorted `(ino, block_index)`.
pub fn censusFor(gpa: Allocator, walk: LayoutWalk, sources: []const u64, chunk: u64) error{ Io, OutOfMemory }!Plan {
    var c: Collector = .{ .gpa = gpa, .sources = sources, .chunk = chunk };
    defer {
        for (c.groups.values()) |*l| l.deinit(gpa);
        c.groups.deinit(gpa);
    }
    try walk.walk(walk.ctx, .{ .ctx = &c, .visit = &Collector.visit });
    var tasks: std.ArrayList(Task) = .empty;
    errdefer {
        for (tasks.items) |t| gpa.free(t.refs);
        tasks.deinit(gpa);
    }
    var it = c.groups.iterator();
    while (it.next()) |kv| {
        const refs = try kv.value_ptr.toOwnedSlice(gpa);
        std.mem.sort(Ref, refs, {}, refLess);
        try tasks.append(gpa, .{ .vol_tag = kv.key_ptr.vol_tag, .block_idx = kv.key_ptr.block_idx, .offset = refs[0].mapping.offset, .incarnation = refs[0].mapping.incarnation, .refs = refs });
    }
    std.mem.sort(Task, tasks.items, {}, taskLess);
    const n = tasks.items.len;
    return .{ .tasks = try tasks.toOwnedSlice(gpa), .distinct_blocks = n };
}

/// `bound_rebalance_census`: clip the tasks to the legs' per-source
/// budgets and tag each kept task with its `dest_hint`. Tasks are kept in
/// census order; the surplus is dropped (owned slice returned).
pub fn boundToLegs(gpa: Allocator, plan: *Plan, legs: []const @import("../jobs/rebalance.zig").Leg) Allocator.Error!void {
    const rebalance = @import("../jobs/rebalance.zig");
    const budgets = try gpa.dupe(rebalance.Leg, legs);
    defer gpa.free(budgets);
    var kept: std.ArrayList(Task) = .empty;
    errdefer kept.deinit(gpa);
    for (plan.tasks) |t| {
        var hint: ?u64 = null;
        for (budgets) |*l| if (l.src == t.vol_tag and l.blocks > 0) {
            l.blocks -= 1;
            hint = l.dst;
            break;
        };
        if (hint) |h| {
            var kt = t;
            kt.dest_hint = h;
            try kept.append(gpa, kt);
        } else {
            gpa.free(t.refs);
        }
    }
    gpa.free(plan.tasks);
    plan.tasks = try kept.toOwnedSlice(gpa);
    plan.distinct_blocks = plan.tasks.len;
}

// ---------------------------------------------------------------------------
// A RAM walk and the tests.
// ---------------------------------------------------------------------------

pub const RamWalk = struct {
    refs: []const Ref,

    pub fn walk(self: *RamWalk) LayoutWalk {
        return .{ .ctx = self, .walk = &run };
    }

    fn run(ctx: *anyopaque, v: LayoutWalk.Visitor) error{ Io, OutOfMemory }!void {
        const self: *RamWalk = @ptrCast(@alignCast(ctx));
        for (self.refs) |r| try v.visit(v.ctx, r);
    }
};

const testing = std.testing;
const CHUNK: u64 = 4 << 20;

test "the census groups by distinct base block across the sources only, orders refs (ino, block_index), counts a clone-shared block ONCE, preserves the tenant decoration verbatim and skips damaged markers" {
    const gpa = testing.allocator;
    const refs = [_]Ref{
        .{ .ino = 9, .block_index = 1, .mapping = Mapping.wholeBlock(1, 2 * CHUNK, 5) },
        .{ .ino = 5, .block_index = 0, .mapping = Mapping.wholeBlock(1, 2 * CHUNK, 5) }, // the clone share
        .{ .ino = 5, .block_index = 1, .mapping = Mapping.wholeBlock(2, 7 * CHUNK, 1) }, // another volume
        .{ .ino = 6, .block_index = 0, .mapping = Mapping.tenant(1, 0, 3, 8192, 1000) }, // a packed tenant
        .{ .ino = 7, .block_index = 0, .mapping = Mapping.tenant(1, 0, 3, 0, 4096) },
        .{ .ino = 8, .block_index = 2, .mapping = .{ .vol_tag = 1, .offset = 9 * CHUNK, .damaged = true, .whole_block = true } },
    };
    var rw: RamWalk = .{ .refs = &refs };
    var plan = try censusFor(gpa, rw.walk(), &.{1}, CHUNK);
    defer plan.deinit(gpa);
    try testing.expectEqual(@as(u64, 2), plan.distinct_blocks);
    try testing.expectEqual(@as(u64, 0), plan.tasks[0].block_idx);
    try testing.expectEqual(@as(usize, 2), plan.tasks[0].refs.len);
    try testing.expectEqual(@as(u64, 6), plan.tasks[0].refs[0].ino);
    try testing.expectEqual(@as(u32, 8192), plan.tasks[0].refs[0].mapping.rel_off);
    try testing.expectEqual(@as(u64, 2), plan.tasks[1].block_idx);
    try testing.expect(plan.tasks[1].isShared());
    try testing.expectEqual(@as(u64, 5), plan.tasks[1].refs[0].ino);
    try testing.expectEqual(@as(u64, 9), plan.tasks[1].refs[1].ino);
    try testing.expect(plan.tasks[1].base().whole_block);
    // Two sources: the other volume's block joins.
    var both = try censusFor(gpa, rw.walk(), &.{ 1, 2 }, CHUNK);
    defer both.deinit(gpa);
    try testing.expectEqual(@as(u64, 3), both.distinct_blocks);
    try testing.expectEqual(@as(u64, 2), both.tasks[2].vol_tag);
}

test "boundToLegs clips the census to the legs' per-source budgets and tags dest hints in census order" {
    const gpa = testing.allocator;
    var refs: [6]Ref = undefined;
    for (&refs, 0..) |*r, i| r.* = .{ .ino = 1, .block_index = @intCast(i), .mapping = Mapping.wholeBlock(1, i * CHUNK, 1) };
    var rw: RamWalk = .{ .refs = &refs };
    var plan = try censusFor(gpa, rw.walk(), &.{1}, CHUNK);
    defer plan.deinit(gpa);
    const rebalance = @import("../jobs/rebalance.zig");
    const legs = [_]rebalance.Leg{ .{ .src = 1, .dst = 2, .blocks = 2 }, .{ .src = 1, .dst = 3, .blocks = 1 } };
    try boundToLegs(gpa, &plan, &legs);
    try testing.expectEqual(@as(usize, 3), plan.tasks.len);
    try testing.expectEqual(@as(u64, 2), plan.tasks[0].dest_hint);
    try testing.expectEqual(@as(u64, 2), plan.tasks[1].dest_hint);
    try testing.expectEqual(@as(u64, 3), plan.tasks[2].dest_hint);
    try testing.expectEqual(@as(u64, 3), plan.distinct_blocks);
}
