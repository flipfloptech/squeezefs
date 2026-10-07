//! The hot-block RAM tier — the sharded CLOCK (charter §6.3 "hot-block RAM
//! tier (sharded clock, consumable `referenced` + sticky `protected`,
//! refcount puts)"; `docs/porting/data_read_path.md` §2.1, §2.5, §13 Q3,
//! Q12). PORTABLE.
//!
//! `ClockCache(Store)` is the ONE clock core: a block-key-addressed index
//! per shard, a circular clock ring of entry references, two bits per entry
//! and a byte budget. The read LRU (`read_lru.zig`) instantiates it over its
//! small-slot slab, the hot tier below over the fill pool. `Store` is the
//! slot owner (`pin(slot)` / `release(slot)`), called UNDER the shard lock so
//! a reader's pin can never race the eviction that frees the slot.
//!
//! The two bits (`:35–45`): `referenced` is the CONSUMABLE second chance —
//! the clock scan clears it and re-queues the entry; `protected` is the
//! STICKY keep-worthiness — set by a protected insert or a promoting `get`,
//! never cleared by the clock, read only to CLASSIFY a victim (every victim
//! has `referenced == false` by construction, so one bit could never tell
//! "spare it one more lap" from "was it ever worth keeping"). Both are
//! Relaxed single-word atomics with no cross-word invariant: a lost update
//! is one lap early or one mis-classified victim, never a wrong byte.
//!
//! Lock discipline: a per-shard spin lock guards the index and the ring for
//! an O(1) probe (the reference's scc reader lock measured 20 % of lane
//! cycles in its WRITER-lock path; the probe here is tens of ns); the
//! eviction LATCH is a try-only atomic flag — a put that finds the evictor
//! busy returns and the evictor converges; `shedTo` ignores second chances
//! ("the clamp is an order, not a scan"). Nothing allocates per op: the
//! index, the entry table and the ring are sized once from the budget.
//!
//! Shard count: `max(next_pow2(cores), 16)` HALVED while `budget / shards <
//! 16 MiB` (never below 1) — core-count sharding gave the default 128 MiB
//! budget ONE 4 MiB block per shard, so hash-colliding keys could never
//! coexist: 6.1 k whole-block refetches / 20 s on a FITTING 64 MiB set.
//! Budget default `max(2 × block, read_mem / 4)`: 2 blocks = one stream's
//! consume-behind window; 0 short-circuits the tier.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const budget = @import("../mem/budget.zig");
const jc = @import("../meta/journal_core.zig");
const rsc = @import("read_size_class.zig");
const fill_pool = @import("read_fill_pool.zig");

pub const BlockKey = rsc.BlockKey;

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// The smallest shard: ≥ 4 default blocks so colliding keys coexist
/// (`cache/mod.rs:166–175`).
pub const MIN_SHARD_BYTES: u64 = 16 * derive.MiB;
/// `max(next_pow2(cores), 16)`: a power-of-two mask; 16 the shipped
/// small-box posture (`lru.rs:10–16`).
pub const MIN_SHARDS: u32 = 16;
/// Ring reclaim runs when `ring_len > 2 × live + SLACK`: the amortization
/// floor below the `2 × live` term (`memory.rs:411–414`).
pub const RING_RECLAIM_SLACK: usize = 64;
/// `tryEvict` scans at most `max(2 × ring_len, 512)` nodes: two laps clear
/// every second chance; 512 the floor for a tiny ring.
pub const EVICT_LOOP_FLOOR: usize = 512;
/// `shedTo` scans at most `max(2 × ring_len, 64)`.
pub const SHED_LOOP_FLOOR: usize = 64;
/// The R5 component (`docs/porting/data_read_path.md` §2.5): floor 64 MiB
/// (16 default blocks — the smallest set the clock can hold without
/// perpetual churn), weight 4 (the largest sheddable class).
pub const COMPONENT_FLOOR: u64 = 64 * derive.MiB;
pub const COMPONENT_WEIGHT: u64 = 4;

pub const stats = struct {
    pub var hot_block_hits: counters.ShardedCounter = .{};
    pub var hot_block_misses: counters.ShardedCounter = .{};
    pub var hot_block_evictions: counters.ShardedCounter = .{};
    pub var hot_block_probation_drops: counters.ShardedCounter = .{};
    pub var hot_block_dehydrate_skips: counters.ShardedCounter = .{};
    pub var hot_block_current_bytes: counters.Gauge = .{};
    pub var hot_block_shards: counters.Gauge = .{};
    /// Validated puts refused at the insert (the entry was removed).
    pub var hot_block_put_refusals: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"hot_block_hits\":{d},\"hot_block_misses\":{d},\"hot_block_evictions\":{d},\"hot_block_probation_drops\":{d},\"hot_block_dehydrate_skips\":{d},\"hot_block_current_bytes\":{d},\"hot_block_shards\":{d},\"hot_block_put_refusals\":{d}}}", .{
        stats.hot_block_hits.load(),
        stats.hot_block_misses.load(),
        stats.hot_block_evictions.load(),
        stats.hot_block_probation_drops.load(),
        stats.hot_block_dehydrate_skips.load(),
        stats.hot_block_current_bytes.load(),
        stats.hot_block_shards.load(),
        stats.hot_block_put_refusals.load(),
    });
}

// ---------------------------------------------------------------------------
// Derivations.
// ---------------------------------------------------------------------------

/// `max(next_pow2(cores), 16)`.
pub fn shardCountFrom(cores: u32) u32 {
    return @intCast(@max(derive.nextPow2(cores), MIN_SHARDS));
}

