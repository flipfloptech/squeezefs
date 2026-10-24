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
//! Memory: the slab is a `GrainSlab` of `GRAIN` slots — the reservation
//! (the knob's share of the MACHINE's RAM, so a share that drops lets the
//! bound grow) in one lazily populated mapping, one stripe per core shard,
//! every per-slot table lazily materialized: an idle instance holds the
//! grains it filled, never its reservation (the idle-RSS law, `docs/
//! evidence/rung-10-idle-rss.md` — the fill pool's ring and arrays over a
//! 109 GiB machine's 7.2 M grains were 224 MiB of an idle daemon).
//! Refcounted so a serve's pin outlives an eviction; the idle trim returns
//! released grains oldest first. Charged to the budget as `read_cache`
//! through the core's component. Yellow freezes growth (an insert evicts
//! first), Red deposits nothing and the shed evicts to the authority's
//! target.

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const budget = @import("../mem/budget.zig");
const cache = @import("../mem/cache.zig");
const lazy = @import("../mem/lazy.zig");
const jc = @import("../meta/journal_core.zig");
const rsc = @import("read_size_class.zig");
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
    /// `read_cache_grain_trimmed_bytes`: released grains whose pages the
    /// idle trim (Yellow/Red ticks, the Red shed) returned.
    pub var read_cache_grain_trimmed_bytes: counters.ShardedCounter = .{};
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

/// The slab's reservation: the knob's share of the machine's RAM, the
/// most the live bound can grow to (a lazily populated mapping).
pub fn derivedCapacity(pct: u64, machine_ram: u64) u64 {
    return cache.derivedBound(pct, machine_ram, machine_ram);
}

/// A shard's entries over a reservation: its share of the reservation's
/// grains, plus one for the deposit that triggers an eviction, plus one
/// window of in-flight headroom (a window's slots are acquired before its
/// shard lock). The same count sizes the shard's item table and its slab
/// stripe; the index holds the live bound's (`Items.indexFor`).
pub fn perShardSlots(reservation: u64, shards: u32) u32 {
    return @intCast(reservation / shards / GRAIN + 1 + WINDOW_MAX_GRAINS);
}

/// The core's sizing: the table the reservation's, the index the bound's.
pub fn coreItems(reservation: u64, shards: u32) cache.Items {
    return .{ .max = perShardSlots(reservation, shards), .entry_floor = GRAIN, .slack = 1 + WINDOW_MAX_GRAINS };
}

