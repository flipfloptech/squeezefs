//! The ≤ 256 KiB read LRU (charter §6.3 "≤ 256 KiB RAM LRU";
//! `docs/porting/data_read_path.md` §2.2, §2.5). PORTABLE.
//!
//! The SAME clock core as the hot tier (`hot_tier.ClockCache`) over a slab
//! of `SMALL_FILL_MAX` slots: every insert is `protected` (small fills are
//! request-sized and LRU-cacheable by construction), every victim is handed
//! to the owner's sink for dehydration (`< 64 KiB` inline, else deferred —
//! `read_size_class.INLINE_PUBLISH_MAX`). Whole-block fills never enter it:
//! one stream must not flush the warm small population (`SMALL_FILL_MAX`).
//!
//! A put COPIES the fill into a slab slot: a small fill is a cold path (one
//! device fetch per entry) and the slab keeps the serve path pool-backed
//! and allocation-free.

const std = @import("std");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const budget = @import("../mem/budget.zig");
const rsc = @import("read_size_class.zig");
const fill_pool = @import("read_fill_pool.zig");
const hot_tier = @import("hot_tier.zig");

pub const BlockKey = rsc.BlockKey;
pub const Validate = hot_tier.Validate;
pub const PutOutcome = hot_tier.PutOutcome;
pub const Victim = hot_tier.Victim;
pub const EvictSink = hot_tier.EvictSink;

/// `read_lru`: floor 32 MiB (128 small entries — the warm small population
/// of one metadata-heavy workload), weight 2.
pub const COMPONENT_FLOOR: u64 = 32 * derive.MiB;
pub const COMPONENT_WEIGHT: u64 = 2;
/// A shard holds ≥ 2 small entries so two colliding keys coexist (the
/// hot tier's min-shard lesson at this tier's grain).
pub const MIN_SHARD_BYTES: u64 = 2 * rsc.SMALL_FILL_MAX;

pub const stats = struct {
    pub var read_lru_hits: counters.ShardedCounter = .{};
    pub var read_lru_puts: counters.ShardedCounter = .{};
    pub var read_lru_evictions: counters.ShardedCounter = .{};
    pub var read_lru_current_bytes: counters.Gauge = .{};
    /// A put that found the slab full and evicted one entry by the clock
    /// first (entries smaller than a slot fill the slab before the budget).
    pub var read_lru_slab_exhausted: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"read_lru_hits\":{d},\"read_lru_puts\":{d},\"read_lru_evictions\":{d},\"read_lru_current_bytes\":{d},\"read_lru_slab_exhausted\":{d}}}", .{
        stats.read_lru_hits.load(),
        stats.read_lru_puts.load(),
        stats.read_lru_evictions.load(),
        stats.read_lru_current_bytes.load(),
        stats.read_lru_slab_exhausted.load(),
    });
}

/// Slab slots for a budget: one per `SMALL_FILL_MAX` plus one per shard
/// for the put that triggers an eviction; floor 2.
pub fn slabSlots(budget_bytes: u64, shards: u32) u32 {
    return @intCast(@max(budget_bytes / rsc.SMALL_FILL_MAX, 1) + shards);
}

pub const View = struct {
    slot: u32,
    len: u32,
};