/// Halve while `budget / shards < min_shard`, never below 1.
pub fn shardsForBudget(cores: u32, budget_bytes: u64, min_shard: u64) u32 {
    var n = shardCountFrom(cores);
    while (n > 1 and budget_bytes / n < min_shard) n /= 2;
    return n;
}

/// `max(2 × block, read_mem / 4)`.
pub fn derivedHotBudget(block_size: u64, read_mem_limit: u64) u64 {
    return @max(2 * block_size, read_mem_limit / 4);
}

/// `--read-mem-cache-size` else RAM / 10.
pub fn derivedReadMemLimit(facts: derive.Facts) u64 {
    return facts.total_ram / 10;
}

/// Explicit (MiB, verbatim; 0 = tier off) > derived.
pub fn resolveHotBudget(explicit_mib: ?u64, block_size: u64, read_mem_limit: u64) u64 {
    if (explicit_mib) |m| return m * derive.MiB;
    return derivedHotBudget(block_size, read_mem_limit);
}

pub fn resolveReadMemLimit(explicit_mib: ?u64, facts: derive.Facts) u64 {
    if (explicit_mib) |m| return m * derive.MiB;
    return derivedReadMemLimit(facts);
}

pub fn hotBudgetFromEnv(env: *const std.process.Environ.Map, block_size: u64, facts: derive.Facts) u64 {
    const read_mem = resolveReadMemLimit(knobs.optIntKnob("SQUEEZEFS_READ_MEM_CACHE_MB", u64, env), facts);
    return resolveHotBudget(knobs.optIntKnob("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", u64, env), block_size, read_mem);
}

// ---------------------------------------------------------------------------
// The clock core.
// ---------------------------------------------------------------------------

/// The put class: `(protected, referenced, stream)` per `lru.rs:321–326`.
pub const PutClass = enum(u8) {
    /// `(0, 0, 0)`: first touch, demand.
    probation,
    /// `(0, 1, 0)`: the one-lap grace for a stream's first touch.
    probation_referenced,
    /// `(1, 1, 0)`: ghost second touch.
    protected,
    /// `(1, 1, 1)`: a governor-granted stream admission; payback-exempt.
    protected_stream,

    pub fn bits(self: PutClass) struct { protected: bool, referenced: bool, stream: bool } {
        return switch (self) {
            .probation => .{ .protected = false, .referenced = false, .stream = false },
            .probation_referenced => .{ .protected = false, .referenced = true, .stream = false },
            .protected => .{ .protected = true, .referenced = true, .stream = false },
            .protected_stream => .{ .protected = true, .referenced = true, .stream = true },
        };
    }
};

pub const EvictClass = enum(u8) { probation, protected };

/// A victim handed to the owner: the slot's reference is TRANSFERRED (the
/// owner reports it and releases it).
pub const Victim = struct {
    key: BlockKey,
    slot: u32,
    len: u32,
    class: EvictClass,
    served_bytes: u64,
    stream_admitted: bool,
};

/// The insert-time validation (finding 17: validated AT the visibility
/// point, under the shard lock). `null` = always valid.
pub const Validate = struct {
    ctx: ?*anyopaque = null,
    call: ?*const fn (ctx: ?*anyopaque) bool = null,

    pub const always: Validate = .{};

    pub fn run(self: Validate) bool {
        const f = self.call orelse return true;
        return f(self.ctx);
    }
};

pub const PutOutcome = enum {
    inserted,
    replaced,
    /// The validation refused: nothing inserted (an occupied entry was
    /// removed — absence is correctness-safe); the caller owns the slot.
    refused,
    /// Larger than the shard budget: never cached; a stale smaller entry
    /// under the key was removed.
    too_large,
};

pub const Hit = struct {
    slot: u32,
    len: u32,
};

