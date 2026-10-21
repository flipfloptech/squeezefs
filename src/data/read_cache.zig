//! The READ CACHE — `Cache(Grain, DropSink)` over the ONE cache core
//! (`mem/cache.zig`; charter §6.3 "the read cache tier"; owner rulings
//! 2026-10-21: 25 % of system RAM by default, plain LRU, one cache model
//! used forward for reading). PORTABLE.
//!
//! What it holds: the bytes a RANGED device read DMA'd — the 4 KiB-outward
//! window of a random sub-block request (`read_path.rangedRead`, the warm
//! ladder's direct device leg, the IPC host's direct-drive READ) — at the
//! window's own grain, `GRAIN` = the LBA grain (4 KiB): the one size every
//! ranged window is a multiple of, so a deposit stores exactly the device
//! bytes the read paid for and a 4 KiB random read is ONE lookup. Whole-
//! block fills (streams) stay the hot tier's: a 4 MiB slot would be 1,024
//! grains here for no byte saved. Windows wider than the ranged bounce
//! slot (`INLINE_PUBLISH_MAX`, 64 KiB = 16 grains) are not deposited: past
//! it the ghost's second touch already escalates to a whole-block fill the
//! hot tier keeps, and 16 bounds a hit's lookups.
//!
//! Addressing: a grain's key is `(vol_tag, block offset, grain offset)` —
//! the same device-offset space every tier is addressed by, so the ONE
//! purge (`purge.zig`) reaches it with the block key alone: `purge(block)`
//! probes the block's grains, `purgeRange(block, rel, len)` exactly the
//! grains an in-place write (W1) rewrote. The key's SHARD hash covers the
//! block part only, so every grain of a block lives in one shard of the
//! core and a window serve, a deposit and a purge each take one lock; the
//! index's hash is the grain's own.
//!
//! The direction-specific parts are the Sink and the fill path: on
//! eviction the DROP sink releases the grain's slab slot under the lock
//! (the device still has the bytes); a deposit is validated at the insert
//! (the block's incarnation word still reads the pre-DMA snapshot) and
//! stamped with the filling inode's lease epoch (`lease_epoch.load`), so a
//! lookup after a mutation of that inode's records or custody — a local
//! write's admission, a layout publish, a token recall (`wire_plane.
//! RecallSink.noteRecalled` bumps it) — is a STALE REFUSAL even before the
//! purge lands; every serve is followed by the caller's binding recheck.
//! `read_cache_stale_served` is the must-stay-0 tripwire of the verify
//! seam (`SQUEEZEFS_TEST_READ_CACHE_VERIFY`).
//!
//! Memory: the slab is a `read_fill_pool.Pool` of `GRAIN` slots (lazily
//! populated; refcounted so a serve's pin outlives an eviction; `mincore`
//! residency; the idle trim), charged to the budget as `read_cache`
//! through the core's component. Yellow freezes growth (an insert evicts
//! first), Red deposits nothing and the shed evicts to the authority's
//! target.

const std = @import("std");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const budget = @import("../mem/budget.zig");
const cache = @import("../mem/cache.zig");
const rsc = @import("read_size_class.zig");
const fill_pool = @import("read_fill_pool.zig");
const lease_epoch = @import("lease_epoch.zig");

pub const BlockKey = rsc.BlockKey;
pub const Validate = cache.Validate;

/// The grain: the LBA grain every ranged window is a multiple of.
pub const GRAIN: u64 = rsc.LBA_GRAIN;
/// The widest window deposited: the ranged bounce slot (16 grains).
pub const WINDOW_MAX: u64 = rsc.INLINE_PUBLISH_MAX;
pub const WINDOW_MAX_GRAINS: usize = @intCast(WINDOW_MAX / GRAIN);
/// A shard holds ≥ 64 grains (256 KiB) so colliding blocks coexist (the
/// hot tier's min-shard lesson at this tier's grain).
pub const MIN_SHARD_BYTES: u64 = 64 * GRAIN;
/// The R5 component: floor 16 MiB (4,096 grains — one core's deepest
/// ranged burst of 16 × 16 grains, times the 16-shard small-box posture),
/// weight 4 (with the hot tier the largest sheddable class: pure cache,
/// refetchable at one device read per grain).
pub const COMPONENT_FLOOR: u64 = 16 * derive.MiB;
pub const COMPONENT_WEIGHT: u64 = 4;
/// The owner's default: a quarter of system RAM (Lustre's `max_cached_mb`
/// posture split between the read and the write cache).
pub const DEFAULT_PCT: u64 = 25;

