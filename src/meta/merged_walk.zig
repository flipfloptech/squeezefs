//! The merged range over the metadata set's slot trees (charter §5.5: "a
//! record's slot is `key_ino >> 40` (refs by their owner ino)") as a k-way
//! merge that FETCHES a tree only once the merge front reaches the least
//! key the routing law lets that tree hold (`key.slotTreeLowerBound`): an
//! ino-major key of forest slot f lies in `[f << 40, (f + 1) << 40)`, a refs
//! key leads with 0x06 above every ino-major key, in whichever tree its
//! owner routes to. A walk over the ino-major space so reads the slot trees
//! one after another; a refs window reads every tree. Each tree is read in
//! chunks that start at `ceil(max / T)` and double per refill, capped at
//! what the call still needs.
//!
//! The law it replaces read `max` entries from EVERY tree per call and kept
//! `max`: T × the page per page, a full scan quadratic up to T × page
//! entries (64 slot trees × 512 — fsck 2.7 s for 32 k entries in one flat
//! directory, 83.5 s for 53 k in a striped one; rung-10 gate-speed §2.2).
//!
//! The bound (`readBound`, `fetchBound`; tie tests below), for a call over
//! T sources that consults `p` of them and takes `e`: reads ≤ 2e + p ×
//! ceil(max / T) when it fills `max` and exactly e when it ends short (every
//! consulted source reached the window's end); fetches ≤ p × (1 + ⌈log2(T +
//! 1)⌉). A source is fetched from its bound, so the output is in key order
//! whatever the sources hold: a key a tree holds BELOW its bound (outside
//! its slot's window — unreachable by every lookup, which routes by key) is
//! not visited; the law-free walk (every bound = the start — C1's raw walk)
//! visits it in order, consulting every tree per call.

const std = @import("std");
const Allocator = std.mem.Allocator;

const key = @import("key.zig");
const tree = @import("tree.zig");

/// The first chunk a consulted source is asked for: an even share of the
/// call's `max` over its T sources.
pub fn firstChunk(max: usize, sources: usize) usize {
    return std.math.divCeil(usize, max, @max(sources, 1)) catch unreachable;
}

/// Entries one call may read for the `consumed` it took (delivered or
/// dropped): every refill of a source is taken whole before the next, and
/// a source's last chunk wastes at most its first chunk plus what it gave
/// (chunks double), so `2e + p × c0` — exactly `e` when the call ended
/// short of `max`.
pub fn readBound(consumed: usize, max: usize, consulted: usize, sources: usize) usize {
    if (consumed < max) return consumed;
    return 2 * consumed + consulted * firstChunk(max, sources);
}

/// Fetches one call may make: a source's chunks double from `c0 ≥ max / T`
/// and all but its last are taken whole, so it fetches at most `1 +
/// ⌈log2(T + 1)⌉` times.
pub fn fetchBound(consulted: usize, sources: usize) usize {
    return consulted * (1 + @as(usize, std.math.log2_int_ceil(usize, sources + 1)));
}

/// One source's buffered share of a call.
const Stream = struct {
    buf: std.ArrayList(tree.KeyValue) = .empty,
    /// `buf[0..pos]` were taken (delivered or dropped); the rest are owned.
    pos: usize = 0,
    chunk: usize,
    /// The next fetch's start: the successor of the last key fetched.
    next_from: key.KeyBuf = .{},
    /// The last fetch returned fewer than asked: the window holds no more.
    exhausted: bool = false,

    fn head(self: *const Stream) []const u8 {
        return self.buf.items[self.pos].key;
    }

    fn pending(self: *const Stream) bool {
        return self.pos < self.buf.items.len;
    }

    fn deinit(self: *Stream, gpa: Allocator) void {
        for (self.buf.items[self.pos..]) |*kv| kv.deinit(gpa);
        self.buf.deinit(gpa);
    }
};

fn headOrder(streams: []Stream, a: u32, b: u32) std.math.Order {
    const o = key.order(streams[a].head(), streams[b].head());
    return if (o != .eq) o else std.math.order(a, b);
}

/// The error set of a source's `fetch`.
fn FetchError(comptime S: type) type {
    return @typeInfo(@typeInfo(@TypeOf(S.fetch)).@"fn".return_type.?).error_union.error_set;
}