pub fn ClockCache(comptime Store: type) type {
    return struct {
        const Self = @This();

        const Entry = struct {
            key: BlockKey = undefined,
            slot: u32 = 0,
            len: u32 = 0,
            /// ABA for ring references.
            gen: u32 = 0,
            live: bool = false,
            stream_admitted: bool = false,
            referenced: Atomic(bool) = .init(false),
            protected: Atomic(bool) = .init(false),
            served_bytes: Atomic(u64) = .init(0),
        };

        const Shard = struct {
            lock: jc.SpinLock = .{},
            map: rsc.KeyMap(u32) = .empty,
            entries: []Entry,
            free: []u32,
            free_len: usize,
            ring: []u64,
            ring_head: usize = 0,
            ring_len: usize = 0,
            current_bytes: Atomic(u64) align(std.atomic.cache_line) = .init(0),
            live: Atomic(usize) = .init(0),
            evict_latch: Atomic(bool) = .init(false),
            max_bytes: u64,

            fn ringRef(idx: u32, gen: u32) u64 {
                return (@as(u64, idx) << 32) | gen;
            }

            fn ringPush(self: *Shard, idx: u32, gen: u32) void {
                assert(self.ring_len < self.ring.len);
                self.ring[(self.ring_head + self.ring_len) % self.ring.len] = ringRef(idx, gen);
                self.ring_len += 1;
            }

            fn ringPop(self: *Shard) ?u64 {
                if (self.ring_len == 0) return null;
                const r = self.ring[self.ring_head];
                self.ring_head = (self.ring_head + 1) % self.ring.len;
                self.ring_len -= 1;
                return r;
            }

            /// The entry a ring reference names, if still that entry.
            fn resolve(self: *Shard, r: u64) ?*Entry {
                const idx: u32 = @intCast(r >> 32);
                const gen: u32 = @truncate(r);
                const e = &self.entries[idx];
                if (!e.live or e.gen != gen) return null;
                return e;
            }

            fn allocEntry(self: *Shard) u32 {
                assert(self.free_len > 0);
                self.free_len -= 1;
                return self.free[self.free_len];
            }

            fn freeEntry(self: *Shard, idx: u32) void {
                const e = &self.entries[idx];
                e.live = false;
                e.gen +%= 1;
                self.free[self.free_len] = idx;
                self.free_len += 1;
            }

            /// Unindex + unaccount; the slot reference transfers to the caller.
            fn removeLocked(self: *Shard, idx: u32) Victim {
                const e = &self.entries[idx];
                const v: Victim = .{
                    .key = e.key,
                    .slot = e.slot,
                    .len = e.len,
                    .class = if (e.protected.load(.monotonic)) .protected else .probation,
                    .served_bytes = e.served_bytes.load(.monotonic),
                    .stream_admitted = e.stream_admitted,
                };
                assert(self.map.remove(e.key));
                _ = self.current_bytes.fetchSub(e.len, .monotonic);
                _ = self.live.fetchSub(1, .monotonic);
                self.freeEntry(idx);
                return v;
            }

            /// Drain-and-refill when tombstones outnumber the law's bound.
            fn reclaimRingLocked(self: *Shard) void {
                const live = self.live.load(.monotonic);
                if (self.ring_len <= 2 * live + RING_RECLAIM_SLACK) return;
                var kept: usize = 0;
                var i: usize = 0;
                const n = self.ring_len;
                while (i < n) : (i += 1) {
                    const r = self.ring[(self.ring_head + i) % self.ring.len];
                    if (self.resolve(r) != null) {
                        self.ring[(self.ring_head + kept) % self.ring.len] = r;
                        kept += 1;
                    }
                }
                self.ring_len = kept;
            }
        };

        gpa: Allocator,
        store: *Store,
        shards: []Shard,
        mask: u64,
        max_entries: usize,

        /// `budget_bytes` split evenly over `shard_count` (a power of two);
        /// `max_entries` per shard bounds the index, the entry table and
        /// the ring (`2 × max_entries + 2 × SLACK`: the reclaim law keeps
        /// `ring_len ≤ 2 × live + SLACK`).
        pub fn init(gpa: Allocator, store: *Store, budget_bytes: u64, shard_count: u32, max_entries: usize) Allocator.Error!Self {
            assert(shard_count > 0 and std.math.isPowerOfTwo(shard_count) and max_entries > 0);
            const shards = try gpa.alloc(Shard, shard_count);
            errdefer gpa.free(shards);
            var built: usize = 0;
            errdefer for (shards[0..built]) |*s| destroyShard(gpa, s);
            for (shards) |*s| {
                const entries = try gpa.alloc(Entry, max_entries);
                errdefer gpa.free(entries);
                @memset(entries, .{});
                const free = try gpa.alloc(u32, max_entries);
                errdefer gpa.free(free);
                var i: u32 = 0;
                while (i < max_entries) : (i += 1) free[i] = @intCast(max_entries - 1 - i);
                const ring = try gpa.alloc(u64, 2 * max_entries + 2 * RING_RECLAIM_SLACK);
                errdefer gpa.free(ring);
                s.* = .{ .entries = entries, .free = free, .free_len = max_entries, .ring = ring, .max_bytes = budget_bytes / shard_count };
                try s.map.ensureTotalCapacity(gpa, @intCast(max_entries));
                built += 1;
            }
            return .{ .gpa = gpa, .store = store, .shards = shards, .mask = shard_count - 1, .max_entries = max_entries };
        }

        fn destroyShard(gpa: Allocator, s: *Shard) void {
            s.map.deinit(gpa);
            gpa.free(s.entries);
            gpa.free(s.free);
            gpa.free(s.ring);
        }

        /// Releases every entry's slot reference through the store.
        pub fn deinit(self: *Self) void {
            for (self.shards) |*s| {
                for (s.entries) |*e| if (e.live) self.store.release(e.slot);
                destroyShard(self.gpa, s);
            }
            self.gpa.free(self.shards);
            self.* = undefined;
        }

        fn shardOf(self: *Self, key: BlockKey) *Shard {
            return &self.shards[@intCast(key.hash() & self.mask)];
        }

        pub fn shardCount(self: *const Self) u32 {
            return @intCast(self.shards.len);
        }

        pub fn shardBudget(self: *const Self) u64 {
            return self.shards[0].max_bytes;
        }

        pub fn currentBytes(self: *const Self) u64 {
            var n: u64 = 0;
            for (self.shards) |*s| n += s.current_bytes.load(.monotonic);
            return n;
        }

        pub fn liveEntries(self: *const Self) usize {
            var n: usize = 0;
            for (self.shards) |*s| n += s.live.load(.monotonic);
            return n;
        }

        /// `get`: `referenced = true`; `served > 0 ∧ !stream_admitted ⇒
        /// served_bytes += served`; `promote ⇒ protected = true`; the slot
        /// pinned under the lock. Null on a miss.
        pub fn get(self: *Self, key: BlockKey, promote: bool, served: u64) ?Hit {
            const s = self.shardOf(key);
            s.lock.lock();
            defer s.lock.unlock();
            const idx = s.map.get(key) orelse return null;
            const e = &s.entries[idx];
            e.referenced.store(true, .monotonic);
            if (served > 0 and !e.stream_admitted) _ = e.served_bytes.fetchAdd(served, .monotonic);
            if (promote) e.protected.store(true, .monotonic);
            self.store.pin(e.slot);
            return .{ .slot = e.slot, .len = e.len };
        }

        /// The residency probe: no pin, no promotion, `referenced` set.
        pub fn touch(self: *Self, key: BlockKey) bool {
            const s = self.shardOf(key);
            s.lock.lock();
            defer s.lock.unlock();
            const idx = s.map.get(key) orelse return false;
            s.entries[idx].referenced.store(true, .monotonic);
            return true;
        }

        pub fn contains(self: *Self, key: BlockKey) bool {
            const s = self.shardOf(key);
            s.lock.lock();
            defer s.lock.unlock();
            return s.map.contains(key);
        }

        /// The entry's bits (test/diagnostic observable).
        pub fn bitsOf(self: *Self, key: BlockKey) ?struct { referenced: bool, protected: bool, stream: bool, served: u64 } {
            const s = self.shardOf(key);
            s.lock.lock();
            defer s.lock.unlock();
            const idx = s.map.get(key) orelse return null;
            const e = &s.entries[idx];
            return .{ .referenced = e.referenced.load(.monotonic), .protected = e.protected.load(.monotonic), .stream = e.stream_admitted, .served = e.served_bytes.load(.monotonic) };
        }

        /// `put`: the slot's reference is ADOPTED on `.inserted`/`.replaced`
        /// (the caller keeps none) and stays the caller's on `.refused`/
        /// `.too_large`. Validation runs under the lock at the visibility
        /// point; a refusal on an occupied key REMOVES the entry (`removed`
        /// receives its victim so the caller can release that slot).
        pub fn put(self: *Self, key: BlockKey, slot: u32, len: u32, class: PutClass, validate: Validate, removed: *?Victim) PutOutcome {
            removed.* = null;
            const s = self.shardOf(key);
            if (len > s.max_bytes) {
                s.lock.lock();
                if (s.map.get(key)) |idx| removed.* = s.removeLocked(idx);
                s.lock.unlock();
                return .too_large;
            }
            const b = class.bits();
            const outcome: PutOutcome = blk: {
                s.lock.lock();
                defer s.lock.unlock();
                if (s.map.get(key)) |idx| {
                    if (!validate.run()) {
                        removed.* = s.removeLocked(idx);
                        break :blk .refused;
                    }
                    const e = &s.entries[idx];
                    // Replace the value: the old slot goes back to the caller
                    // as a victim-shaped record (its bits are irrelevant).
                    removed.* = .{ .key = e.key, .slot = e.slot, .len = e.len, .class = .probation, .served_bytes = 0, .stream_admitted = false };
                    _ = s.current_bytes.fetchSub(e.len, .monotonic);
                    e.slot = slot;
                    e.len = len;
                    _ = s.current_bytes.fetchAdd(len, .monotonic);
                    e.referenced.store(true, .monotonic);
                    // A probation re-put never demotes (sticky).
                    if (b.protected) {
                        e.protected.store(true, .monotonic);
                        e.stream_admitted = b.stream;
                    }
                    break :blk .replaced;
                }
                if (!validate.run()) break :blk .refused;
                if (s.free_len == 0) {
                    // The table is full of live entries (the budget admits
                    // more entries than the table holds only when every
                    // entry is far below the block size): evict one first.
                    if (self.evictOneLocked(s, false)) |v| {
                        self.report(v);
                    } else break :blk .refused;
                }
                const idx = s.allocEntry();
                const e = &s.entries[idx];
                e.* = .{ .key = key, .slot = slot, .len = len, .gen = e.gen, .live = true, .stream_admitted = b.stream, .referenced = .init(b.referenced), .protected = .init(b.protected), .served_bytes = .init(0) };
                s.map.putAssumeCapacity(key, idx);
                s.ringPush(idx, e.gen);
                _ = s.current_bytes.fetchAdd(len, .monotonic);
                _ = s.live.fetchAdd(1, .monotonic);
                break :blk .inserted;
            };
            if (outcome == .inserted or outcome == .replaced) self.evictToBudget(s);
            return outcome;
        }

        /// One clock step under the lock: pop; dead ⇒ skip; referenced ⇒
        /// clear + re-push (unless `ignore_referenced`); else the victim.
        fn evictOneLocked(self: *Self, s: *Shard, ignore_referenced: bool) ?Victim {
            _ = self;
            const floor: usize = if (ignore_referenced) SHED_LOOP_FLOOR else EVICT_LOOP_FLOOR;
            var loops: usize = @max(2 * s.ring_len, floor);
            while (loops > 0) : (loops -= 1) {
                const r = s.ringPop() orelse return null;
                const e = s.resolve(r) orelse continue;
                if (!ignore_referenced and e.referenced.swap(false, .monotonic)) {
                    s.ringPush(@intCast(r >> 32), e.gen);
                    continue;
                }
                return s.removeLocked(@intCast(r >> 32));
            }
            return null;
        }

        /// Under the try-latch: evict until the shard is within budget.
        fn evictToBudget(self: *Self, s: *Shard) void {
            if (s.evict_latch.cmpxchgStrong(false, true, .acquire, .monotonic) != null) return;
            defer s.evict_latch.store(false, .release);
            while (s.current_bytes.load(.monotonic) > s.max_bytes) {
                s.lock.lock();
                const v = self.evictOneLocked(s, false);
                s.lock.unlock();
                const victim = v orelse return;
                self.report(victim);
            }
        }

        /// The R5 Red clamp: evict (second chances IGNORED) until the total
        /// is ≤ `target`, round-robin over the shards; returns victims to
        /// the owner through `onEvict`. Latch-contended shards are skipped.
        pub fn shedTo(self: *Self, target: u64) void {
            var progress = true;
            while (self.currentBytes() > target and progress) {
                progress = false;
                for (self.shards) |*s| {
                    if (self.currentBytes() <= target) return;
                    if (s.evict_latch.cmpxchgStrong(false, true, .acquire, .monotonic) != null) continue;
                    defer s.evict_latch.store(false, .release);
                    s.lock.lock();
                    const v = self.evictOneLocked(s, true);
                    s.lock.unlock();
                    if (v) |victim| {
                        self.report(victim);
                        progress = true;
                    }
                }
            }
        }

        /// Evict ONE entry by the clock regardless of the byte budget,
        /// starting at `key`'s shard then round-robin (the slot-store
        /// exhaustion trigger: a store of fixed slots bounds entries as
        /// well as bytes). False when nothing could be evicted.
        pub fn evictAny(self: *Self, key: BlockKey) bool {
            const start: usize = @intCast(key.hash() & self.mask);
            var i: usize = 0;
            while (i < self.shards.len) : (i += 1) {
                const s = &self.shards[(start + i) % self.shards.len];
                if (s.evict_latch.cmpxchgStrong(false, true, .acquire, .monotonic) != null) continue;
                defer s.evict_latch.store(false, .release);
                s.lock.lock();
                const v = self.evictOneLocked(s, false);
                s.lock.unlock();
                if (v) |victim| {
                    self.report(victim);
                    return true;
                }
            }
            return false;
        }

        /// The purge arm: the entry is removed and its slot reference
        /// returned to the caller (null when absent — one probe, no alloc).
        pub fn remove(self: *Self, key: BlockKey) ?Victim {
            const s = self.shardOf(key);
            s.lock.lock();
            defer s.lock.unlock();
            const idx = s.map.get(key) orelse return null;
            const v = s.removeLocked(idx);
            s.reclaimRingLocked();
            return v;
        }

        /// Ring population of `key`'s shard (the reclaim law's observable).
        pub fn ringLenOf(self: *Self, key: BlockKey) usize {
            const s = self.shardOf(key);
            s.lock.lock();
            defer s.lock.unlock();
            return s.ring_len;
        }

        fn report(self: *Self, v: Victim) void {
            self.store.onEvict(v);
        }
    };
}

