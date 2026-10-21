//! THE ONE purge helper (charter §6.3 "ONE purge helper covers all five
//! tiers (a forgotten tier is a stale-serve class)"; `docs/porting/
//! data_read_path.md` §2.6, §13 Q2). PORTABLE.
//!
//! The ONLY legal way to drop a block key from the read caches: displaced-
//! key frees, fill undos, incarnation purges, the W1 patch's post-DMA purge,
//! the overlay's durable displacement and every clone/unlink/write-path
//! invalidation route here. The reference pinned the census with a grep
//! guard over a hand-written list; here it is STRUCTURAL: `Tiers` names
//! every block-key store, `comptime` asserts each field's type implements
//! the purge arm, and `tier_names` (the charter's five) must equal the field
//! set exactly — adding a tier without adding its arm does not compile.
//!
//! `purgeBlockRange` is the in-place write's face (W1, the in-place
//! drains): the whole-block tiers drop the block (they hold its whole
//! image) and the read cache drops exactly the grains the DMA rewrote, so
//! a 4 KiB write under a random reader costs the reader one grain, not
//! the block's thousand.
//!
//! Invalidations deliberately do NOT report to the admission governor
//! (`lru.rs:76–77`): a purge is a correctness event, not a waste signal.
//! `PurgeSeam` is the write side's face onto it (W1 purges through it).
//!
//! The CENSUS (`purgeCensus`) is the token recall's fallback when a
//! recalled object's keys cannot be enumerated (spec
//! `sym_tokens_intents_striping.md` §1.7 step 2): every tier's census face
//! (`nextKey`, structural like the purge arm) names the keys a selector
//! picks and each goes through `purgeBlockKey` — still the one way a key
//! leaves the tiers, never a per-tier drop.

const std = @import("std");
const counters = @import("../core/counters.zig");
const rsc = @import("read_size_class.zig");
const hot_tier = @import("hot_tier.zig");
const read_lru = @import("read_lru.zig");
const read_hold = @import("read_hold.zig");
const read_cache = @import("read_cache.zig");
const lease_epoch = @import("lease_epoch.zig");
const nvme_read_cache = @import("nvme_read_cache.zig");

pub const BlockKey = rsc.BlockKey;

/// The charter's five tiers, by field name. The census below fails to
/// compile if `Tiers` gains or loses a field relative to this list.
pub const tier_names = [_][]const u8{ "hot", "lru", "hold", "grains", "nvme" };

/// Every block-key-addressed byte store.
pub const Tiers = struct {
    hot: *hot_tier.HotTier,
    lru: *read_lru.ReadLru,
    hold: *read_hold.Hold,
    grains: *read_cache.ReadCache,
    nvme: *nvme_read_cache.Cache,
};

const tiers_info = @typeInfo(Tiers).@"struct";

comptime {
    if (tiers_info.field_names.len != tier_names.len) @compileError("purge.Tiers must name exactly the charter's five tiers");
    for (tiers_info.field_names, tiers_info.field_types, 0..) |name, FT, i| {
        if (!std.mem.eql(u8, name, tier_names[i])) @compileError("purge.Tiers field order/name diverged from tier_names: " ++ name);
        const T = @typeInfo(FT).pointer.child;
        if (!@hasDecl(T, "purge")) @compileError(@typeName(T) ++ " is a block-key store without a purge arm");
        const P = @TypeOf(@field(T, "purge"));
        if (P != fn (*T, BlockKey) bool) @compileError(@typeName(T) ++ ".purge must be `fn (*Self, BlockKey) bool`");
        if (!@hasDecl(T, "nextKey")) @compileError(@typeName(T) ++ " is a block-key store without a census face");
        if (@TypeOf(@field(T, "nextKey")) != fn (*T, *u64, rsc.KeySelect) ?BlockKey) @compileError(@typeName(T) ++ ".nextKey must be `fn (*Self, *u64, KeySelect) ?BlockKey`");
    }
}

pub const stats = struct {
    /// `read_tier_purges`: calls of the ONE helper.
    pub var read_tier_purges: counters.ShardedCounter = .{};
    /// `read_tier_purge_hits`: tier entries actually dropped (≤ 5 per call).
    pub var read_tier_purge_hits: counters.ShardedCounter = .{};
    /// `read_tier_range_purges`: calls of the ranged face (in-place writes).
    pub var read_tier_range_purges: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"read_tier_purges\":{d},\"read_tier_purge_hits\":{d},\"read_tier_range_purges\":{d}}}", .{ stats.read_tier_purges.load(), stats.read_tier_purge_hits.load(), stats.read_tier_range_purges.load() });
}