/// Append up to `max` entries in `[start, end]` from `sources` to `out` in
/// key order. `S` carries `lb: key.KeyBuf` (≥ `start`; every key the source
/// holds in the window sorts at or above it) and `fetch(self, gpa, from,
/// end, max, out, cost) E!void` (up to `max` of its entries in `[from,
/// end]`, ascending, adding what it read to `cost`). Adds the sources it
/// consulted to `cost.sources`.
pub fn merge(comptime S: type, gpa: Allocator, sources: []S, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost) (FetchError(S) || Allocator.Error)!void {
    if (max == 0 or sources.len == 0) return;
    const streams = try gpa.alloc(Stream, sources.len);
    defer gpa.free(streams);
    const c0 = firstChunk(max, sources.len);
    for (streams) |*st| st.* = .{ .chunk = c0 };
    defer for (streams) |*st| st.deinit(gpa);
    // Consulted in ascending lower bound.
    const order = try gpa.alloc(u32, sources.len);
    defer gpa.free(order);
    for (order, 0..) |*o, i| o.* = @intCast(i);
    std.mem.sort(u32, order, sources, struct {
        fn lt(srcs: []S, a: u32, b: u32) bool {
            return key.lessThan(srcs[a].lb.slice(), srcs[b].lb.slice());
        }
    }.lt);
    var heap = std.PriorityQueue(u32, []Stream, headOrder).initContext(streams);
    defer heap.deinit(gpa);
    try heap.ensureTotalCapacity(gpa, sources.len);
    var consulted: usize = 0;
    var taken: usize = 0;
    while (taken < max) {
        // Every source whose bound the front has reached joins the heap
        // before its least head is taken.
        while (consulted < order.len) {
            const i = order[consulted];
            if (heap.peek()) |top| if (key.lessThan(streams[top].head(), sources[i].lb.slice())) break;
            consulted += 1;
            cost.sources += 1;
            try fill(S, gpa, &sources[i], &streams[i], sources[i].lb.slice(), end, max - taken, cost);
            if (streams[i].pending()) try heap.push(gpa, i);
        }
        const i = heap.pop() orelse break;
        const st = &streams[i];
        try out.ensureUnusedCapacity(gpa, 1);
        out.appendAssumeCapacity(st.buf.items[st.pos]);
        st.pos += 1;
        taken += 1;
        if (!st.pending() and !st.exhausted and taken < max) {
            st.chunk *|= 2;
            try fill(S, gpa, &sources[i], st, st.next_from.slice(), end, max - taken, cost);
        }
        if (st.pending()) try heap.push(gpa, i);
    }
}

/// The source's next chunk into its (taken) buffer.
fn fill(comptime S: type, gpa: Allocator, src: *S, st: *Stream, from: []const u8, end: []const u8, room: usize, cost: *tree.WalkCost) (FetchError(S) || Allocator.Error)!void {
    const want = @min(st.chunk, room);
    st.buf.clearRetainingCapacity();
    st.pos = 0;
    try src.fetch(gpa, from, end, want, &st.buf, cost);
    st.exhausted = st.buf.items.len < want;
    if (st.buf.items.len != 0) st.next_from = key.keySuccessor(st.buf.items[st.buf.items.len - 1].key);
}

// ---------------------------------------------------------------------------
// Tests: a sorted array per source, its lower bound by the routing law.
// ---------------------------------------------------------------------------

const testing = std.testing;

const ArraySource = struct {
    keys: []const []const u8,
    lb: key.KeyBuf = .{},
    fetches: usize = 0,

    pub fn fetch(self: *ArraySource, gpa: Allocator, from: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), cost: *tree.WalkCost) Allocator.Error!void {
        self.fetches += 1;
        cost.nodes += 1;
        var i = std.sort.lowerBound([]const u8, self.keys, from, struct {
            fn cmp(f: []const u8, k: []const u8) std.math.Order {
                return key.order(f, k);
            }
        }.cmp);
        var n: usize = 0;
        while (i < self.keys.len and n < max and !key.lessThan(end, self.keys[i])) : (i += 1) {
            const k = try gpa.dupe(u8, self.keys[i]);
            errdefer gpa.free(k);
            const v = try gpa.dupe(u8, "v");
            try out.append(gpa, .{ .key = k, .value = v });
            n += 1;
        }
        cost.entries += n;
    }
};