pub const stats = struct {
    /// `read_cache_grain_purges`: grains dropped through the ONE purge (a
    /// local write, a free, a token recall).
    pub var read_cache_grain_purges: counters.ShardedCounter = .{};
    /// `read_cache_grain_binding_refusals`: serves withdrawn by the
    /// caller's binding recheck after the copy (a loss, retried — never a
    /// stale byte); the core's `read_cache_grain_stale_refusals` are the
    /// epoch's.
    pub var read_cache_grain_binding_refusals: counters.ShardedCounter = .{};
    /// `read_cache_grain_slab_exhausted`: a deposit found no free slot and
    /// evicted the tail first.
    pub var read_cache_grain_slab_exhausted: counters.ShardedCounter = .{};
    /// `read_cache_grain_window_puts`: windows deposited (the core's
    /// `fills` count grains).
    pub var read_cache_grain_window_puts: counters.ShardedCounter = .{};
    /// `read_cache_stale_served`: the verify seam re-read the device after
    /// a serve and the bytes differed (must stay 0).
    pub var read_cache_stale_served: counters.Tripwire = .init("read_cache_stale_served");
};

/// `SQUEEZEFS_READ_CACHE_PCT` (default 25; 0 = off) over the facts
/// (`cache.resolveBound`: the percentage of system RAM, scaled with the
/// budget's ceiling below the solo 70 %).
pub fn resolveReadCacheBytes(pct: u64, facts: derive.Facts, budget_bytes: u64) u64 {
    return cache.resolveBound(pct, facts, budget_bytes);
}

/// Slab slots for a budget: one per grain plus one per shard for the
/// deposit that triggers an eviction, plus the in-flight headroom of one
/// window per core (a window's slots are acquired before its shard lock).
pub fn slabSlots(budget_bytes: u64, shards: u32, cores: u32) u32 {
    return @intCast(budget_bytes / GRAIN + shards + @as(u64, cores) * WINDOW_MAX_GRAINS);
}

/// A grain's key: its device offset (the index's hash and the comparison)
/// and the block it belongs to (the SHARD's hash — one shard per block).
pub const GrainKey = struct {
    vol_tag: u64,
    block_off: u64,
    offset: u64,

    pub fn hash(self: GrainKey) u64 {
        return BlockKey.init(self.vol_tag, self.offset).hash();
    }

    pub fn shardHash(self: GrainKey) u64 {
        return BlockKey.init(self.vol_tag, self.block_off).hash();
    }

    pub fn eql(a: GrainKey, b: GrainKey) bool {
        return a.vol_tag == b.vol_tag and a.offset == b.offset;
    }
};

/// One cached grain: its slab slot.
pub const Grain = struct {
    pub const Key = GrainKey;
    key: Key,
    ino: u64,
    epoch: u64,
    bytes: u32,
    slot: u32,
};

/// The read direction's sink: the slot's tier reference released under
/// the shard lock — the device still has the bytes.
pub const DropSink = struct {
    pub const runs_under_lock = true;
    slab: *fill_pool.Pool,

    pub fn onEvict(self: *DropSink, g: *const Grain, _: cache.Why) cache.Outcome {
        _ = self.slab.unpin(g.slot);
        return .released;
    }
};

pub const Core = cache.Cache(Grain, DropSink);

fn leaseEpochOf(_: ?*anyopaque, ino: u64) u64 {
    return lease_epoch.load(ino);
}