/// Drop `key` from EVERY tier; returns how many held it. Absent keys cost
/// one probe per tier and no allocation (the W1 patch purges at 400 k+/s).
pub fn purgeBlockKey(t: Tiers, key: BlockKey) u32 {
    stats.read_tier_purges.inc();
    var hits: u32 = 0;
    inline for (tiers_info.field_names) |name| {
        if (@field(t, name).purge(key)) hits += 1;
    }
    if (hits > 0) stats.read_tier_purge_hits.add(hits);
    return hits;
}

/// The in-place write's purge: `[rel_s, rel_s + len)` of `key`'s block was
/// rewritten on the device. Every whole-block tier drops the block; the
/// read cache drops the window's grains alone. Returns the tiers that
/// held something.
pub fn purgeBlockRange(t: Tiers, key: BlockKey, rel_s: u64, len: u64) u32 {
    stats.read_tier_purges.inc();
    stats.read_tier_range_purges.inc();
    var hits: u32 = 0;
    inline for (tiers_info.field_names) |name| {
        if (comptime std.mem.eql(u8, name, "grains")) {
            if (@field(t, name).purgeRange(key, rel_s, len) > 0) hits += 1;
        } else if (@field(t, name).purge(key)) hits += 1;
    }
    if (hits > 0) stats.read_tier_purge_hits.add(hits);
    return hits;
}

/// The census: every key some tier holds that `select` picks, dropped
/// from EVERY tier through `purgeBlockKey`; returns the keys dropped. One
/// walk per tier behind its cursor (a key two tiers hold is purged once,
/// the second tier's walk no longer finds it); nothing allocated, so the
/// recall's fallback cannot fail half-done. A key deposited behind a
/// cursor that already passed is the depositor's to settle (its binding
/// recheck), exactly as against a scoped purge.
pub fn purgeCensus(t: Tiers, select: rsc.KeySelect) u32 {
    var n: u32 = 0;
    inline for (tiers_info.field_names) |name| {
        var cursor: u64 = 0;
        while (@field(t, name).nextKey(&cursor, select)) |k| {
            _ = purgeBlockKey(t, k);
            n += 1;
        }
    }
    return n;
}

/// The write side's face: `purge(ctx, key)` and the ranged form.
pub const PurgeSeam = struct {
    ctx: *anyopaque,
    purge: *const fn (ctx: *anyopaque, key: BlockKey) void,
    purge_range: *const fn (ctx: *anyopaque, key: BlockKey, rel_s: u64, len: u64) void,

    pub fn call(self: PurgeSeam, key: BlockKey) void {
        self.purge(self.ctx, key);
    }

    pub fn callRange(self: PurgeSeam, key: BlockKey, rel_s: u64, len: u64) void {
        self.purge_range(self.ctx, key, rel_s, len);
    }
};

fn seamPurge(ctx: *anyopaque, key: BlockKey) void {
    const t: *Tiers = @ptrCast(@alignCast(ctx));
    _ = purgeBlockKey(t.*, key);
}

fn seamPurgeRange(ctx: *anyopaque, key: BlockKey, rel_s: u64, len: u64) void {
    const t: *Tiers = @ptrCast(@alignCast(ctx));
    _ = purgeBlockRange(t.*, key, rel_s, len);
}

/// The seam over a `Tiers` the caller keeps alive.
pub fn seam(t: *Tiers) PurgeSeam {
    return .{ .ctx = t, .purge = &seamPurge, .purge_range = &seamPurgeRange };
}

// ---------------------------------------------------------------------------

const testing = std.testing;
const fill_pool = @import("read_fill_pool.zig");