// ---------------------------------------------------------------------------
// The hot tier over the fill pool.
// ---------------------------------------------------------------------------

/// Where a victim goes after the clock: the owner's dehydration arm
/// (admission's governor ledger + the NVMe publish of protected victims).
pub const EvictSink = struct {
    ctx: ?*anyopaque = null,
    on_evict: ?*const fn (ctx: ?*anyopaque, v: Victim) void = null,

    pub const none: EvictSink = .{};
};

pub const HotTier = struct {
    pool: *fill_pool.Pool,
    cache: ClockCache(HotTier),
    sink: EvictSink = .none,
    block_size: u64,
    budget_bytes: u64,

    /// `budget_bytes == 0` disables the tier (every `get` misses, every
    /// `put` refuses) — the caller degrades admission to `always`.
    pub fn init(gpa: Allocator, pool: *fill_pool.Pool, block_size: u64, budget_bytes: u64, cores: u32) Allocator.Error!*HotTier {
        const self = try gpa.create(HotTier);
        errdefer gpa.destroy(self);
        const shards = shardsForBudget(cores, budget_bytes, MIN_SHARD_BYTES);
        // Production floors the total at `shards × block` so every shard
        // holds ≥ 1 block; entries per shard = that many + 1 for the put
        // that triggers the eviction.
        const per_shard_blocks: usize = @intCast(@max(budget_bytes / shards / block_size, 1) + 1);
        self.* = .{ .pool = pool, .cache = undefined, .block_size = block_size, .budget_bytes = budget_bytes };
        self.cache = try ClockCache(HotTier).init(gpa, self, budget_bytes, shards, per_shard_blocks);
        stats.hot_block_shards.set(shards);
        return self;
    }

    pub fn deinit(self: *HotTier) void {
        const gpa = self.cache.gpa;
        self.cache.deinit();
        gpa.destroy(self);
    }

    pub fn enabled(self: *const HotTier) bool {
        return self.budget_bytes != 0;
    }

    // ---- the Store face the clock calls under its lock ----

    pub fn pin(self: *HotTier, slot: u32) void {
        self.pool.pin(slot);
    }

    pub fn release(self: *HotTier, slot: u32) void {
        _ = self.pool.unpin(slot);
    }

    /// Every victim: counted, classified, handed to the sink (which owns
    /// the publish decision), then its slot reference released.
    pub fn onEvict(self: *HotTier, v: Victim) void {
        stats.hot_block_evictions.inc();
        if (v.class == .probation) stats.hot_block_probation_drops.inc();
        if (self.sink.on_evict) |f| f(self.sink.ctx, v);
        _ = self.pool.unpin(v.slot);
        stats.hot_block_current_bytes.set(self.cache.currentBytes());
    }

    // ---- the tier face ----

    /// A serving hit (promotes; credits `served`); the caller `unpin`s.
    pub fn getServing(self: *HotTier, key: BlockKey, served: u64) ?Hit {
        if (!self.enabled()) return null;
        return self.cache.get(key, true, served);
    }

    /// Stream sub-read consumption is not keep-worthiness.
    pub fn getNoPromote(self: *HotTier, key: BlockKey) ?Hit {
        if (!self.enabled()) return null;
        return self.cache.get(key, false, 0);
    }

    pub fn touchNoPromote(self: *HotTier, key: BlockKey) bool {
        if (!self.enabled()) return false;
        return self.cache.touch(key);
    }

    pub fn contains(self: *HotTier, key: BlockKey) bool {
        if (!self.enabled()) return false;
        return self.cache.contains(key);
    }

    /// Drop a serve pin.
    pub fn unpin(self: *HotTier, hit: Hit) void {
        _ = self.pool.unpin(hit.slot);
    }

    /// The validated put: the pool reference the caller holds on `slot`
    /// is adopted (one more pin is taken for the tier so the caller's own
    /// reference stays the caller's — the caller releases its fill pin as
    /// usual). Yellow+ callers pass `freeze` so the insert evicts first
    /// (net-zero growth).
    pub fn putClassValidated(self: *HotTier, key: BlockKey, slot: u32, len: u32, class: PutClass, validate: Validate, freeze: bool) PutOutcome {
        if (!self.enabled()) return .refused;
        const freeze_at = if (freeze) self.cache.currentBytes() else 0;
        self.pool.pin(slot);
        var removed: ?Victim = null;
        const out = self.cache.put(key, slot, len, class, validate, &removed);
        if (removed) |r| _ = self.pool.unpin(r.slot);
        switch (out) {
            .inserted, .replaced => {
                if (freeze) self.cache.shedTo(freeze_at);
            },
            .refused => {
                stats.hot_block_put_refusals.inc();
                _ = self.pool.unpin(slot);
            },
            .too_large => _ = self.pool.unpin(slot),
        }
        stats.hot_block_current_bytes.set(self.cache.currentBytes());
        return out;
    }

    /// The purge arm (`purge.zig`): true when an entry was dropped.
    pub fn purge(self: *HotTier, key: BlockKey) bool {
        const v = self.cache.remove(key) orelse return false;
        _ = self.pool.unpin(v.slot);
        stats.hot_block_current_bytes.set(self.cache.currentBytes());
        return true;
    }

    pub fn shedTo(self: *HotTier, target: u64) void {
        self.cache.shedTo(target);
    }

    pub fn currentBytes(self: *const HotTier) u64 {
        return self.cache.currentBytes();
    }

    fn gaugeFn(ctx: *anyopaque) u64 {
        const self: *HotTier = @ptrCast(@alignCast(ctx));
        return self.currentBytes();
    }

    fn shedFn(ctx: *anyopaque, target: u64) void {
        const self: *HotTier = @ptrCast(@alignCast(ctx));
        self.shedTo(target);
    }

    /// `hot_block_tier`: floor 64 MiB, weight 4, shed = `shedTo`.
    pub fn component(self: *HotTier) budget.Component {
        return budget.Component.init("hot_block_tier", COMPONENT_FLOOR, COMPONENT_WEIGHT, .{ .func = .{ .ctx = self, .call = &gaugeFn } }, .{ .ctx = self, .call = &shedFn });
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "tie: shard derivation — max(next_pow2(cores), 16), halved to the 16 MiB min shard, never below 1" {
    try testing.expectEqual(@as(u32, 16), shardCountFrom(1));
    try testing.expectEqual(@as(u32, 16), shardCountFrom(16));
    try testing.expectEqual(@as(u32, 32), shardCountFrom(17));
    try testing.expectEqual(@as(u32, 64), shardCountFrom(64));
    // 128 MiB default budget on 16 cores: 16 shards × 8 MiB < 16 MiB ⇒ 8 shards × 16 MiB.
    try testing.expectEqual(@as(u32, 8), shardsForBudget(16, 128 * derive.MiB, MIN_SHARD_BYTES));
    try testing.expectEqual(@as(u32, 1), shardsForBudget(64, 8 * derive.MiB, MIN_SHARD_BYTES));
    try testing.expectEqual(@as(u32, 64), shardsForBudget(64, 4 * derive.GiB, MIN_SHARD_BYTES));
    try testing.expectEqual(@as(u32, 1), shardsForBudget(64, 0, MIN_SHARD_BYTES));
    // Budget: max(2 × block, read_mem / 4); read_mem = RAM / 10.
    try testing.expectEqual(@as(u64, 8 * derive.MiB), derivedHotBudget(4 * derive.MiB, 16 * derive.MiB));
    try testing.expectEqual(@as(u64, 256 * derive.MiB), derivedHotBudget(4 * derive.MiB, derive.GiB));
    try testing.expectEqual(@as(u64, 32 * derive.GiB), derivedReadMemLimit(derive.Facts.synthetic(32, 320 * derive.GiB)));
    try testing.expectEqual(@as(u64, 0), resolveHotBudget(0, 4 * derive.MiB, derive.GiB));
    try testing.expectEqual(@as(u64, 48 * derive.MiB), resolveHotBudget(48, 4 * derive.MiB, derive.GiB));
}

test "put classes carry the (protected, referenced, stream) bits" {
    try testing.expectEqual(false, PutClass.probation.bits().referenced);
    try testing.expect(PutClass.probation_referenced.bits().referenced and !PutClass.probation_referenced.bits().protected);
    try testing.expect(PutClass.protected.bits().protected and !PutClass.protected.bits().stream);
    try testing.expect(PutClass.protected_stream.bits().stream);
}

const TestSink = struct {
    victims: std.ArrayList(Victim) = .empty,
    fn on(ctx: ?*anyopaque, v: Victim) void {
        const self: *TestSink = @ptrCast(@alignCast(ctx.?));
        self.victims.append(testing.allocator, v) catch unreachable;
    }
    fn sink(self: *TestSink) EvictSink {
        return .{ .ctx = self, .on_evict = &on };
    }
};

fn fillSlot(p: *fill_pool.Pool, s: u32, v: u8) void {
    @memset(p.bytes(s), v);
}

test "clock discipline: probation victims leave before protected; a referenced entry survives one lap; evictions carry their class; refcount puts" {
    const bs: usize = 4096;
    const pool = try fill_pool.Pool.initRam(testing.allocator, bs, 16);
    defer pool.deinit();
    // Budget 3 blocks, 1 shard (the derivation halves to 1 at this size).
    const tier = try HotTier.init(testing.allocator, pool, bs, 3 * bs, 4);
    defer tier.deinit();
    var sink: TestSink = .{};
    defer sink.victims.deinit(testing.allocator);
    tier.sink = sink.sink();
    try testing.expectEqual(@as(u32, 1), tier.cache.shardCount());

    const k = [_]BlockKey{ .init(1, 0), .init(1, bs), .init(1, 2 * bs), .init(1, 3 * bs), .init(1, 4 * bs) };
    const classes = [_]PutClass{ .probation, .protected, .probation_referenced };
    var slots: [5]u32 = undefined;
    for (0..3) |i| {
        slots[i] = pool.acquire().?;
        fillSlot(pool, slots[i], @intCast(i));
        try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(k[i], slots[i], @intCast(bs), classes[i], .always, false));
        // The tier adopted one reference; the fill's own is released.
        try testing.expectEqual(@as(u32, 2), pool.refCount(slots[i]));
        try testing.expect(!pool.unpin(slots[i]));
    }
    try testing.expectEqual(@as(u64, 3 * bs), tier.currentBytes());
    // A fourth probation put: the clock pops k0 (probation, unreferenced) first.
    slots[3] = pool.acquire().?;
    try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(k[3], slots[3], @intCast(bs), .probation, .always, false));
    _ = pool.unpin(slots[3]);
    try testing.expectEqual(@as(usize, 1), sink.victims.items.len);
    try testing.expect(sink.victims.items[0].key.eql(k[0]));
    try testing.expectEqual(EvictClass.probation, sink.victims.items[0].class);
    try testing.expectEqual(@as(u32, 0), pool.refCount(slots[0]));
    // A fifth: k1 (protected, referenced=1) gets its second chance and is
    // re-queued; k2 (probation_referenced) likewise; k3 (probation, 0) leaves.
    slots[4] = pool.acquire().?;
    try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(k[4], slots[4], @intCast(bs), .probation, .always, false));
    _ = pool.unpin(slots[4]);
    try testing.expectEqual(@as(usize, 2), sink.victims.items.len);
    try testing.expect(sink.victims.items[1].key.eql(k[3]));
    // Now k1 and k2 have referenced cleared. A serving hit on k1 pins the
    // slot and re-arms its second chance; the one-lap grace of k2 is spent.
    const hit = tier.getServing(k[1], 512).?;
    try testing.expectEqual(@as(u32, 2), pool.refCount(hit.slot));
    try testing.expectEqual(@as(u8, 1), pool.bytes(hit.slot)[0]);
    tier.unpin(hit);
    const bits = tier.cache.bitsOf(k[1]).?;
    try testing.expect(bits.protected and bits.referenced);
    try testing.expectEqual(@as(u64, 512), bits.served);
    // The ring is now [k4, k1, k2] (re-queued entries go to the tail): k4
    // (probation, never referenced) leaves next.
    const extra = pool.acquire().?;
    try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(.init(1, 9 * bs), extra, @intCast(bs), .probation, .always, false));
    _ = pool.unpin(extra);
    try testing.expectEqual(@as(usize, 3), sink.victims.items.len);
    try testing.expect(sink.victims.items[2].key.eql(k[4]));
    try testing.expectEqual(EvictClass.probation, sink.victims.items[2].class);
    // Then k1 (re-armed by the serve) is spared once more and k2 — whose
    // one-lap grace was consumed — leaves and classifies PROBATION: the
    // grace is not keep-worthiness.
    const extra2 = pool.acquire().?;
    try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(.init(1, 10 * bs), extra2, @intCast(bs), .probation, .always, false));
    _ = pool.unpin(extra2);
    try testing.expectEqual(@as(usize, 4), sink.victims.items.len);
    try testing.expect(sink.victims.items[3].key.eql(k[2]));
    try testing.expectEqual(EvictClass.probation, sink.victims.items[3].class);
    try testing.expect(tier.contains(k[1]));
}