/// THE GRAIN SLAB: the reservation's `GRAIN` slots in ONE lazily populated
/// anonymous mapping, split into one STRIPE per core shard — a shard's
/// grains live in its stripe, so the slot an eviction frees is the one
/// the shard's next deposit takes. Per stripe, under its spin lock: a bump
/// cursor over the never-used slots, the WARM chain of released slots
/// (resident: a fill wrote them; most recent at the head, so a deposit
/// reuses the hottest page and the idle trim takes the tail — the oldest
/// release) and the COLD chain of trimmed ones (pages returned). Per slot,
/// lazily materialized (`mem/lazy.zig`): the refcount (the tier's
/// reference + each serve's pin: the last release chains the slot, so a
/// pinned slot is never reused under a copy), the chain links and the idle
/// clock's stamp. Nothing is allocated per op; an idle instance costs the
/// slots it has filled. Residency is counted, not measured: a slot is
/// resident from its fill until its trim (`GRAIN` is the page).
pub const GrainSlab = struct {
    pub const Slot = u32;
    /// Chain links hold `slot + 1` so the lazily zeroed table reads empty.
    const NONE: u32 = 0;

    const Stripe = struct {
        lock: jc.SpinLock align(std.atomic.cache_line) = .{},
        /// Slots `[0, fresh)` of the stripe were handed out at least once.
        fresh: u32 = 0,
        warm_head: u32 = NONE,
        warm_tail: u32 = NONE,
        cold_head: u32 = NONE,
    };

    gpa: Allocator,
    base: [*]align(std.heap.page_size_min) u8,
    bytes_len: usize,
    capacity: u32,
    per_stripe: u32,
    stripes: []Stripe,
    refs: []Atomic(u32),
    next: []u32,
    prev: []u32,
    freed_tick: []Atomic(u64),
    /// The idle clock: the sampler's tick count (a test points it at its own).
    clock: *const Atomic(u64) = &budget.authority.tick_count,
    checked_out_bytes: counters.Gauge = .{},
    /// The warm chains' bytes: released, still resident.
    warm_bytes: counters.Gauge = .{},

    pub fn init(gpa: Allocator, stripes: u32, per_stripe: u32) Allocator.Error!*GrainSlab {
        assert(stripes > 0 and per_stripe > 0);
        const capacity: u32 = std.math.mul(u32, stripes, per_stripe) catch return error.OutOfMemory;
        const self = try gpa.create(GrainSlab);
        errdefer gpa.destroy(self);
        const slab_bytes = try lazy.alloc(u8, @as(usize, capacity) * GRAIN);
        errdefer lazy.free(u8, slab_bytes);
        const stripe_tab = try gpa.alloc(Stripe, stripes);
        errdefer gpa.free(stripe_tab);
        @memset(stripe_tab, .{});
        const refs = try lazy.alloc(Atomic(u32), capacity);
        errdefer lazy.free(Atomic(u32), refs);
        const next = try lazy.alloc(u32, capacity);
        errdefer lazy.free(u32, next);
        const prev = try lazy.alloc(u32, capacity);
        errdefer lazy.free(u32, prev);
        const freed_tick = try lazy.alloc(Atomic(u64), capacity);
        self.* = .{ .gpa = gpa, .base = @alignCast(slab_bytes.ptr), .bytes_len = slab_bytes.len, .capacity = capacity, .per_stripe = per_stripe, .stripes = stripe_tab, .refs = refs, .next = next, .prev = prev, .freed_tick = freed_tick };
        return self;
    }

    pub fn deinit(self: *GrainSlab) void {
        lazy.free(Atomic(u64), self.freed_tick);
        lazy.free(u32, self.prev);
        lazy.free(u32, self.next);
        lazy.free(Atomic(u32), self.refs);
        self.gpa.free(self.stripes);
        lazy.free(u8, self.base[0..self.bytes_len]);
        self.gpa.destroy(self);
    }

    /// Test seam: the idle clock.
    pub fn setClock(self: *GrainSlab, clock: *const Atomic(u64)) void {
        self.clock = clock;
    }

    fn stripeOf(self: *const GrainSlab, slot: Slot) *Stripe {
        return &self.stripes[slot / self.per_stripe];
    }

    fn unlinkWarm(self: *GrainSlab, st: *Stripe, slot: Slot) void {
        const p = self.prev[slot];
        const n = self.next[slot];
        if (p != NONE) self.next[p - 1] = n else st.warm_head = n;
        if (n != NONE) self.prev[n - 1] = p else st.warm_tail = p;
        self.warm_bytes.sub(GRAIN);
    }

    /// A slot of stripe `stripe` at refcount 1: the warm head, else a
    /// cold slot, else a never-used one; null when the stripe is full.
    pub fn acquire(self: *GrainSlab, stripe: usize) ?Slot {
        const st = &self.stripes[stripe];
        const slot: Slot = blk: {
            st.lock.lock();
            defer st.lock.unlock();
            if (st.warm_head != NONE) {
                const s = st.warm_head - 1;
                self.unlinkWarm(st, s);
                break :blk s;
            }
            if (st.cold_head != NONE) {
                const s = st.cold_head - 1;
                st.cold_head = self.next[s];
                break :blk s;
            }
            if (st.fresh < self.per_stripe) {
                const s: Slot = @intCast(stripe * self.per_stripe + st.fresh);
                st.fresh += 1;
                break :blk s;
            }
            return null;
        };
        assert(self.refs[slot].load(.monotonic) == 0);
        self.refs[slot].store(1, .monotonic);
        self.checked_out_bytes.add(GRAIN);
        return slot;
    }

    /// One more reference (a serve's pin, taken under the shard lock while
    /// the tier's reference stands, so the slot cannot be freed under it).
    pub fn pin(self: *GrainSlab, slot: Slot) void {
        const prev = self.refs[slot].fetchAdd(1, .monotonic);
        assert(prev != 0);
    }

    /// Drop a reference; true when this was the last and the slot went to
    /// its stripe's warm head. acq_rel: the last release orders every
    /// reader's loads before the chain push, so a re-acquirer's fill never
    /// races a straggling copy.
    pub fn unpin(self: *GrainSlab, slot: Slot) bool {
        const prev = self.refs[slot].fetchSub(1, .acq_rel);
        assert(prev != 0);
        if (prev != 1) return false;
        self.checked_out_bytes.sub(GRAIN);
        self.freed_tick[slot].store(self.clock.load(.monotonic), .monotonic);
        const st = self.stripeOf(slot);
        st.lock.lock();
        defer st.lock.unlock();
        self.prev[slot] = NONE;
        self.next[slot] = st.warm_head;
        if (st.warm_head != NONE) self.prev[st.warm_head - 1] = slot + 1 else st.warm_tail = slot + 1;
        st.warm_head = slot + 1;
        self.warm_bytes.add(GRAIN);
        return true;
    }

    pub fn bytes(self: *GrainSlab, slot: Slot) []align(std.heap.page_size_min) u8 {
        return @alignCast(self.base[@as(usize, slot) * GRAIN ..][0..GRAIN]);
    }

    pub fn checkedOutBytes(self: *const GrainSlab) u64 {
        return self.checked_out_bytes.load();
    }

    /// Everything the slab holds resident: the checked-out grains and the
    /// warm (released, untrimmed) ones.
    pub fn residentBytes(self: *const GrainSlab) u64 {
        return self.checked_out_bytes.load() + self.warm_bytes.load();
    }

    pub fn freeSlots(self: *const GrainSlab) usize {
        return self.capacity - @as(usize, @intCast(self.checked_out_bytes.load() / GRAIN));
    }

    /// The idle trim (`budget.TrimFn`): every warm slot released ≥
    /// `min_idle_ticks` ago goes cold.
    pub fn trim(self: *GrainSlab, min_idle_ticks: u64) void {
        self.trimToward(min_idle_ticks, 0);
    }

    /// Return warm slots, each stripe's oldest release first, until the
    /// warm bytes are ≤ `target_warm_bytes` (or every stripe's tail is
    /// younger than `min_idle_ticks`). A slot is unlinked under its
    /// stripe's lock (no acquirer can take it), its pages returned outside
    /// it, then chained cold.
    pub fn trimToward(self: *GrainSlab, min_idle_ticks: u64, target_warm_bytes: u64) void {
        const now = self.clock.load(.monotonic);
        for (self.stripes) |*st| {
            while (self.warm_bytes.load() > target_warm_bytes) {
                const slot: Slot = blk: {
                    st.lock.lock();
                    defer st.lock.unlock();
                    if (st.warm_tail == NONE) break;
                    const s = st.warm_tail - 1;
                    if (now -| self.freed_tick[s].load(.monotonic) < min_idle_ticks) break;
                    self.unlinkWarm(st, s);
                    break :blk s;
                };
                if (builtin.os.tag == .linux) _ = std.os.linux.madvise(self.bytes(slot).ptr, GRAIN, std.os.linux.MADV.DONTNEED);
                stats.read_cache_grain_trimmed_bytes.add(GRAIN);
                st.lock.lock();
                defer st.lock.unlock();
                self.next[slot] = st.cold_head;
                st.cold_head = slot + 1;
            }
        }
    }
};

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
    slab: *GrainSlab,

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
    slab: *GrainSlab,
    sink: DropSink,
    core: *Core,
    block_size: u64,
    /// The bound at init (0 = the tier off for the mount's life).
    budget_bytes: u64,
    /// `SQUEEZEFS_READ_CACHE_PCT`, for the live re-derivation.
    pct: u64,
    /// The slab's reservation: the bound can grow to this and no further.
    capacity_bytes: u64,

    pub const InitError = Allocator.Error;
    pub const PutOutcome = enum { inserted, refused, ineligible };

    /// `budget_bytes` is the bound at init (`cache.resolveBound` over the
    /// mount's facts; 0 disables the tier: every probe misses, every
    /// deposit refuses, one slot and one shard are allocated).
    /// `capacity_bytes` is the reservation — the percentage of the
    /// MACHINE's RAM (`derive.Facts.probe()`, before the fleet share
    /// divides it): lazily populated slab and tables, so an unreached
    /// bound costs nothing; the live bound re-derived at every budget tick
    /// (`rederive`) grows up to it when the share drops (the mount's own
    /// CLI and supervisor are three `squeezefs` processes at mount time:
    /// every supervised mount resolves its first budget at a third).
    pub fn init(gpa: Allocator, block_size: u64, pct: u64, budget_bytes: u64, capacity_bytes: u64, cores: u32) InitError!*ReadCache {
        assert(block_size >= GRAIN and derive.isAligned(block_size, GRAIN));
        const self = try gpa.create(ReadCache);
        errdefer gpa.destroy(self);
        // Off: one shard of one slot, nothing reserved.
        const cap = if (budget_bytes == 0) 0 else @max(capacity_bytes, budget_bytes);
        const shard_count = if (budget_bytes == 0) 1 else cache.shardsFor(cores, cap, MIN_SHARD_BYTES);
        const items: cache.Items = if (budget_bytes == 0) .{ .max = 1 } else coreItems(cap, shard_count);
        const slab = try GrainSlab.init(gpa, shard_count, @intCast(items.max));
        errdefer slab.deinit();
        self.* = .{ .gpa = gpa, .slab = slab, .sink = .{ .slab = slab }, .core = undefined, .block_size = block_size, .budget_bytes = budget_bytes, .pct = pct, .capacity_bytes = cap };
        self.core = try Core.init(gpa, &self.sink, budget_bytes, shard_count, items, .{ .call = &leaseEpochOf });
        return self;
    }

    /// Every grain dropped (a re-join in place, decision 0003 §BO: a block
    /// the evicted incarnation cached may have been rewritten in place by
    /// the holder that recovered its files — no recall reached this mount).
    pub fn purgeAll(self: *ReadCache) void {
        _ = self.core.evictUntil(0);
    }

    /// The live bound for the authority's budget `budget_now`: the
    /// percentage of `budget ÷ 0.7` (= the machine's RAM over the live
    /// share, or the cage's 0.8 scaled), capped by the reservation.
    pub fn rederive(self: *ReadCache, budget_now: u64) void {
        if (!self.enabled() or budget_now == 0) return;
        const ram_proxy = budget_now / cache.SOLO_BUDGET_TENTHS *| 10;
        self.core.setBound(@min(cache.derivedBound(self.pct, ram_proxy, budget_now), self.capacity_bytes));
    }

    pub fn boundBytes(self: *const ReadCache) u64 {
        return self.core.bound;
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
        slab: *GrainSlab,

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
        // The block's shard is its stripe: the slot a tail eviction frees
        // is the one the retry takes.
        const stripe = self.core.shardIndex(grainKey(block, 0));
        var inserted: usize = 0;
        for (0..n) |i| {
            const slot = self.slab.acquire(stripe) orelse blk: {
                // The slab bounds slots as the budget bounds bytes: an
                // exhausted stripe evicts its shard's tail and retries once.
                stats.read_cache_grain_slab_exhausted.inc();
                _ = self.core.evictOne(grainKey(block, 0));
                break :blk self.slab.acquire(stripe) orelse break;
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
    /// freed slots the kernel still backs), as the read LRU gauges. The
    /// sampler's tick is also where the bound follows the live budget.
    fn gaugeFn(ctx: *anyopaque) u64 {
        const self: *ReadCache = @ptrCast(@alignCast(ctx));
        self.rederive(budget.authority.budgetBytes());
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
        try w.print(",\"read_cache_grain_purges\":{d},\"read_cache_grain_binding_refusals\":{d},\"read_cache_grain_slab_exhausted\":{d},\"read_cache_grain_window_puts\":{d},\"read_cache_grain_trimmed_bytes\":{d},\"read_cache_grain_resident_bytes\":{d},\"read_cache_stale_served\":{d}}}", .{
            stats.read_cache_grain_purges.load(),
            stats.read_cache_grain_binding_refusals.load(),
            stats.read_cache_grain_slab_exhausted.load(),
            stats.read_cache_grain_window_puts.load(),
            stats.read_cache_grain_trimmed_bytes.load(),
            self.slab.residentBytes(),
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
    // A shard's slots: its grains + the evicting deposit + one window.
    try testing.expectEqual(@as(u32, 512 + 1 + 16), perShardSlots(4 * derive.MiB, 2));
    try testing.expectEqual(cache.Items{ .max = 512 + 1 + 16, .entry_floor = GRAIN, .slack = 17 }, coreItems(4 * derive.MiB, 2));
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

/// Pages `mincore` reports resident over `[ptr, ptr + len)` (Linux).
fn residentPages(ptr: *const anyopaque, len: usize) !usize {
    const page = std.heap.pageSize();
    const start = std.mem.alignBackward(usize, @intFromPtr(ptr), page);
    const span = std.mem.alignForward(usize, @intFromPtr(ptr) + len, page) - start;
    const vec = try testing.allocator.alloc(u8, span / page);
    defer testing.allocator.free(vec);
    try testing.expectEqual(std.os.linux.E.SUCCESS, std.os.linux.errno(std.os.linux.syscall3(.mincore, start, span, @intFromPtr(vec.ptr))));
    var n: usize = 0;
    for (vec) |v| n += v & 1;
    return n;
}

test "the idle-RSS law at the read cache: the reservation is 25 % of a 109 GiB machine (7.1 M grains over 32 shards), the bound a 64 MiB budget's 22 MiB — the index holds the BOUND's grains, and the slab, the slot tables and the item tables are resident only where a deposit touched them (dev: 1.39 GiB of an idle daemon, all written at birth)" {
    const bs: u64 = 1024 * GRAIN;
    const reservation: u64 = 109 * derive.GiB / 4;
    const bound: u64 = 22 * derive.MiB;
    const rc = try ReadCache.init(testing.allocator, bs, DEFAULT_PCT, bound, reservation, 32);
    defer rc.deinit();
    try testing.expectEqual(@as(usize, 32), rc.shardCount());
    const per_shard = perShardSlots(reservation, 32);
    try testing.expectEqual(@as(usize, per_shard), rc.core.items.max);
    for (0..32) |i| try testing.expectEqual(@as(usize, bound / 32 / GRAIN + 1 + WINDOW_MAX_GRAINS), rc.core.indexItems(i));
    try testing.expectEqual(@as(u32, 32 * per_shard), rc.slab.capacity);
    // Deposits into a few blocks: a handful of grains.
    var win: [4 * GRAIN]u8 = undefined;
    fillGrain(&win, 0x6b);
    for (0..8) |k| try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(BlockKey.init(4, k * bs), 0, &win, 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(@as(u64, 32 * GRAIN), rc.currentBytes());
    try testing.expectEqual(@as(u64, 32 * GRAIN), rc.slab.residentBytes());
    if (builtin.os.tag != .linux) return;
    // ≤ one page per stripe touched in each lazy table (8 blocks ⇒ ≤ 8
    // stripes), 32 grains of slab: never the reservation's 7.1 M slots.
    const slab = rc.slab;
    try testing.expect(try residentPages(slab.base, slab.bytes_len) <= 32);
    try testing.expect(try residentPages(slab.refs.ptr, slab.refs.len * 4) <= 8);
    try testing.expect(try residentPages(slab.freed_tick.ptr, slab.freed_tick.len * 8) <= 8);
    var items_pages: usize = 0;
    for (rc.core.shards) |*sh| items_pages += try residentPages(sh.items.ptr, sh.items.len * @sizeOf(@TypeOf(sh.items[0])));
    try testing.expect(items_pages <= 8 * 2);
    // A release goes warm (still resident), the idle trim takes it cold.
    rc.shedTo(0);
    try testing.expectEqual(@as(u64, 0), rc.currentBytes());
    try testing.expectEqual(@as(u64, 32 * GRAIN), rc.slab.residentBytes());
    const trimmed0 = stats.read_cache_grain_trimmed_bytes.load();
    rc.slab.trim(0);
    try testing.expectEqual(@as(u64, 0), rc.slab.residentBytes());
    try testing.expectEqual(trimmed0 + 32 * GRAIN, stats.read_cache_grain_trimmed_bytes.load());
    try testing.expectEqual(@as(usize, 0), try residentPages(slab.base, slab.bytes_len));
}

test "the grain slab: a stripe hands out never-used slots, then reuses its warm chain hottest first; the idle trim takes the oldest release and only past its idle age; a full stripe answers null while its neighbour still has room" {
    const slab = try GrainSlab.init(testing.allocator, 2, 3);
    defer slab.deinit();
    var clock: Atomic(u64) = .init(10);
    slab.setClock(&clock);
    const a = slab.acquire(0).?;
    const b = slab.acquire(0).?;
    const c = slab.acquire(0).?;
    try testing.expectEqual(@as(?GrainSlab.Slot, null), slab.acquire(0));
    try testing.expectEqual(@as(GrainSlab.Slot, 3), slab.acquire(1).?);
    try testing.expectEqual(@as(usize, 2), slab.freeSlots());
    @memset(slab.bytes(a), 1);
    @memset(slab.bytes(b), 2);
    // A pin outlives the tier's release.
    slab.pin(b);
    try testing.expect(slab.unpin(a));
    clock.store(11, .monotonic);
    try testing.expect(!slab.unpin(b));
    try testing.expect(slab.unpin(b));
    try testing.expectEqual(@as(u64, 2 * GRAIN), slab.warm_bytes.load());
    // The hottest release (b) is reused first.
    try testing.expectEqual(b, slab.acquire(0).?);
    try testing.expect(slab.unpin(b));
    // Trim at age 1 at tick 11: a (released at 10) goes, b (11) stays.
    slab.trim(1);
    try testing.expectEqual(@as(u64, GRAIN), slab.warm_bytes.load());
    try testing.expectEqual(b, slab.acquire(0).?);
    try testing.expectEqual(a, slab.acquire(0).?);
    try testing.expectEqual(@as(?GrainSlab.Slot, null), slab.acquire(0));
    for ([_]GrainSlab.Slot{ a, b, c, 3 }) |s| try testing.expect(slab.unpin(s));
    try testing.expectEqual(slab.capacity, @as(u32, @intCast(slab.freeSlots())));
}

fn fillGrain(buf: []u8, tag: u8) void {
    @memset(buf, tag);
}

test "the read cache: a deposited window serves grain-exact, partial and unaligned windows, touches move an entry to the head, the tail is the least recently used, an absent grain is a miss, 0 bytes is off" {
    const bs: u64 = 64 * GRAIN;
    // Budget 4 grains in ONE shard.
    const rc = try ReadCache.init(testing.allocator, bs, DEFAULT_PCT, 4 * GRAIN, 4 * GRAIN, 1);
    defer rc.deinit();
    try testing.expectEqual(@as(usize, 1), rc.shardCount());
    const blk = BlockKey.init(3, 2 * bs);
    var win: [2 * GRAIN]u8 = undefined;
    fillGrain(win[0..GRAIN], 0xA1);
    fillGrain(win[GRAIN..], 0xA2);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 8 * GRAIN, &win, 7, lease_epoch.load(7), .always, false));
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
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 20 * GRAIN, &g, 7, lease_epoch.load(7), .always, false));
    fillGrain(&g, 0xB2);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 21 * GRAIN, &g, 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(@as(u64, 4 * GRAIN), rc.currentBytes());
    // Touch grain 9 again so the order (MRU → LRU) is 9, 21, 20, 8.
    try testing.expectEqual(@as(?usize, GRAIN), rc.get(blk, 9 * GRAIN, 10 * GRAIN, out[0..GRAIN]));
    const ev0 = rc.core.stats.evictions.load();
    fillGrain(&g, 0xC1);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 30 * GRAIN, &g, 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(ev0 + 1, rc.core.stats.evictions.load());
    try testing.expect(!rc.contains(blk, 8 * GRAIN, 9 * GRAIN));
    try testing.expect(rc.contains(blk, 9 * GRAIN, 10 * GRAIN));
    try testing.expect(rc.contains(blk, 20 * GRAIN, 22 * GRAIN));
    try testing.expect(rc.contains(blk, 30 * GRAIN, 31 * GRAIN));
    // The next victim is 20 (LRU after 8), not 9 (touched) — exact recency.
    fillGrain(&g, 0xC2);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 31 * GRAIN, &g, 7, lease_epoch.load(7), .always, false));
    try testing.expect(!rc.contains(blk, 20 * GRAIN, 21 * GRAIN));
    try testing.expect(rc.contains(blk, 9 * GRAIN, 10 * GRAIN));
    // A replaced grain carries the new bytes.
    fillGrain(&g, 0xD9);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 9 * GRAIN, &g, 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(@as(?usize, GRAIN), rc.get(blk, 9 * GRAIN, 10 * GRAIN, out[0..GRAIN]));
    try testing.expectEqual(@as(u8, 0xD9), out[0]);
    try testing.expectEqual(@as(u64, 4 * GRAIN), rc.currentBytes());
    // Ineligible windows: unaligned, too wide, past the block.
    try testing.expectEqual(ReadCache.PutOutcome.ineligible, rc.putValidated(blk, 1, &g, 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(ReadCache.PutOutcome.ineligible, rc.putValidated(blk, bs - GRAIN, &win, 7, lease_epoch.load(7), .always, false));
    var wide: [WINDOW_MAX + GRAIN]u8 = undefined;
    try testing.expectEqual(ReadCache.PutOutcome.ineligible, rc.putValidated(blk, 0, &wide, 7, lease_epoch.load(7), .always, false));
    // Off.
    const off = try ReadCache.init(testing.allocator, bs, DEFAULT_PCT, 0, 0, 1);
    defer off.deinit();
    try testing.expect(!off.enabled());
    try testing.expectEqual(ReadCache.PutOutcome.refused, off.putValidated(blk, 0, &g, 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(@as(?usize, null), off.get(blk, 0, GRAIN, out[0..GRAIN]));
    try testing.expect(!off.purge(blk));
}

test "the read cache: the ranged purge drops exactly the written grains, the block purge every grain of the block and nothing of its neighbour, the census names blocks the selector picks, a refused validation deposits nothing, freeze is net-zero growth, shed evicts tails, the component gauges the slab and sheds through the core, the stats family names the core's and its own" {
    const bs: u64 = 64 * GRAIN;
    const rc = try ReadCache.init(testing.allocator, bs, DEFAULT_PCT, 8 * GRAIN, 8 * GRAIN, 1);
    defer rc.deinit();
    const a = BlockKey.init(1, 0);
    const b = BlockKey.init(1, bs);
    var win: [4 * GRAIN]u8 = undefined;
    fillGrain(&win, 0x11);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(a, 0, &win, 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(b, 4 * GRAIN, win[0 .. 2 * GRAIN], 8, lease_epoch.load(8), .always, false));
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
    try testing.expectEqual(ReadCache.PutOutcome.refused, rc.putValidated(a, 0, &win, 7, lease_epoch.load(7), .{ .ctx = null, .call = &Refuse.no }, false));
    try testing.expectEqual(free_before, rc.slab.freeSlots());
    try testing.expectEqual(@as(u64, 0), rc.currentBytes());
    // Freeze: 4 grains in, then a frozen 2-grain deposit evicts 2 first.
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(a, 0, &win, 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(b, 0, win[0 .. 2 * GRAIN], 8, lease_epoch.load(8), .always, true));
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
    const rc = try ReadCache.init(testing.allocator, bs, DEFAULT_PCT, 8 * GRAIN, 8 * GRAIN, 1);
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

test "the read cache: the bound follows the live budget — a mount-time third grows to the reservation when the share drops, a cage shrink evicts down; off stays off" {
    const bs: u64 = 64 * GRAIN;
    // Init at a third of RAM's share (the mount's census of 3), the
    // reservation the machine's quarter.
    const gib = derive.GiB;
    const rc = try ReadCache.init(testing.allocator, bs, 25, 16 * GRAIN, 48 * GRAIN, 1);
    defer rc.deinit();
    try testing.expectEqual(@as(u64, 16 * GRAIN), rc.boundBytes());
    // The live budget: 70 % of a machine whose quarter is 48 grains ⇒ the
    // reservation (within the derivation's integer rounding); a bigger
    // budget is capped there exactly.
    const b48: u64 = 48 * GRAIN * 4 * 7 / 10;
    rc.rederive(b48);
    try testing.expectEqual(@min(cache.derivedBound(25, b48 / 7 * 10, b48), 48 * GRAIN), rc.boundBytes());
    try testing.expect(rc.boundBytes() + GRAIN > 48 * GRAIN);
    rc.rederive(10 * gib);
    try testing.expectEqual(@as(u64, 48 * GRAIN), rc.boundBytes());
    var g: [GRAIN]u8 = undefined;
    fillGrain(&g, 0x5c);
    const blk = BlockKey.init(1, 0);
    for (0..20) |i| try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, @as(u64, i) * GRAIN, &g, 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(@as(u64, 20 * GRAIN), rc.currentBytes());
    // A cage shrinks the budget: 70 % of a machine whose quarter is 8
    // grains ⇒ the LRU tail evicts down to the new bound (7 whole grains
    // fit under the rounded bound; the oldest went first).
    const b8: u64 = 8 * GRAIN * 4 * 7 / 10;
    rc.rederive(b8);
    try testing.expectEqual(cache.derivedBound(25, b8 / 7 * 10, b8), rc.boundBytes());
    try testing.expect(rc.currentBytes() <= rc.boundBytes() and rc.currentBytes() >= 7 * GRAIN);
    try testing.expect(rc.contains(blk, 19 * GRAIN, 20 * GRAIN) and !rc.contains(blk, 0, GRAIN));
    const off = try ReadCache.init(testing.allocator, bs, 0, 0, 48 * GRAIN, 1);
    defer off.deinit();
    off.rederive(10 * gib);
    try testing.expect(!off.enabled());
}

test "the read cache: a served grain pinned across an eviction keeps its bytes until the copy is done (the slot is not reused while pinned)" {
    const bs: u64 = 16 * GRAIN;
    const rc = try ReadCache.init(testing.allocator, bs, DEFAULT_PCT, GRAIN, GRAIN, 1);
    defer rc.deinit();
    const a = BlockKey.init(9, 0);
    var g: [GRAIN]u8 = undefined;
    fillGrain(&g, 0x5A);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(a, 0, &g, 7, lease_epoch.load(7), .always, false));
    // Pin the entry's slot as a serve would, evict it by depositing another
    // grain, and check the pinned slot still reads the old bytes and the
    // new grain took a different slot.
    var pins: ReadCache.Pins = .{ .slab = rc.slab };
    try testing.expectEqual(@as(usize, 1), rc.core.lookupMany(&.{ReadCache.grainKey(a, 0)}, &pins, ReadCache.Pins.visit));
    const slot = pins.slots[0];
    fillGrain(&g, 0xC3);
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(a, GRAIN, &g, 7, lease_epoch.load(7), .always, false));
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
    const rc = try ReadCache.init(testing.allocator, bs, DEFAULT_PCT, 32 * MIN_SHARD_BYTES, 32 * MIN_SHARD_BYTES, 32);
    defer rc.deinit();
    try testing.expect(rc.shardCount() > 1);
    var win: [2 * GRAIN]u8 = undefined;
    fillGrain(&win, 0x77);
    var k: u64 = 0;
    while (k < 64) : (k += 1) {
        const blk = BlockKey.init(5, k * bs);
        try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 0, &win, 7, lease_epoch.load(7), .always, false));
        try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(blk, 10 * GRAIN, &win, 7, lease_epoch.load(7), .always, false));
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
        try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(victim_blk, off, &filler, 7, lease_epoch.load(7), .always, false));
    }
    const ev0 = rc.core.stats.evictions.load();
    try testing.expectEqual(ReadCache.PutOutcome.inserted, rc.putValidated(victim_blk, off, &filler, 7, lease_epoch.load(7), .always, false));
    try testing.expectEqual(ev0 + 1, rc.core.stats.evictions.load());
    try testing.expectEqual(others_before, rc.currentBytes() - vs.bytes.load(.monotonic));
    // The evicted grain was this shard's oldest deposit: grain 0 of block 0.
    try testing.expect(!rc.contains(victim_blk, 0, GRAIN));
    try testing.expect(rc.contains(victim_blk, GRAIN, 2 * GRAIN));
}