test "the ONE purge drops a key from every tier that holds it and counts the hits; the seam routes to it" {
    const bs: usize = 4096;
    const pool = try fill_pool.Pool.initRam(testing.allocator, bs, 8);
    defer pool.deinit();
    const hot = try hot_tier.HotTier.init(testing.allocator, pool, bs, 4 * bs, 1);
    defer hot.deinit();
    const lru = try read_lru.ReadLru.init(testing.allocator, 2 * rsc.SMALL_FILL_MAX, 1);
    defer lru.deinit();
    const hold = try read_hold.Hold.init(testing.allocator, pool, 4);
    defer hold.deinit();
    hold.setBudget(4 * bs);
    var nvme = try nvme_read_cache.Cache.initRam(testing.allocator, 1, 8 * nvme_read_cache.GRAIN);
    defer nvme.deinit();
    const grains = try read_cache.ReadCache.init(testing.allocator, bs, read_cache.DEFAULT_PCT, 8 * read_cache.GRAIN, 8 * read_cache.GRAIN, 1);
    defer grains.deinit();
    var tiers: Tiers = .{ .hot = hot, .lru = lru, .hold = hold, .grains = grains, .nvme = &nvme };

    const k = BlockKey.init(1, 0);
    const s = pool.acquire().?;
    @memset(pool.bytes(s), 1);
    try testing.expectEqual(hot_tier.PutOutcome.inserted, hot.putClassValidated(k, s, @intCast(bs), .protected, .always, false));
    try testing.expectEqual(read_hold.InsertOutcome.inserted, hold.insert(k, s, @intCast(bs), true, .always));
    _ = pool.unpin(s);
    try testing.expectEqual(hot_tier.PutOutcome.inserted, lru.putValidated(k, pool.bytes(s)[0..bs], .always, false));
    try testing.expectEqual(nvme_read_cache.PutOutcome.published, nvme.putValidated(k, pool.bytes(s)[0..bs], 1, .always, false));
    try testing.expectEqual(read_cache.ReadCache.PutOutcome.inserted, grains.putValidated(k, 0, pool.bytes(s)[0..bs], 7, lease_epoch.load(7), .always, false));
    try testing.expect(hot.contains(k) and lru.contains(k) and hold.contains(k) and nvme.has(k) and grains.contains(k, 0, bs));

    const calls_before = stats.read_tier_purges.load();
    const hits_before = stats.read_tier_purge_hits.load();
    try testing.expectEqual(@as(u32, 5), purgeBlockKey(tiers, k));
    try testing.expect(!hot.contains(k) and !lru.contains(k) and !hold.contains(k) and !nvme.has(k) and !grains.contains(k, 0, bs));
    try testing.expectEqual(@as(u32, 0), pool.refCount(s));
    try testing.expectEqual(calls_before + 1, stats.read_tier_purges.load());
    try testing.expectEqual(hits_before + 5, stats.read_tier_purge_hits.load());
    // An absent key: one probe per tier, zero hits.
    try testing.expectEqual(@as(u32, 0), purgeBlockKey(tiers, k));
    try testing.expectEqual(hits_before + 5, stats.read_tier_purge_hits.load());
    // The seam.
    const s2 = pool.acquire().?;
    try testing.expectEqual(hot_tier.PutOutcome.inserted, hot.putClassValidated(k, s2, @intCast(bs), .protected, .always, false));
    _ = pool.unpin(s2);
    seam(&tiers).call(k);
    try testing.expect(!hot.contains(k));
}