pub const ReadLru = struct {
    gpa: Allocator,
    slab: *fill_pool.Pool,
    cache: hot_tier.ClockCache(ReadLru),
    sink: EvictSink = .none,
    budget_bytes: u64,

    pub fn init(gpa: Allocator, budget_bytes: u64, cores: u32) (Allocator.Error || fill_pool.InitError)!*ReadLru {
        const self = try gpa.create(ReadLru);
        errdefer gpa.destroy(self);
        const shards = hot_tier.shardsForBudget(cores, budget_bytes, MIN_SHARD_BYTES);
        const slots = slabSlots(budget_bytes, shards);
        const slab = try fill_pool.Pool.initRam(gpa, rsc.SMALL_FILL_MAX, slots);
        errdefer slab.deinit();
        self.* = .{ .gpa = gpa, .slab = slab, .cache = undefined, .budget_bytes = budget_bytes };
        self.cache = try hot_tier.ClockCache(ReadLru).init(gpa, self, budget_bytes, shards, @as(usize, slots) / shards + 1);
        return self;
    }

    pub fn deinit(self: *ReadLru) void {
        self.cache.deinit();
        self.slab.deinit();
        self.gpa.destroy(self);
    }

    // ---- the Store face ----

    pub fn pin(self: *ReadLru, slot: u32) void {
        self.slab.pin(slot);
    }

    pub fn release(self: *ReadLru, slot: u32) void {
        _ = self.slab.unpin(slot);
    }

    pub fn onEvict(self: *ReadLru, v: Victim) void {
        stats.read_lru_evictions.inc();
        if (self.sink.on_evict) |f| f(self.sink.ctx, v);
        _ = self.slab.unpin(v.slot);
        stats.read_lru_current_bytes.set(self.cache.currentBytes());
    }

    // ---- the tier face ----

    /// Copy `data` (≤ `SMALL_FILL_MAX`) into the slab and insert it
    /// `protected` under the validation. `freeze` (Yellow+) = net-zero growth.
    pub fn putValidated(self: *ReadLru, key: BlockKey, data: []const u8, validate: Validate, freeze: bool) PutOutcome {
        assert(data.len <= rsc.SMALL_FILL_MAX);
        if (self.budget_bytes == 0) return .refused;
        // The slab bounds ENTRIES as the budget bounds bytes: an exhausted
        // slab evicts one entry by the clock and retries once.
        const slot = self.slab.acquire() orelse blk: {
            stats.read_lru_slab_exhausted.inc();
            if (!self.cache.evictAny(key)) return .refused;
            break :blk self.slab.acquire() orelse return .refused;
        };
        @memcpy(self.slab.bytes(slot)[0..data.len], data);
        const freeze_at = if (freeze) self.cache.currentBytes() else 0;
        var removed: ?Victim = null;
        const out = self.cache.put(key, slot, @intCast(data.len), .protected, validate, &removed);
        if (removed) |r| _ = self.slab.unpin(r.slot);
        switch (out) {
            .inserted, .replaced => {
                stats.read_lru_puts.inc();
                if (freeze) self.cache.shedTo(freeze_at);
            },
            .refused, .too_large => _ = self.slab.unpin(slot),
        }
        stats.read_lru_current_bytes.set(self.cache.currentBytes());
        return out;
    }

    /// A hit pins the slot; the caller copies out of `bytes(view)` and `unpin`s.
    pub fn get(self: *ReadLru, key: BlockKey) ?View {
        const h = self.cache.get(key, true, 0) orelse return null;
        stats.read_lru_hits.inc();
        return .{ .slot = h.slot, .len = h.len };
    }

    pub fn bytes(self: *ReadLru, v: View) []const u8 {
        return self.slab.bytes(v.slot)[0..v.len];
    }

    pub fn unpin(self: *ReadLru, v: View) void {
        _ = self.slab.unpin(v.slot);
    }

    pub fn touchNoPromote(self: *ReadLru, key: BlockKey) bool {
        return self.cache.touch(key);
    }

    pub fn contains(self: *ReadLru, key: BlockKey) bool {
        return self.cache.contains(key);
    }

    /// The census face (`purge.zig`).
    pub fn nextKey(self: *ReadLru, cursor: *u64, select: rsc.KeySelect) ?BlockKey {
        return self.cache.nextKey(cursor, select);
    }

    /// The purge arm.
    pub fn purge(self: *ReadLru, key: BlockKey) bool {
        const v = self.cache.remove(key) orelse return false;
        _ = self.slab.unpin(v.slot);
        stats.read_lru_current_bytes.set(self.cache.currentBytes());
        return true;
    }

    pub fn shedTo(self: *ReadLru, target: u64) void {
        self.cache.shedTo(target);
    }

    pub fn currentBytes(self: *const ReadLru) u64 {
        return self.cache.currentBytes();
    }

    /// The RAM this tier holds: its slab's resident bytes (the entries'
    /// slots plus the freed slots the kernel still backs) — not the logical
    /// entry bytes (`read_lru_current_bytes`), which undercount a slab
    /// whose evicted slots stay resident.
    fn gaugeFn(ctx: *anyopaque) u64 {
        const self: *ReadLru = @ptrCast(@alignCast(ctx));
        return self.slab.residentBytes();
    }

    /// Evict down to the target, then return the idle slots the eviction
    /// freed (Red: everything idle) so the gauge moves in the same call.
    fn shedFn(ctx: *anyopaque, target: u64) void {
        const self: *ReadLru = @ptrCast(@alignCast(ctx));
        self.shedTo(target);
        self.slab.trimToward(budget.trim_red_min_idle_ticks, target -| self.slab.checkedOutBytes());
    }

    fn trimFn(ctx: *anyopaque, min_idle_ticks: u64) void {
        const self: *ReadLru = @ptrCast(@alignCast(ctx));
        self.slab.trim(min_idle_ticks);
    }

    pub fn component(self: *ReadLru) budget.Component {
        return budget.Component.init("read_lru", COMPONENT_FLOOR, COMPONENT_WEIGHT, .{ .func = .{ .ctx = self, .call = &gaugeFn } }, .{ .ctx = self, .call = &shedFn }).withTrim(.{ .ctx = self, .call = &trimFn }).kernelWalk();
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "tie: slab slots = budget / 256 KiB + shards (floor 1 + shards); min shard 2 entries" {
    try testing.expectEqual(@as(u32, 4 + 1), slabSlots(4 * rsc.SMALL_FILL_MAX, 1));
    try testing.expectEqual(@as(u32, 1 + 2), slabSlots(0, 2));
    try testing.expectEqual(@as(u64, 512 * derive.KiB), MIN_SHARD_BYTES);
}

test "the small LRU: protected inserts, pinned hits copy out, the clock evicts the oldest unreferenced, purge drops, a > 256 KiB value is the caller's bug" {
    const lru = try ReadLru.init(testing.allocator, 2 * rsc.SMALL_FILL_MAX, 1);
    defer lru.deinit();
    try testing.expectEqual(@as(u32, 1), lru.cache.shardCount());
    var sink: TestSink = .{};
    defer sink.victims.deinit(testing.allocator);
    lru.sink = .{ .ctx = &sink, .on_evict = &TestSink.on };
    const a = BlockKey.init(1, 0);
    const b = BlockKey.init(1, 4096);
    const c = BlockKey.init(1, 8192);
    var payload: [4096]u8 = @splat(0x11);
    try testing.expectEqual(PutOutcome.inserted, lru.putValidated(a, &payload, .always, false));
    payload = @splat(0x22);
    try testing.expectEqual(PutOutcome.inserted, lru.putValidated(b, &payload, .always, false));
    try testing.expectEqual(@as(u64, 8192), lru.currentBytes());
    const va = lru.get(a).?;
    try testing.expectEqual(@as(u8, 0x11), lru.bytes(va)[0]);
    try testing.expectEqual(@as(usize, 4096), lru.bytes(va).len);
    lru.unpin(va);
    try testing.expect(lru.cache.bitsOf(a).?.protected);
    // Bytes are far under budget; the clock only evicts over budget. Fill it.
    var big: [rsc.SMALL_FILL_MAX]u8 = undefined;
    @memset(&big, 0x33);
    try testing.expectEqual(PutOutcome.inserted, lru.putValidated(c, &big, .always, false));
    const d = BlockKey.init(1, 12288);
    try testing.expectEqual(PutOutcome.inserted, lru.putValidated(d, &big, .always, false));
    // Over budget by 8 KiB: the clock gives a (referenced by the get) and b
    // (referenced at insert) their second chances, then evicts the oldest
    // unreferenced — which after one lap is a again.
    try testing.expect(sink.victims.items.len >= 1);
    try testing.expectEqual(hot_tier.EvictClass.protected, sink.victims.items[0].class);
    try testing.expect(lru.currentBytes() <= 2 * rsc.SMALL_FILL_MAX);
    try testing.expect(lru.purge(d) or lru.purge(c));
    try testing.expect(!lru.purge(.init(9, 9)));
    const comp = lru.component();
    try testing.expectEqualStrings("read_lru", comp.name);
    try testing.expectEqual(COMPONENT_FLOOR, comp.floor);
    try testing.expectEqual(COMPONENT_WEIGHT, comp.weight);
    // The gauge is the slab's RAM: entries + evicted-but-resident slots.
    try testing.expectEqual(lru.slab.residentBytes(), comp.current());
    try testing.expect(comp.current() > lru.currentBytes());
    // A shed to 0 evicts everything AND returns the freed slots: the gauge reads 0.
    comp.shed.?.call(comp.shed.?.ctx, 0);
    try testing.expectEqual(@as(u64, 0), lru.currentBytes());
    try testing.expectEqual(@as(usize, lru.slab.capacity), lru.slab.freeSlots());
    try testing.expectEqual(@as(u64, 0), comp.current());
    try testing.expect(comp.trim != null);
}

const TestSink = struct {
    victims: std.ArrayList(Victim) = .empty,
    fn on(ctx: ?*anyopaque, v: Victim) void {
        const self: *TestSink = @ptrCast(@alignCast(ctx.?));
        self.victims.append(testing.allocator, v) catch unreachable;
    }
};