pub const ReadCache = struct {
    gpa: Allocator,
    slab: *fill_pool.Pool,
    sink: DropSink,
    core: *Core,
    block_size: u64,
    budget_bytes: u64,

    pub const InitError = Allocator.Error || fill_pool.InitError;
    pub const PutOutcome = enum { inserted, refused, ineligible };

    /// `budget_bytes == 0` disables the tier (every probe misses, every
    /// deposit refuses, one slot and one shard are allocated).
    pub fn init(gpa: Allocator, block_size: u64, budget_bytes: u64, cores: u32) InitError!*ReadCache {
        assert(block_size >= GRAIN and derive.isAligned(block_size, GRAIN));
        const self = try gpa.create(ReadCache);
        errdefer gpa.destroy(self);
        const shard_count = if (budget_bytes == 0) 1 else cache.shardsFor(cores, budget_bytes, MIN_SHARD_BYTES);
        const slots = if (budget_bytes == 0) 1 else slabSlots(budget_bytes, shard_count, cores);
        const slab = try fill_pool.Pool.initRam(gpa, @intCast(GRAIN), slots);
        errdefer slab.deinit();
        // Items per shard: its share of grains + one for the deposit that
        // evicts + one window of headroom.
        const per_shard: usize = @intCast(budget_bytes / shard_count / GRAIN + 1 + WINDOW_MAX_GRAINS);
        self.* = .{ .gpa = gpa, .slab = slab, .sink = .{ .slab = slab }, .core = undefined, .block_size = block_size, .budget_bytes = budget_bytes };
        self.core = try Core.init(gpa, &self.sink, budget_bytes, shard_count, per_shard, .{ .call = &leaseEpochOf });
        return self;
    }

    pub fn deinit(self: *ReadCache) void {
        self.core.deinit();
        self.slab.deinit();
        self.gpa.destroy(self);
    }

    pub fn enabled(self: *const ReadCache) bool {
        return self.budget_bytes != 0;
    }

    pub fn shardCount(self: *const ReadCache) usize {
        return self.core.shards.len;
    }

    fn blockOf(self: *const ReadCache, key: BlockKey) BlockKey {
        return .init(key.vol_tag, key.offset - key.offset % self.block_size);
    }

    fn grainKey(block: BlockKey, rel: u64) GrainKey {
        return .{ .vol_tag = block.vol_tag, .block_off = block.offset, .offset = block.offset + rel };
    }

    /// A window `[rel_s, rel_e)` of a block is depositable here: grain-
    /// aligned, inside the block and at most `WINDOW_MAX` wide.
    pub fn windowEligible(self: *const ReadCache, rel_s: u64, rel_e: u64) bool {
        return rel_e > rel_s and rel_e - rel_s <= WINDOW_MAX and rel_e <= self.block_size and derive.isAligned(rel_s, GRAIN) and derive.isAligned(rel_e, GRAIN);
    }

    // ---- the tier face ----

    const Pins = struct {
        slots: [WINDOW_MAX_GRAINS]u32 = undefined,
        n: usize = 0,
        slab: *fill_pool.Pool,

        fn visit(self: *Pins, g: *const Grain) void {
            self.slab.pin(g.slot);
            self.slots[self.n] = g.slot;
            self.n += 1;
        }
    };

    /// Serve `[rel_s, rel_e)` of `block` into `out` through its 4 KiB-
    /// outward window (the ranged read's own, so an unaligned request hits
    /// the grains its DMA deposited): every grain of the window resident
    /// and current ⇒ the copy, each grain touched to the head; else null
    /// and `out` untouched. The grains are pinned under the lock (the
    /// core's visitor) and copied outside it, so a concurrent eviction
    /// frees nothing a copy still reads.
    pub fn get(self: *ReadCache, block: BlockKey, rel_s: u64, rel_e: u64, out: []u8) ?usize {
        if (!self.enabled()) return null;
        assert(out.len == rel_e - rel_s);
        const lo = rel_s - rel_s % GRAIN;
        const hi = derive.alignUp(rel_e, GRAIN);
        if (!self.windowEligible(lo, hi)) return null;
        var keys: [WINDOW_MAX_GRAINS]GrainKey = undefined;
        const n: usize = @intCast((hi - lo) / GRAIN);
        for (0..n) |i| keys[i] = grainKey(block, lo + @as(u64, i) * GRAIN);
        var pins: Pins = .{ .slab = self.slab };
        if (self.core.lookupMany(keys[0..n], &pins, Pins.visit) != n) {
            for (pins.slots[0..pins.n]) |slot| _ = self.slab.unpin(slot);
            return null;
        }
        for (pins.slots[0..n], 0..) |slot, i| {
            // The grain's intersection with the request.
            const g_lo = lo + @as(u64, i) * GRAIN;
            const from: usize = @intCast(@max(rel_s, g_lo) - g_lo);
            const to: usize = @intCast(@min(rel_e, g_lo + GRAIN) - g_lo);
            const at: usize = @intCast(@max(rel_s, g_lo) - rel_s);
            @memcpy(out[at..][0 .. to - from], self.slab.bytes(slot)[from..to]);
            _ = self.slab.unpin(slot);
        }
        return out.len;
    }

    /// Every grain of `[rel_s, rel_e)`'s window resident (no touch, no copy,
    /// no epoch judgement).
    pub fn contains(self: *ReadCache, block: BlockKey, rel_s: u64, rel_e: u64) bool {
        const lo = rel_s - rel_s % GRAIN;
        const hi = derive.alignUp(rel_e, GRAIN);
        if (!self.enabled() or !self.windowEligible(lo, hi)) return false;
        var off = lo;
        while (off < hi) : (off += GRAIN) {
            if (!self.core.contains(grainKey(block, off))) return false;
        }
        return true;
    }

    /// Deposit the window `data` = `[rel_s, rel_s + data.len)` of `block`,
    /// read for `ino` under lease epoch `epoch` (snapshotted BEFORE the
    /// DMA): the grains copied into fresh slots, then each inserted at the
    /// head under the validation (the block's word still the pre-DMA
    /// snapshot); a grain already resident is replaced (both copies were
    /// validated against the same word); `freeze` (Yellow+) keeps the
    /// shard's bytes at entry.
    pub fn putValidated(self: *ReadCache, block: BlockKey, rel_s: u64, data: []const u8, ino: u64, epoch: u64, validate: Validate, freeze: bool) PutOutcome {
        if (!self.enabled()) return .refused;
        if (!self.windowEligible(rel_s, rel_s + data.len)) return .ineligible;
        const n: usize = @intCast(data.len / GRAIN);
        var inserted: usize = 0;
        for (0..n) |i| {
            const slot = self.slab.acquire() orelse blk: {
                // The slab bounds slots as the budget bounds bytes: an
                // exhausted slab evicts a tail and retries once.
                stats.read_cache_grain_slab_exhausted.inc();
                _ = self.core.evictOne(grainKey(block, 0));
                break :blk self.slab.acquire() orelse break;
            };
            @memcpy(self.slab.bytes(slot)[0..GRAIN], data[i * GRAIN ..][0..GRAIN]);
            const key = grainKey(block, rel_s + @as(u64, i) * GRAIN);
            const g: Grain = .{ .key = key, .ino = ino, .epoch = epoch, .bytes = @intCast(GRAIN), .slot = slot };
            var out = self.core.insert(g, .{ .validate = validate, .freeze = freeze });
            if (out == .exists) {
                _ = self.core.invalidateKey(key);
                out = self.core.insert(g, .{ .validate = validate, .freeze = freeze });
            }
            if (out != .inserted) {
                _ = self.slab.unpin(slot);
                break;
            }
            inserted += 1;
        }
        if (inserted == 0) return .refused;
        stats.read_cache_grain_window_puts.inc();
        return .inserted;
    }

    /// The purge arm (`purge.zig`): every grain of `key`'s block. `key`
    /// may be the block key or any grain key within it.
    pub fn purge(self: *ReadCache, key: BlockKey) bool {
        return self.purgeRange(key, 0, self.block_size) > 0;
    }

    /// The ranged purge: the grains of `[rel_s, rel_s + len)` of `key`'s
    /// block (an in-place write's exact window; the rest of the block's
    /// grains stay). Returns the grains dropped.
    pub fn purgeRange(self: *ReadCache, key: BlockKey, rel_s: u64, len: u64) u32 {
        if (!self.enabled() or len == 0) return 0;
        const block = self.blockOf(key);
        const lo = rel_s - rel_s % GRAIN;
        const hi = @min(derive.alignUp(rel_s + len, GRAIN), self.block_size);
        const shard = self.core.shardFor(grainKey(block, 0));
        var dropped: u32 = 0;
        var keys: [64]GrainKey = undefined;
        var off = lo;
        while (off < hi) {
            var n: usize = 0;
            while (off < hi and n < keys.len) : (off += GRAIN) {
                keys[n] = grainKey(block, off);
                n += 1;
            }
            dropped += self.core.invalidateKeys(shard, keys[0..n]);
        }
        if (dropped > 0) stats.read_cache_grain_purges.add(dropped);
        return dropped;
    }

    /// The census face (`purge.purgeCensus`): the next resident grain's
    /// BLOCK key at or after `cursor` that `select` picks; the cursor moves
    /// past the grain (the purge drops the whole block, so the shard's
    /// later grains of it no longer match).
    pub fn nextKey(self: *ReadCache, cursor: *u64, select: rsc.KeySelect) ?BlockKey {
        const Pick = struct {
            fn blockPicked(sel: *const rsc.KeySelect, g: *const Grain) bool {
                return sel.picks(BlockKey.init(g.key.vol_tag, g.key.block_off));
            }
        };
        const g = self.core.censusNext(cursor, &select, Pick.blockPicked) orelse return null;
        return BlockKey.init(g.key.vol_tag, g.key.block_off);
    }

    /// Evict LRU tails until the entry bytes ≤ `target`.
    pub fn shedTo(self: *ReadCache, target: u64) void {
        _ = self.core.evictUntil(target);
    }

    pub fn currentBytes(self: *const ReadCache) u64 {
        return self.core.currentBytes();
    }

    /// The RAM this tier holds: its slab's resident bytes (entries plus the
    /// freed slots the kernel still backs), as the read LRU gauges.
    fn gaugeFn(ctx: *anyopaque) u64 {
        const self: *ReadCache = @ptrCast(@alignCast(ctx));
        return self.slab.residentBytes();
    }

    fn trimFn(ctx: *anyopaque, min_idle_ticks: u64) void {
        const self: *ReadCache = @ptrCast(@alignCast(ctx));
        self.slab.trim(min_idle_ticks);
    }

    /// Evict to the target through the core, then return the idle slots the
    /// eviction freed (Red: everything idle) so the gauge moves in the
    /// same call.
    fn shedFn(ctx: *anyopaque, target: u64) void {
        const self: *ReadCache = @ptrCast(@alignCast(ctx));
        self.shedTo(target);
        self.slab.trimToward(budget.trim_red_min_idle_ticks, target -| self.slab.checkedOutBytes());
    }

    /// `read_cache`: floor 16 MiB, weight 4; the shed is the core's LRU
    /// eviction followed by the slab's idle trim.
    pub fn component(self: *ReadCache) budget.Component {
        return budget.Component.init("read_cache", @min(COMPONENT_FLOOR, self.budget_bytes), COMPONENT_WEIGHT, .{ .func = .{ .ctx = self, .call = &gaugeFn } }, .{ .ctx = self, .call = &shedFn }).withTrim(.{ .ctx = self, .call = &trimFn });
    }

    /// The `.stats` family: the core's instruments under the `read_cache_
    /// grain` prefix plus this tier's own.
    pub fn writeStatsJson(self: *const ReadCache, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.writeAll("{");
        try self.core.stats.writeJson(w, "read_cache_grain");
        try w.print(",\"read_cache_grain_purges\":{d},\"read_cache_grain_binding_refusals\":{d},\"read_cache_grain_slab_exhausted\":{d},\"read_cache_grain_window_puts\":{d},\"read_cache_stale_served\":{d}}}", .{
            stats.read_cache_grain_purges.load(),
            stats.read_cache_grain_binding_refusals.load(),
            stats.read_cache_grain_slab_exhausted.load(),
            stats.read_cache_grain_window_puts.load(),
            stats.read_cache_stale_served.load(),
        });
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "tie: 25 % of 100 GiB RAM is a 25 GiB read cache; a cage below the solo 70 % scales it by the ceiling; 0 % is off; the grain, the window, the slots" {
    const gib = derive.GiB;
    const facts = derive.Facts.synthetic(32, 100 * gib);
    try testing.expectEqual(@as(u64, 25 * gib), resolveReadCacheBytes(DEFAULT_PCT, facts, 70 * gib));
    try testing.expectEqual(@as(u64, 8 * gib / 7 * 10 / 4), resolveReadCacheBytes(25, facts, 8 * gib));
    try testing.expectEqual(@as(u64, 0), resolveReadCacheBytes(0, facts, 70 * gib));
    try testing.expectEqual(@as(u64, 4096), GRAIN);
    try testing.expectEqual(@as(usize, 16), WINDOW_MAX_GRAINS);
    // Slots: grains + shards + one window per core.
    try testing.expectEqual(@as(u32, 1024 + 2 + 2 * 16), slabSlots(4 * derive.MiB, 2, 2));
    // Every grain of a block SHARDS to the block; the index hashes and
    // compares the grain (one hash for both made a block's grains one
    // probe cluster: the lanes burned 96 % of the box on the first A/B).
    const a: GrainKey = .{ .vol_tag = 1, .block_off = 1 << 22, .offset = (1 << 22) + 4096 };
    const b: GrainKey = .{ .vol_tag = 1, .block_off = 1 << 22, .offset = (1 << 22) + 8192 };
    try testing.expectEqual(a.shardHash(), b.shardHash());
    try testing.expect(a.hash() != b.hash());
    try testing.expect(!a.eql(b));
    try testing.expectEqual(BlockKey.init(1, 1 << 22).hash(), a.shardHash());
}

fn fillGrain(buf: []u8, tag: u8) void {
    @memset(buf, tag);
}

test "the read cache: a deposited window serves grain-exact, partial and unaligned windows, touches move an entry to the head, the tail is the least recently used, an absent grain is a miss, 0 bytes is off" {
    const bs: u64 = 64 * GRAIN;
    // Budget 4 grains in ONE shard.
    const rc = try ReadCache.init(testing.allocator, bs, 4 * GRAIN, 1);
    defer rc.deinit();
    try testing.expectEqual(@as(usize, 1), rc.shardCount());
    const blk = BlockKey.init(3, 2 * bs);
    var win: [2 * GRAIN]u8 = undefined;
    fillGrain(win[0..GRAIN], 0xA1);
    fillGrain(win[GRAIN..], 0xA2);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 8 * GRAIN, &win, 7, 0, .always, false));
    try testing.expectEqual(@as(u64, 2 * GRAIN), rc.currentBytes());
    var out: [2 * GRAIN]u8 = undefined;
    try testing.expectEqual(@as(?usize, 2 * GRAIN), rc.get(blk, 8 * GRAIN, 10 * GRAIN, &out));
    try testing.expectEqualSlices(u8, &win, &out);
    try testing.expectEqual(@as(?usize, GRAIN), rc.get(blk, 9 * GRAIN, 10 * GRAIN, out[0..GRAIN]));
    try testing.expectEqual(@as(u8, 0xA2), out[0]);
    // An unaligned request inside the window: its 4 KiB-outward window
    // is resident, the slice is exact across the grain boundary.
    try testing.expectEqual(@as(?usize, 300), rc.get(blk, 9 * GRAIN - 100, 9 * GRAIN + 200, out[0..300]));
    try testing.expectEqual(@as(u8, 0xA1), out[0]);
    try testing.expectEqual(@as(u8, 0xA1), out[99]);
    try testing.expectEqual(@as(u8, 0xA2), out[100]);
    try testing.expectEqual(@as(u8, 0xA2), out[299]);
    // A grain absent ⇒ miss (the window straddles into unresident grains).
    try testing.expectEqual(@as(?usize, null), rc.get(blk, 9 * GRAIN, 11 * GRAIN, &out));
    try testing.expect(rc.contains(blk, 8 * GRAIN, 10 * GRAIN));
    try testing.expect(!rc.contains(blk, 7 * GRAIN, 9 * GRAIN));
    // Two more grains fill the budget; the oldest untouched is grain 8 (9
    // was just served). The fifth evicts exactly it.
    var g: [GRAIN]u8 = undefined;
    fillGrain(&g, 0xB1);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 20 * GRAIN, &g, 7, 0, .always, false));
    fillGrain(&g, 0xB2);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 21 * GRAIN, &g, 7, 0, .always, false));
    try testing.expectEqual(@as(u64, 4 * GRAIN), rc.currentBytes());
    // Touch grain 9 again so the order (MRU → LRU) is 9, 21, 20, 8.
    try testing.expectEqual(@as(?usize, GRAIN), rc.get(blk, 9 * GRAIN, 10 * GRAIN, out[0..GRAIN]));
    const ev0 = rc.core.stats.evictions.load();
    fillGrain(&g, 0xC1);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 30 * GRAIN, &g, 7, 0, .always, false));
    try testing.expectEqual(ev0 + 1, rc.core.stats.evictions.load());
    try testing.expect(!rc.contains(blk, 8 * GRAIN, 9 * GRAIN));
    try testing.expect(rc.contains(blk, 9 * GRAIN, 10 * GRAIN));
    try testing.expect(rc.contains(blk, 20 * GRAIN, 22 * GRAIN));
    try testing.expect(rc.contains(blk, 30 * GRAIN, 31 * GRAIN));
    // The next victim is 20 (LRU after 8), not 9 (touched) — exact recency.
    fillGrain(&g, 0xC2);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 31 * GRAIN, &g, 7, 0, .always, false));
    try testing.expect(!rc.contains(blk, 20 * GRAIN, 21 * GRAIN));
    try testing.expect(rc.contains(blk, 9 * GRAIN, 10 * GRAIN));
    // A replaced grain carries the new bytes.
    fillGrain(&g, 0xD9);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 9 * GRAIN, &g, 7, 0, .always, false));
    try testing.expectEqual(@as(?usize, GRAIN), rc.get(blk, 9 * GRAIN, 10 * GRAIN, out[0..GRAIN]));
    try testing.expectEqual(@as(u8, 0xD9), out[0]);
    try testing.expectEqual(@as(u64, 4 * GRAIN), rc.currentBytes());
    // Ineligible windows: unaligned, too wide, past the block.
    try testing.expectEqual(ReadCache.PutOutcome.ineligible, rc.putValidated(blk, 1, &g, 7, 0, .always, false));
    try testing.expectEqual(ReadCache.PutOutcome.ineligible, rc.putValidated(blk, bs - GRAIN, &win, 7, 0, .always, false));
    var wide: [WINDOW_MAX + GRAIN]u8 = undefined;
    try testing.expectEqual(ReadCache.PutOutcome.ineligible, rc.putValidated(blk, 0, &wide, 7, 0, .always, false));
    // Off.
    const off = try ReadCache.init(testing.allocator, bs, 0, 1);
    defer off.deinit();
    try testing.expect(!off.enabled());
    try testing.expectEqual(ReadCache.PutOutcome.refused, off.putValidated(blk, 0, &g, 7, 0, .always, false));
    try testing.expectEqual(@as(?usize, null), off.get(blk, 0, GRAIN, out[0..GRAIN]));
    try testing.expect(!off.purge(blk));
}