/// A set of `T` forest slots, each tree holding its slot's inode and dentry
/// keys and refs keys of its owners (the refs interleave across trees).
const Fixture = struct {
    arena: std.heap.ArenaAllocator,
    per: [][]const []const u8,
    all: []const []const u8,

    fn init(gpa: Allocator, trees: usize, per_tree: usize, seed: u64) !Fixture {
        var f: Fixture = .{ .arena = .init(gpa), .per = undefined, .all = undefined };
        errdefer f.arena.deinit();
        const a = f.arena.allocator();
        var prng = std.Random.DefaultPrng.init(seed);
        const r = prng.random();
        f.per = try a.alloc([]const []const u8, trees);
        var all: std.ArrayList([]const u8) = .empty;
        for (f.per, 0..) |*keys, slot| {
            var list: std.ArrayList([]const u8) = .empty;
            const base = @as(u64, slot) << key.GUEST_NS_SHIFT;
            for (0..per_tree) |j| {
                const ino = base + 2 + j * (1 + r.uintLessThan(u64, 3));
                const k = switch (r.uintLessThan(u8, 3)) {
                    0 => try a.dupe(u8, &key.inodeKey(ino)),
                    1 => try a.dupe(u8, &key.dentryKey(ino, r.uintLessThan(u64, key.HASH54_MAX), 0)),
                    else => try a.dupe(u8, &key.blockRefKey(7, r.uintLessThan(u64, 1 << 20), ino, 0)),
                };
                try list.append(a, k);
            }
            std.mem.sort([]const u8, list.items, {}, struct {
                fn lt(_: void, x: []const u8, y: []const u8) bool {
                    return key.lessThan(x, y);
                }
            }.lt);
            // Drop the PRNG's duplicates (a key lives once per tree).
            var n: usize = 0;
            for (list.items) |k| if (n == 0 or !std.mem.eql(u8, list.items[n - 1], k)) {
                list.items[n] = k;
                n += 1;
            };
            keys.* = list.items[0..n];
            try all.appendSlice(a, keys.*);
        }
        std.mem.sort([]const u8, all.items, {}, struct {
            fn lt(_: void, x: []const u8, y: []const u8) bool {
                return key.lessThan(x, y);
            }
        }.lt);
        f.all = all.items;
        return f;
    }

    fn deinit(self: *Fixture) void {
        self.arena.deinit();
    }

    /// One call's sources over `[start, end]`, each bounded by the law (or
    /// every bound the start: the law-free walk).
    fn sources(self: *const Fixture, buf: []ArraySource, start: []const u8, law: bool) []ArraySource {
        for (self.per, 0..) |keys, slot| buf[slot] = .{ .keys = keys, .lb = if (law) key.slotTreeLowerBound(@intCast(slot), start) else key.KeyBuf.fromSlice(start) };
        return buf[0..self.per.len];
    }
};

const Call = struct { consumed: usize, cost: tree.WalkCost, fetches: usize };

fn pageOnce(gpa: Allocator, f: *const Fixture, buf: []ArraySource, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue), law: bool) !Call {
    const srcs = f.sources(buf, start, law);
    var cost: tree.WalkCost = .{};
    const before = out.items.len;
    try merge(ArraySource, gpa, srcs, end, max, out, &cost);
    var fetches: usize = 0;
    for (srcs) |s| fetches += s.fetches;
    return .{ .consumed = out.items.len - before, .cost = cost, .fetches = fetches };
}

test "the merged walk pages through the whole space in key order — every page the first `max` of the union at/after its start — within the read and fetch bounds per call, over 1, 3, 64 and 512 per page, bounded by the law and law-free alike" {
    const gpa = testing.allocator;
    var f = try Fixture.init(gpa, 16, 300, 0x5EED);
    defer f.deinit();
    var buf: [16]ArraySource = undefined;
    const end: [40]u8 = @splat(0xFF);
    for ([_]bool{ true, false }) |law| for ([_]usize{ 1, 3, 64, 512 }) |max| {
        var got: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &got);
        var start = key.KeyBuf.fromSlice(&key.inodeKey(0));
        while (true) {
            const before = got.items.len;
            const c = try pageOnce(gpa, &f, &buf, start.slice(), &end, max, &got, law);
            try testing.expect(c.cost.entries <= readBound(c.consumed, max, c.cost.sources, f.per.len));
            try testing.expect(c.fetches <= fetchBound(c.cost.sources, f.per.len));
            if (got.items.len == before) break;
            start = key.keySuccessor(got.items[got.items.len - 1].key);
        }
        try testing.expectEqual(f.all.len, got.items.len);
        for (got.items, f.all) |kv, want| try testing.expectEqualSlices(u8, want, kv.key);
    };
}