test "the ranged purge drops the whole block from the whole-block tiers and exactly the window's grains from the read cache" {
    const bs: usize = 16 * @as(usize, @intCast(read_cache.GRAIN));
    const pool = try fill_pool.Pool.initRam(testing.allocator, bs, 8);
    defer pool.deinit();
    const hot = try hot_tier.HotTier.init(testing.allocator, pool, bs, 4 * bs, 1);
    defer hot.deinit();
    const lru = try read_lru.ReadLru.init(testing.allocator, 2 * rsc.SMALL_FILL_MAX, 1);
    defer lru.deinit();
    const hold = try read_hold.Hold.init(testing.allocator, pool, 4);
    defer hold.deinit();
    hold.setBudget(4 * bs);
    var nvme = try nvme_read_cache.Cache.initRam(testing.allocator, 1, 8 * nvme_read_cache.GRAIN);
    defer nvme.deinit();
    const grains = try read_cache.ReadCache.init(testing.allocator, bs, read_cache.DEFAULT_PCT, 64 * read_cache.GRAIN, 64 * read_cache.GRAIN, 1);
    defer grains.deinit();
    var tiers: Tiers = .{ .hot = hot, .lru = lru, .hold = hold, .grains = grains, .nvme = &nvme };
    const k = BlockKey.init(4, 2 * bs);
    const s = pool.acquire().?;
    @memset(pool.bytes(s), 7);
    try testing.expectEqual(hot_tier.PutOutcome.inserted, hot.putClassValidated(k, s, @intCast(bs), .protected, .always, false));
    _ = pool.unpin(s);
    const g = read_cache.GRAIN;
    try testing.expectEqual(read_cache.ReadCache.PutOutcome.inserted, grains.putValidated(k, 0, pool.bytes(s)[0 .. 4 * g], 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(read_cache.ReadCache.PutOutcome.inserted, grains.putValidated(k, 8 * g, pool.bytes(s)[0 .. 2 * g], 7, lease_epoch.load(7), .always, false));
    const range_calls = stats.read_tier_range_purges.load();
    // W1 rewrote grain 1: the hot tier loses the block, the cache grain 1 alone.
    try testing.expectEqual(@as(u32, 2), purgeBlockRange(tiers, k, g, g));
    try testing.expect(!hot.contains(k));
    try testing.expect(grains.contains(k, 0, g) and !grains.contains(k, g, 2 * g) and grains.contains(k, 2 * g, 4 * g) and grains.contains(k, 8 * g, 10 * g));
    try testing.expectEqual(range_calls + 1, stats.read_tier_range_purges.load());
    // The seam's ranged face: an unaligned sub-grain write drops the one
    // grain that holds it.
    seam(&tiers).callRange(k, 9 * g + 17, 100);
    try testing.expect(grains.contains(k, 8 * g, 9 * g) and !grains.contains(k, 9 * g, 10 * g));
    try testing.expectEqual(@as(u32, 0), purgeBlockRange(tiers, k, 12 * g, g));
}

test "the census is structural: every tier type's purge arm and census face have the one signature" {
    inline for (tiers_info.field_types) |FT| {
        const T = @typeInfo(FT).pointer.child;
        try testing.expect(@TypeOf(@field(T, "purge")) == fn (*T, BlockKey) bool);
        try testing.expect(@TypeOf(@field(T, "nextKey")) == fn (*T, *u64, rsc.KeySelect) ?BlockKey);
    }
    try testing.expectEqual(@as(usize, 5), tier_names.len);
}

test "the census purge drops exactly the keys its selector picks, from every tier, through the ONE purge; the rest keep their entries" {
    const bs: usize = 4096;
    const pool = try fill_pool.Pool.initRam(testing.allocator, bs, 16);
    defer pool.deinit();
    const hot = try hot_tier.HotTier.init(testing.allocator, pool, bs, 8 * bs, 2);
    defer hot.deinit();
    const lru = try read_lru.ReadLru.init(testing.allocator, 4 * rsc.SMALL_FILL_MAX, 2);
    defer lru.deinit();
    const hold = try read_hold.Hold.init(testing.allocator, pool, 8);
    defer hold.deinit();
    hold.setBudget(8 * bs);
    var nvme = try nvme_read_cache.Cache.initRam(testing.allocator, 2, 16 * nvme_read_cache.GRAIN);
    defer nvme.deinit();
    const grains = try read_cache.ReadCache.init(testing.allocator, bs, read_cache.DEFAULT_PCT, 16 * read_cache.GRAIN, 16 * read_cache.GRAIN, 1);
    defer grains.deinit();
    const tiers: Tiers = .{ .hot = hot, .lru = lru, .hold = hold, .grains = grains, .nvme = &nvme };
    // Volume 1 is "foreign" (picked), volume 2 is not; each key in a
    // different mix of tiers.
    const Pick = struct {
        fn call(_: *anyopaque, k: BlockKey) bool {
            return k.vol_tag == 1;
        }
    };
    var dummy: u8 = 0;
    const select: rsc.KeySelect = .{ .ctx = &dummy, .call = &Pick.call };
    const keys = [_]BlockKey{ .init(1, 0), .init(1, bs), .init(2, 0), .init(2, bs) };
    for (keys, 0..) |k, i| {
        const s = pool.acquire().?;
        @memset(pool.bytes(s), @intCast(i));
        if (i != 1) try testing.expectEqual(hot_tier.PutOutcome.inserted, hot.putClassValidated(k, s, @intCast(bs), .protected, .always, false));
        if (i != 0) try testing.expectEqual(read_hold.InsertOutcome.inserted, hold.insert(k, s, @intCast(bs), true, .always));
        _ = pool.unpin(s);
        try testing.expectEqual(hot_tier.PutOutcome.inserted, lru.putValidated(k, pool.bytes(s)[0..bs], .always, false));
        try testing.expectEqual(nvme_read_cache.PutOutcome.published, nvme.putValidated(k, pool.bytes(s)[0..bs], 1, .always, false));
        if (i != 2) try testing.expectEqual(read_cache.ReadCache.PutOutcome.inserted, grains.putValidated(k, 0, pool.bytes(s)[0..bs], 7, lease_epoch.load(7), .always, false));
    }
    const calls0 = stats.read_tier_purges.load();
    try testing.expectEqual(@as(u32, 2), purgeCensus(tiers, select));
    try testing.expectEqual(calls0 + 2, stats.read_tier_purges.load());
    for (keys[0..2]) |k| try testing.expect(!hot.contains(k) and !lru.contains(k) and !hold.contains(k) and !nvme.has(k) and !grains.contains(k, 0, bs));
    try testing.expect(hot.contains(keys[2]) and lru.contains(keys[2]) and nvme.has(keys[2]));
    try testing.expect(hot.contains(keys[3]) and hold.contains(keys[3]) and lru.contains(keys[3]) and nvme.has(keys[3]) and grains.contains(keys[3], 0, bs));
    // A second census finds nothing left to drop.
    try testing.expectEqual(@as(u32, 0), purgeCensus(tiers, select));
}