test "the read cache: the ranged purge drops exactly the written grains, the block purge every grain of the block and nothing of its neighbour, the census names blocks the selector picks, a refused validation deposits nothing, freeze is net-zero growth, shed evicts tails, the component gauges the slab and sheds through the core, the stats family names the core's and its own" {
    const bs: u64 = 64 * GRAIN;
    const rc = try ReadCache.init(testing.allocator, bs, 8 * GRAIN, 1);
    defer rc.deinit();
    const a = BlockKey.init(1, 0);
    const b = BlockKey.init(1, bs);
    var win: [4 * GRAIN]u8 = undefined;
    fillGrain(&win, 0x11);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(a, 0, &win, 7, 0, .always, false));
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(b, 4 * GRAIN, win[0 .. 2 * GRAIN], 8, 0, .always, false));
    try testing.expectEqual(@as(u64, 6 * GRAIN), rc.currentBytes());
    // W1 rewrote [GRAIN + 100, 2 × GRAIN + 100) of a: grains 1 and 2 go.
    try testing.expectEqual(@as(u32, 2), rc.purgeRange(a, GRAIN + 100, GRAIN));
    try testing.expect(rc.contains(a, 0, GRAIN));
    try testing.expect(!rc.contains(a, GRAIN, 2 * GRAIN));
    try testing.expect(!rc.contains(a, 2 * GRAIN, 3 * GRAIN));
    try testing.expect(rc.contains(a, 3 * GRAIN, 4 * GRAIN));
    try testing.expect(rc.contains(b, 4 * GRAIN, 6 * GRAIN));
    // A grain key of a purges a's block, not b's.
    try testing.expect(rc.purge(.init(1, 3 * GRAIN)));
    try testing.expect(!rc.contains(a, 0, GRAIN));
    try testing.expect(rc.contains(b, 4 * GRAIN, 6 * GRAIN));
    try testing.expect(!rc.purge(a));
    try testing.expectEqual(@as(u64, 2 * GRAIN), rc.currentBytes());
    // The census: a selector picking everything names b; the purge of b
    // drops both grains and the census is then empty.
    const Pick = struct {
        fn all(_: *anyopaque, _: BlockKey) bool {
            return true;
        }
    };
    var sink: u8 = 0;
    var cursor: u64 = 0;
    const first = rc.nextKey(&cursor, .{ .ctx = &sink, .call = &Pick.all }).?;
    try testing.expect(first.eql(b));
    try testing.expect(rc.purge(first));
    try testing.expectEqual(@as(?BlockKey, null), rc.nextKey(&cursor, .{ .ctx = &sink, .call = &Pick.all }));
    try testing.expectEqual(@as(u64, 0), rc.currentBytes());
    // A refused validation deposits nothing (the slots return).
    const Refuse = struct {
        fn no(_: ?*anyopaque) bool {
            return false;
        }
    };
    const free_before = rc.slab.freeSlots();
    try testing.expectEqual(ReadCache.PutOutcome.refused, rc.putValidated(a, 0, &win, 7, 0, .{ .ctx = null, .call = &Refuse.no }, false));
    try testing.expectEqual(free_before, rc.slab.freeSlots());
    try testing.expectEqual(@as(u64, 0), rc.currentBytes());
    // Freeze: 4 grains in, then a frozen 2-grain deposit evicts 2 first.
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(a, 0, &win, 7, 0, .always, false));
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(b, 0, win[0 .. 2 * GRAIN], 8, 0, .always, true));
    try testing.expectEqual(@as(u64, 4 * GRAIN), rc.currentBytes());
    try testing.expect(!rc.contains(a, 0, 2 * GRAIN));
    try testing.expect(rc.contains(a, 2 * GRAIN, 4 * GRAIN));
    try testing.expect(rc.contains(b, 0, 2 * GRAIN));
    // Shed to one grain.
    rc.shedTo(GRAIN);
    try testing.expectEqual(GRAIN, rc.currentBytes());
    try testing.expect(rc.contains(b, GRAIN, 2 * GRAIN));
    const comp = rc.component();
    try testing.expectEqualStrings("read_cache", comp.name);
    try testing.expectEqual(@min(COMPONENT_FLOOR, 8 * GRAIN), comp.floor);
    try testing.expectEqual(COMPONENT_WEIGHT, comp.weight);
    try testing.expectEqual(rc.slab.residentBytes(), comp.current());
    comp.shed.?.call(comp.shed.?.ctx, 0);
    try testing.expectEqual(@as(u64, 0), rc.currentBytes());
    try testing.expectEqual(@as(usize, rc.slab.capacity), rc.slab.freeSlots());
    try testing.expect(comp.trim != null);
    var buf: [2048]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try rc.writeStatsJson(&w);
    const s = w.buffered();
    try testing.expect(s[0] == '{' and s[s.len - 1] == '}');
    for ([_][]const u8{ "read_cache_grain_hits", "read_cache_grain_evictions", "read_cache_grain_binding_refusals", "read_cache_grain_purges", "read_cache_grain_window_puts", "read_cache_stale_served", "read_cache_grain_bound_bytes" }) |k| {
        try testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}