test "an ino-major page consults the trees its front reaches, not all T: 512 entries over 64 slot trees of ~100 ino-major keys each read from at most ceil(512 / 100) + 1 trees; a refs window consults every tree" {
    const gpa = testing.allocator;
    var f: Fixture = .{ .arena = .init(gpa), .per = undefined, .all = &.{} };
    defer f.deinit();
    const a = f.arena.allocator();
    const trees = 64;
    f.per = try a.alloc([]const []const u8, trees);
    for (f.per, 0..) |*keys, slot| {
        const list = try a.alloc([]const u8, 101);
        const base = @as(u64, slot) << key.GUEST_NS_SHIFT;
        for (list[0..100], 0..) |*k, j| k.* = try a.dupe(u8, &key.inodeKey(base + 2 + j));
        // Each tree's one ref sorts after every ino-major key of every tree.
        list[100] = try a.dupe(u8, &key.blockRefKey(7, slot, base + 2, 0));
        keys.* = list;
    }
    var buf: [trees]ArraySource = undefined;
    const end: [40]u8 = @splat(0xFF);
    var out: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &out);
    const c = try pageOnce(gpa, &f, &buf, &key.inodeKey(0), &end, 512, &out, true);
    try testing.expectEqual(@as(usize, 512), c.consumed);
    try testing.expect(c.cost.sources <= 512 / 100 + 2);
    try testing.expect(c.cost.entries <= readBound(512, 512, c.cost.sources, trees));
    // The law it replaces read min(512, the tree's share) from all 64.
    try testing.expect(c.cost.entries < 64 * 100 / 4);
    // A refs window: every tree holds one, every tree is consulted.
    var refs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &refs);
    const r = try pageOnce(gpa, &f, &buf, &.{key.TREE_BLOCK_REFS}, &end, 512, &refs, true);
    try testing.expectEqual(@as(usize, trees), r.consumed);
    try testing.expectEqual(@as(u64, trees), r.cost.sources);
    try testing.expectEqual(@as(u64, trees), r.cost.entries);
    for (refs.items[1..], 1..) |kv, i| try testing.expect(key.lessThan(refs.items[i - 1].key, kv.key));
}

test "a key a tree holds below its bound (outside its slot's window — no lookup reaches it) is not visited by the law's walk, which stays in key order; the law-free walk visits it in order" {
    const gpa = testing.allocator;
    var f: Fixture = .{ .arena = .init(gpa), .per = undefined, .all = &.{} };
    defer f.deinit();
    const a = f.arena.allocator();
    f.per = try a.alloc([]const []const u8, 3);
    const lo1 = @as(u64, 1) << key.GUEST_NS_SHIFT;
    const lo2 = @as(u64, 2) << key.GUEST_NS_SHIFT;
    f.per[0] = &.{};
    f.per[1] = &.{ try a.dupe(u8, &key.inodeKey(lo1 + 2)), try a.dupe(u8, &key.inodeKey(lo1 + 9)) };
    // Tree 2 holds slot 1's key lo1 + 5, below its bound (slot 2's first key).
    f.per[2] = &.{ try a.dupe(u8, &key.inodeKey(lo1 + 5)), try a.dupe(u8, &key.inodeKey(lo2 + 2)) };
    var buf: [3]ArraySource = undefined;
    const end: [40]u8 = @splat(0xFF);
    var out: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &out);
    _ = try pageOnce(gpa, &f, &buf, &key.inodeKey(0), &end, 10, &out, true);
    try testing.expectEqual(@as(usize, 3), out.items.len);
    for (out.items, [_]u64{ lo1 + 2, lo1 + 9, lo2 + 2 }) |kv, ino| try testing.expectEqualSlices(u8, &key.inodeKey(ino), kv.key);
    var raw: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &raw);
    const c = try pageOnce(gpa, &f, &buf, &key.inodeKey(0), &end, 10, &raw, false);
    try testing.expectEqual(@as(u64, 3), c.cost.sources);
    try testing.expectEqual(@as(usize, 4), raw.items.len);
    for (raw.items, [_]u64{ lo1 + 2, lo1 + 5, lo1 + 9, lo2 + 2 }) |kv, ino| try testing.expectEqualSlices(u8, &key.inodeKey(ino), kv.key);
}

test "the bounds restate their derivation (drift is red): a short call reads what it took; a full one 2e + p × ceil(max / T); fetches p × (1 + ⌈log2(T + 1)⌉)" {
    try testing.expectEqual(@as(usize, 8), firstChunk(512, 64));
    try testing.expectEqual(@as(usize, 1), firstChunk(1, 64));
    try testing.expectEqual(@as(usize, 512), firstChunk(512, 0));
    try testing.expectEqual(@as(usize, 300), readBound(300, 512, 64, 64));
    try testing.expectEqual(@as(usize, 2 * 512 + 3 * 8), readBound(512, 512, 3, 64));
    try testing.expectEqual(@as(usize, 3 * (1 + 7)), fetchBound(3, 64));
    try testing.expectEqual(@as(usize, 1 + 1), fetchBound(1, 1));
}