test "protected is sticky: a probation re-put never demotes; a promoting get sets it; a protected victim classifies protected with its served credit" {
    const bs: usize = 4096;
    const pool = try fill_pool.Pool.initRam(testing.allocator, bs, 8);
    defer pool.deinit();
    const tier = try HotTier.init(testing.allocator, pool, bs, 2 * bs, 1);
    defer tier.deinit();
    var sink: TestSink = .{};
    defer sink.victims.deinit(testing.allocator);
    tier.sink = sink.sink();
    const k0 = BlockKey.init(2, 0);
    const s0 = pool.acquire().?;
    try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(k0, s0, @intCast(bs), .probation, .always, false));
    _ = pool.unpin(s0);
    try testing.expect(!tier.cache.bitsOf(k0).?.protected);
    // A serving get promotes.
    tier.unpin(tier.getServing(k0, 100).?);
    try testing.expect(tier.cache.bitsOf(k0).?.protected);
    // A probation re-put replaces the value and keeps the sticky bit.
    const s1 = pool.acquire().?;
    fillSlot(pool, s1, 7);
    try testing.expectEqual(PutOutcome.replaced, tier.putClassValidated(k0, s1, @intCast(bs), .probation, .always, false));
    _ = pool.unpin(s1);
    try testing.expectEqual(@as(u32, 0), pool.refCount(s0));
    try testing.expect(tier.cache.bitsOf(k0).?.protected);
    const h = tier.getNoPromote(k0).?;
    try testing.expectEqual(@as(u8, 7), pool.bytes(h.slot)[0]);
    tier.unpin(h);
    // Shed to zero ignores second chances and classifies the victim.
    tier.shedTo(0);
    try testing.expectEqual(@as(usize, 1), sink.victims.items.len);
    try testing.expectEqual(EvictClass.protected, sink.victims.items[0].class);
    try testing.expectEqual(@as(u64, 100), sink.victims.items[0].served_bytes);
    try testing.expectEqual(@as(u64, 0), tier.currentBytes());
    try testing.expectEqual(@as(usize, 0), tier.cache.liveEntries());
}