test "the read cache: a grain filled under an inode's lease epoch is refused (and dropped) once that epoch moved — the lookup's coherence law; a fill under the new epoch serves" {
    const bs: u64 = 64 * GRAIN;
    const rc = try ReadCache.init(testing.allocator, bs, 8 * GRAIN, 1);
    defer rc.deinit();
    const ino: u64 = 0x4e4e_0001;
    const blk = BlockKey.init(2, bs);
    var g: [GRAIN]u8 = undefined;
    fillGrain(&g, 0x33);
    const e0 = lease_epoch.load(ino);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 0, &g, ino, e0, .always, false));
    var out: [GRAIN]u8 = undefined;
    try testing.expectEqual(@as(?usize, GRAIN), rc.get(blk, 0, GRAIN, &out));
    // A write admission / a token recall on the ino: the entry is stale.
    lease_epoch.bump(ino);
    const stale0 = rc.core.stats.stale_refusals.load();
    try testing.expectEqual(@as(?usize, null), rc.get(blk, 0, GRAIN, &out));
    try testing.expectEqual(stale0 + 1, rc.core.stats.stale_refusals.load());
    try testing.expect(!rc.contains(blk, 0, GRAIN));
    try testing.expectEqual(@as(u64, 0), rc.currentBytes());
    fillGrain(&g, 0x44);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 0, &g, ino, lease_epoch.load(ino), .always, false));
    try testing.expectEqual(@as(?usize, GRAIN), rc.get(blk, 0, GRAIN, &out));
    try testing.expectEqual(@as(u8, 0x44), out[0]);
}