test "the read cache: a block's purge (by key and through the census) leaves none of its grains while depositors on other blocks double the shards' indexes; at rest every live grain holds exactly one slab reference (the stripes and the index agree) and a full purge returns every slot" {
    const bs: u64 = 64 * GRAIN;
    const reservation: u64 = 8192 * GRAIN;
    const ino: u64 = 0x5ead_c0de_0001;
    const purge_vol: u64 = 9;
    const per_depositor: u64 = 32;
    const Race = struct {
        rc: *ReadCache,
        go: Atomic(bool) = .init(false),
        done: Atomic(u32) = .init(0),
        violations: Atomic(u32) = .init(0),
        rounds_in_growth: Atomic(u64) = .init(0),

        fn fail(r: *@This()) void {
            _ = r.violations.fetchAdd(1, .monotonic);
        }

        fn depositor(r: *@This(), vol: u64) void {
            var win: [WINDOW_MAX]u8 = undefined;
            fillGrain(&win, @truncate(vol));
            while (!r.go.load(.acquire)) std.atomic.spinLoopHint();
            for (0..per_depositor) |k| {
                const blk = BlockKey.init(vol, k * bs);
                if (r.rc.putValidated(blk, 0, &win, ino, lease_epoch.load(ino), .always, false) != .inserted) r.fail();
                if (r.rc.putValidated(blk, 32 * GRAIN, &win, ino, lease_epoch.load(ino), .always, false) != .inserted) r.fail();
            }
            _ = r.done.fetchAdd(1, .release);
        }

        fn mine(_: *anyopaque, key: BlockKey) bool {
            return key.vol_tag == purge_vol;
        }

        fn purger(r: *@This()) void {
            var win: [WINDOW_MAX]u8 = undefined;
            fillGrain(&win, 0x99);
            r.go.store(true, .release);
            var round: u64 = 0;
            while (round == 0 or r.done.load(.acquire) < 2) : (round += 1) {
                if (r.done.load(.acquire) < 2) _ = r.rounds_in_growth.fetchAdd(1, .monotonic);
                for (0..4) |k| {
                    const blk = BlockKey.init(purge_vol, (round * 4 + k) * bs);
                    if (r.rc.putValidated(blk, 16 * GRAIN, &win, ino, lease_epoch.load(ino), .always, false) != .inserted) r.fail();
                    if (!r.rc.contains(blk, 16 * GRAIN, 32 * GRAIN)) r.fail();
                }
                if (round % 2 == 0) {
                    for (0..4) |k| if (!r.rc.purge(BlockKey.init(purge_vol, (round * 4 + k) * bs))) r.fail();
                } else {
                    // The recall's census: one grain names its block, the
                    // block's purge drops the rest.
                    var cursor: u64 = 0;
                    var n: u32 = 0;
                    while (r.rc.nextKey(&cursor, .{ .ctx = r, .call = &mine })) |b| {
                        if (!r.rc.purge(b)) r.fail();
                        n += 1;
                    }
                    if (n != 4) r.fail();
                }
                for (0..4) |k| {
                    const blk = BlockKey.init(purge_vol, (round * 4 + k) * bs);
                    var off: u64 = 0;
                    while (off < bs) : (off += GRAIN) if (r.rc.core.contains(ReadCache.grainKey(blk, off))) r.fail();
                }
            }
        }
    };
    for (0..10) |_| {
        // A bound of 64 grains seeds each of the 16 shards' indexes at
        // 4 + 17 entries; the reservation's bound lets them double on
        // demand.
        const rc = try ReadCache.init(testing.allocator, bs, DEFAULT_PCT, 64 * GRAIN, reservation, 2);
        defer rc.deinit();
        try testing.expectEqual(@as(usize, 16), rc.shardCount());
        try testing.expectEqual(@as(usize, 4 + 17), rc.core.indexItems(0));
        rc.core.setBound(reservation);
        var r: Race = .{ .rc = rc };
        const d0 = try std.Thread.spawn(.{}, Race.depositor, .{ &r, 1 });
        const d1 = try std.Thread.spawn(.{}, Race.depositor, .{ &r, 2 });
        r.purger();
        d0.join();
        d1.join();
        try testing.expectEqual(@as(u32, 0), r.violations.load(.monotonic));
        try testing.expect(rc.core.stats.index_growths.load() > 0);
        try testing.expect(r.rounds_in_growth.load(.monotonic) > 0);
        // Every depositor grain survived (the bound evicts nothing here).
        for ([_]u64{ 1, 2 }) |vol| for (0..per_depositor) |k| {
            const blk = BlockKey.init(vol, k * bs);
            try testing.expect(rc.contains(blk, 0, 16 * GRAIN) and rc.contains(blk, 32 * GRAIN, 48 * GRAIN));
        };
        try testing.expectEqual(@as(u64, 2 * per_depositor * 32 * GRAIN), rc.currentBytes());
        try testing.expectEqual(rc.currentBytes(), rc.slab.checkedOutBytes());
        for ([_]u64{ 1, 2 }) |vol| for (0..per_depositor) |k| try testing.expect(rc.purge(BlockKey.init(vol, k * bs)));
        try testing.expectEqual(@as(u64, 0), rc.currentBytes());
        try testing.expectEqual(@as(usize, rc.slab.capacity), rc.slab.freeSlots());
    }
}