test "validated put: a refusing validate at the visibility point inserts nothing and removes a stale occupant; purge drops the entry; a disabled tier misses" {
    const bs: usize = 4096;
    const pool = try fill_pool.Pool.initRam(testing.allocator, bs, 8);
    defer pool.deinit();
    const tier = try HotTier.init(testing.allocator, pool, bs, 4 * bs, 1);
    defer tier.deinit();
    const k = BlockKey.init(3, 0);
    const Refuse = struct {
        fn no(_: ?*anyopaque) bool {
            return false;
        }
    };
    const refuse: Validate = .{ .call = &Refuse.no };
    const s0 = pool.acquire().?;
    try testing.expectEqual(PutOutcome.refused, tier.putClassValidated(k, s0, @intCast(bs), .protected, refuse, false));
    // The caller still owns its reference.
    try testing.expectEqual(@as(u32, 1), pool.refCount(s0));
    try testing.expect(!tier.contains(k));
    try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(k, s0, @intCast(bs), .protected, .always, false));
    _ = pool.unpin(s0);
    const s1 = pool.acquire().?;
    try testing.expectEqual(PutOutcome.refused, tier.putClassValidated(k, s1, @intCast(bs), .protected, refuse, false));
    // The occupant was removed (absence is correctness-safe).
    try testing.expect(!tier.contains(k));
    try testing.expectEqual(@as(u32, 0), pool.refCount(s0));
    try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(k, s1, @intCast(bs), .probation, .always, false));
    _ = pool.unpin(s1);
    try testing.expect(tier.purge(k));
    try testing.expect(!tier.purge(k));
    try testing.expectEqual(@as(u32, 0), pool.refCount(s1));
    // Too large for the shard budget: never cached.
    const s2 = pool.acquire().?;
    try testing.expectEqual(PutOutcome.too_large, tier.putClassValidated(k, s2, @intCast(5 * bs), .probation, .always, false));
    try testing.expect(pool.unpin(s2));
    const off = try HotTier.init(testing.allocator, pool, bs, 0, 1);
    defer off.deinit();
    try testing.expect(!off.enabled());
    const s3 = pool.acquire().?;
    try testing.expectEqual(PutOutcome.refused, off.putClassValidated(k, s3, @intCast(bs), .protected, .always, false));
    try testing.expect(pool.unpin(s3));
    try testing.expect(off.getServing(k, 0) == null);
}