test "the read cache: a served grain pinned across an eviction keeps its bytes until the copy is done (the slot is not reused while pinned)" {
    const bs: u64 = 16 * GRAIN;
    const rc = try ReadCache.init(testing.allocator, bs, GRAIN, 1);
    defer rc.deinit();
    const a = BlockKey.init(9, 0);
    var g: [GRAIN]u8 = undefined;
    fillGrain(&g, 0x5A);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(a, 0, &g, 7, 0, .always, false));
    // Pin the entry's slot as a serve would, evict it by depositing another
    // grain, and check the pinned slot still reads the old bytes and the
    // new grain took a different slot.
    var pins: ReadCache.Pins = .{ .slab = rc.slab };
    try testing.expectEqual(@as(usize, 1), rc.core.lookupMany(&.{ReadCache.grainKey(a, 0)}, &pins, ReadCache.Pins.visit));
    const slot = pins.slots[0];
    fillGrain(&g, 0xC3);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(a, GRAIN, &g, 7, 0, .always, false));
    try testing.expect(!rc.contains(a, 0, GRAIN));
    try testing.expectEqual(@as(u8, 0x5A), rc.slab.bytes(slot)[0]);
    var pins2: ReadCache.Pins = .{ .slab = rc.slab };
    try testing.expectEqual(@as(usize, 1), rc.core.lookupMany(&.{ReadCache.grainKey(a, GRAIN)}, &pins2, ReadCache.Pins.visit));
    try testing.expect(pins2.slots[0] != slot);
    _ = rc.slab.unpin(pins2.slots[0]);
    try testing.expect(rc.slab.unpin(slot));
}

test "the read cache: a sharded instance keeps every grain of a block in one shard and evicts each shard's own tail" {
    const bs: u64 = 1024 * GRAIN;
    const rc = try ReadCache.init(testing.allocator, bs, 32 * MIN_SHARD_BYTES, 32);
    defer rc.deinit();
    try testing.expect(rc.shardCount() > 1);
    var win: [2 * GRAIN]u8 = undefined;
    fillGrain(&win, 0x77);
    var k: u64 = 0;
    while (k < 64) : (k += 1) {
        const blk = BlockKey.init(5, k * bs);
        try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 0, &win, 7, 0, .always, false));
        try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 10 * GRAIN, &win, 7, 0, .always, false));
        try testing.expectEqual(rc.core.shardIndex(ReadCache.grainKey(blk, 0)), rc.core.shardIndex(ReadCache.grainKey(blk, 11 * GRAIN)));
    }
    try testing.expectEqual(@as(u64, 64 * 4 * GRAIN), rc.currentBytes());
    // Fill one shard past its share: only that shard's tail moves.
    const victim_blk = BlockKey.init(5, 0);
    const vs = &rc.core.shards[rc.core.shardIndex(ReadCache.grainKey(victim_blk, 0))];
    const others_before = rc.currentBytes() - vs.bytes.load(.monotonic);
    var filler: [GRAIN]u8 = undefined;
    fillGrain(&filler, 0x01);
    var off: u64 = 20 * GRAIN;
    while (vs.bytes.load(.monotonic) + GRAIN <= vs.max_bytes) : (off += GRAIN) {
        try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(victim_blk, off, &filler, 7, 0, .always, false));
    }
    const ev0 = rc.core.stats.evictions.load();
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(victim_blk, off, &filler, 7, 0, .always, false));
    try testing.expectEqual(ev0 + 1, rc.core.stats.evictions.load());
    try testing.expectEqual(others_before, rc.currentBytes() - vs.bytes.load(.monotonic));
    // The evicted grain was this shard's oldest deposit: grain 0 of block 0.
    try testing.expect(!rc.contains(victim_blk, 0, GRAIN));
    try testing.expect(rc.contains(victim_blk, GRAIN, 2 * GRAIN));
}