test "ring reclaim: purges leave tombstones that the drain-and-refill bounds at 2 × live + 64" {
    const bs: usize = 4096;
    const pool = try fill_pool.Pool.initRam(testing.allocator, bs, 4);
    defer pool.deinit();
    const tier = try HotTier.init(testing.allocator, pool, bs, 200 * bs, 1);
    defer tier.deinit();
    const k0 = BlockKey.init(4, 0);
    var i: u64 = 1;
    // One live entry, then 200 insert/purge pairs under other keys.
    const s = pool.acquire().?;
    try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(k0, s, @intCast(bs), .protected, .always, false));
    _ = pool.unpin(s);
    while (i <= 200) : (i += 1) {
        const kk = BlockKey.init(4, i * bs);
        const sl = pool.acquire().?;
        try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(kk, sl, @intCast(bs), .probation, .always, false));
        _ = pool.unpin(sl);
        try testing.expect(tier.purge(kk));
    }
    try testing.expect(tier.cache.ringLenOf(k0) <= 2 * 1 + RING_RECLAIM_SLACK + 1);
    try testing.expect(tier.contains(k0));
}

test "freeze (Yellow+): the insert is net-zero growth" {
    const bs: usize = 4096;
    const pool = try fill_pool.Pool.initRam(testing.allocator, bs, 8);
    defer pool.deinit();
    const tier = try HotTier.init(testing.allocator, pool, bs, 4 * bs, 1);
    defer tier.deinit();
    for (0..2) |i| {
        const sl = pool.acquire().?;
        try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(.init(5, i * bs), sl, @intCast(bs), .probation, .always, false));
        _ = pool.unpin(sl);
    }
    try testing.expectEqual(@as(u64, 2 * bs), tier.currentBytes());
    const sl = pool.acquire().?;
    try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(.init(5, 7 * bs), sl, @intCast(bs), .protected, .always, true));
    _ = pool.unpin(sl);
    try testing.expectEqual(@as(u64, 2 * bs), tier.currentBytes());
    try testing.expect(tier.contains(.init(5, 7 * bs)));
}

test "component: hot_block_tier floor 64 MiB weight 4, gauge reads the tier, shed trims" {
    const bs: usize = 4096;
    const pool = try fill_pool.Pool.initRam(testing.allocator, bs, 4);
    defer pool.deinit();
    const tier = try HotTier.init(testing.allocator, pool, bs, 4 * bs, 1);
    defer tier.deinit();
    const c = tier.component();
    try testing.expectEqualStrings("hot_block_tier", c.name);
    try testing.expectEqual(COMPONENT_FLOOR, c.floor);
    try testing.expectEqual(COMPONENT_WEIGHT, c.weight);
    const sl = pool.acquire().?;
    try testing.expectEqual(PutOutcome.inserted, tier.putClassValidated(.init(6, 0), sl, @intCast(bs), .protected, .always, false));
    _ = pool.unpin(sl);
    try testing.expectEqual(@as(u64, bs), c.current());
    c.shed.?.call(c.shed.?.ctx, 0);
    try testing.expectEqual(@as(u64, 0), c.current());
}
