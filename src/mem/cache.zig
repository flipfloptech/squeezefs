//! THE ONE CACHE CORE (owner's design direction 2026-10-21): one LRU model
//! used in one direction for reading and reversed for writing. The read
//! cache is `Cache(DataGrain, DropSink)` (`data/read_cache.zig`): on
//! eviction the bytes are DROPPED — the device still has them. The write
//! cache is `Cache(Resident, PromoteSink)` over the staging ring: on
//! eviction the resident is PROMOTED to the data volumes before its memory
//! is released. Nothing direction-specific lives here: the core owns the
//! recency order, the bound, the epoch validation and the eviction
//! protocol; the instance owns the bytes (a slab slot it owns, a ring
//! segment it does not) through its `Entry` and its `Sink`. PORTABLE; it
//! imports no plane.
//!
//! `Cache(Entry, Sink)`:
//!   - `Entry` is a plain value with `key: Key` (`Key.hash() u64`,
//!     `Key.eql()`), `ino: u64`, `epoch: u64` (the inode's lease/witness
//!     epoch it was filled under) and `bytes: u32` (the memory it charges).
//!     The shard is `key.hash() & mask`: a Key whose hash covers only part
//!     of the key (a block, not the grain) keeps related entries in one
//!     shard so `lookupMany` and a ranged invalidation take one lock.
//!   - `Sink` has `onEvict(*Sink, *const Entry, Why) Outcome` and the
//!     comptime `runs_under_lock`. A sink that runs under the lock (the
//!     drop sink: a slot's reference released, never blocking) is called
//!     with the shard held and must answer `.released`. A sink that may
//!     park (the promote sink: a DMA, a flush) runs OUTSIDE the lock under
//!     the EVICTION PROTOCOL: the victim is marked `evicting` and stays
//!     INDEXED and SERVABLE while the sink runs (a resident is visible until
//!     its bytes are on the volume), then under the lock it is removed on
//!     `.released` or, on `.kept` (the sink failed), relinked at the head
//!     and the pass BACKS OFF (`Progress.kept`) — bytes are never lost.
//!   - LRU is EXACT within a shard: an intrusive doubly linked list over a
//!     preallocated item table; a hit's touch unlinks and relinks at the
//!     head under the shard's spin lock (O(1), no allocation); the shard
//!     over its share evicts its tail. The approximation is the sharding
//!     alone — `S` shards split the bound evenly and keys hash uniformly,
//!     so a victim is the oldest of a 1/S slice, not of the whole (a
//!     shard's tail may be younger than another's by the hash's skew). No
//!     frequency, no second chance (owner ruling 2026-10-21).
//!   - Epoch validation on lookup: with an `EpochSource` bound, an entry
//!     whose inode's epoch moved since its fill is a STALE REFUSAL — not
//!     served, evicted through the sink (the read view's and the lease
//!     epoch's law: a mutation of the inode's records or custody since the
//!     fill may have made the bytes the wrong ones).
//!   - The bound: `derivedBound(pct, RAM, budget)` = the `*_PCT` knob's
//!     share of SYSTEM RAM, scaled down with the budget authority's
//!     ceiling when that ceiling is below the solo 70 % (a cgroup cage, a
//!     fleet share); the authority's pressure levels re-derive it live —
//!     Yellow freezes growth (`insert(.., .freeze)` evicts first), Red
//!     sheds to the authority's target through the component's `shed`.
//!
//! The striped lock makes the touch-vs-evict race a locked section, not a
//! lock-free protocol: a touch and an eviction of one item serialize on
//! the shard, so no model is owed (the tier that pins bytes across the
//! lock — the read cache's slab — proves its own pin-vs-free order).

const std = @import("std");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const budget = @import("budget.zig");
const jc = @import("../meta/journal_core.zig");

/// `max(next_pow2(cores), 16)`, halved while `bound / shards < min_shard`
/// (never below 1): a shard must hold enough entries that colliding keys
/// coexist (the hot tier's lesson: one block per shard refetched 6.1 k
/// times on a fitting set).
pub fn shardsFor(cores: u32, bound: u64, min_shard: u64) u32 {
    var n: u32 = @intCast(@max(derive.nextPow2(cores), 16));
    while (n > 1 and bound / n < min_shard) n /= 2;
    return n;
}

/// The solo budget's share of RAM (`budget.resolveBudgetFrom`: 70 %).
pub const SOLO_BUDGET_TENTHS: u64 = 7;

/// `pct % × min(RAM, budget ÷ 0.7)`: the percentage of SYSTEM RAM the knob
/// names, scaled down with the authority's ceiling when it is below the
/// solo 70 % so the cache keeps its proportion of what the budget may
/// spend. 0 % is off.
pub fn derivedBound(pct: u64, total_ram: u64, budget_bytes: u64) u64 {
    assert(pct <= 100);
    const base = @min(total_ram, budget_bytes / SOLO_BUDGET_TENTHS *| 10);
    return @intCast(@as(u128, base) * pct / 100);
}

pub fn resolveBound(pct: u64, facts: derive.Facts, budget_bytes: u64) u64 {
    return derivedBound(pct, facts.total_ram, budget_bytes);
}

/// Why the sink is handed an entry.
pub const Why = enum(u8) {
    /// The shard is over its share (an insert, `evictUntil`, a shed).
    bound,
    /// The inode's epoch moved since the fill (a lookup's refusal).
    stale,
    /// `invalidate*`: the caller declared the entry's bytes superseded.
    invalidate,
    /// The instance is going away.
    deinit,
};

pub const Outcome = enum(u8) {
    /// The memory may be released: the core drops the entry.
    released,
    /// The sink could not take the bytes (a promotion failed): the entry
    /// stays, relinked at the head, and the pass backs off.
    kept,
};

/// An eviction pass's result.
pub const Progress = struct {
    released: u32 = 0,
    kept: u32 = 0,
};

/// The insert-time validation (the read path's incarnation `still`), run
/// under the shard lock at the visibility point. `null` call = always.
pub const Validate = struct {
    ctx: ?*anyopaque = null,
    call: ?*const fn (ctx: ?*anyopaque) bool = null,

    pub const always: Validate = .{};

    pub fn run(self: Validate) bool {
        const f = self.call orelse return true;
        return f(self.ctx);
    }
};

/// The inode epoch a lookup validates against (`lease_epoch.load`, the
/// witness's `genOf`, a test's table).
pub const EpochSource = struct {
    ctx: ?*anyopaque = null,
    call: *const fn (ctx: ?*anyopaque, ino: u64) u64,
};

pub const InsertOutcome = enum(u8) {
    inserted,
    /// The key is held: the instance invalidates it first if it means to
    /// replace (two steps, so a blocking sink never runs under an insert).
    exists,
    /// The validation refused, or the bound is 0, or no entry slot could
    /// be made (every tail kept by its sink).
    refused,
};

pub const InsertOptions = struct {
    validate: Validate = .always,
    /// Yellow: net-zero growth — the bound is the shard's bytes at entry.
    freeze: bool = false,
};

/// The per-instance instruments (the `.stats` family the instance exports).
pub const Stats = struct {
    hits: counters.ShardedCounter = .{},
    misses: counters.ShardedCounter = .{},
    fills: counters.ShardedCounter = .{},
    evictions: counters.ShardedCounter = .{},
    stale_refusals: counters.ShardedCounter = .{},
    invalidations: counters.ShardedCounter = .{},
    sink_released: counters.ShardedCounter = .{},
    sink_kept: counters.ShardedCounter = .{},
    insert_refusals: counters.ShardedCounter = .{},
    bytes: counters.Gauge = .{},
    bound_bytes: counters.Gauge = .{},
    shards: counters.Gauge = .{},

    /// The family's keys, each prefixed (`<prefix>_hits`, …), inside the
    /// caller's braces.
    pub fn writeJson(self: *const Stats, w: *std.Io.Writer, prefix: []const u8) std.Io.Writer.Error!void {
        try w.print("\"{s}_hits\":{d},\"{s}_misses\":{d},\"{s}_fills\":{d},\"{s}_evictions\":{d},\"{s}_stale_refusals\":{d},\"{s}_invalidations\":{d},\"{s}_sink_released\":{d},\"{s}_sink_kept\":{d},\"{s}_insert_refusals\":{d},\"{s}_bytes\":{d},\"{s}_bound_bytes\":{d},\"{s}_shards\":{d}", .{
            prefix, self.hits.load(),
            prefix, self.misses.load(),
            prefix, self.fills.load(),
            prefix, self.evictions.load(),
            prefix, self.stale_refusals.load(),
            prefix, self.invalidations.load(),
            prefix, self.sink_released.load(),
            prefix, self.sink_kept.load(),
            prefix, self.insert_refusals.load(),
            prefix, self.bytes.load(),
            prefix, self.bound_bytes.load(),
            prefix, self.shards.load(),
        });
    }
};

const NIL: u32 = std.math.maxInt(u32);

pub fn Cache(comptime Entry: type, comptime Sink: type) type {
    comptime {
        if (!@hasDecl(Entry, "Key")) @compileError(@typeName(Entry) ++ " needs a `Key` type with hash() and eql()");
        if (!@hasField(Entry, "key") or !@hasField(Entry, "ino") or !@hasField(Entry, "epoch") or !@hasField(Entry, "bytes")) @compileError(@typeName(Entry) ++ " needs fields key, ino, epoch, bytes");
        if (!@hasDecl(Sink, "onEvict")) @compileError(@typeName(Sink) ++ " needs onEvict(*Sink, *const Entry, Why) Outcome");
        if (!@hasDecl(Sink, "runs_under_lock")) @compileError(@typeName(Sink) ++ " needs `pub const runs_under_lock: bool`");
    }
    return struct {
        const Self = @This();
        pub const Key = Entry.Key;
        const under_lock: bool = Sink.runs_under_lock;

        const KeyContext = struct {
            pub fn hash(_: KeyContext, k: Key) u64 {
                return k.hash();
            }
            pub fn eql(_: KeyContext, a: Key, b: Key) bool {
                return a.eql(b);
            }
        };
        const KeyMap = std.HashMapUnmanaged(Key, u32, KeyContext, std.hash_map.default_max_load_percentage);
        const InoMap = std.AutoHashMapUnmanaged(u64, u32);

        const State = enum(u8) { free, live, evicting };

        const Item = struct {
            entry: Entry = undefined,
            prev: u32 = NIL,
            next: u32 = NIL,
            ino_prev: u32 = NIL,
            ino_next: u32 = NIL,
            state: State = .free,
        };

        const Shard = struct {
            lock: jc.SpinLock = .{},
            map: KeyMap = .empty,
            inos: InoMap = .empty,
            items: []Item,
            free: []u32,
            free_len: usize,
            head: u32 = NIL,
            tail: u32 = NIL,
            bytes: Atomic(u64) align(std.atomic.cache_line) = .init(0),
            max_bytes: u64,

            fn linkHead(s: *Shard, idx: u32) void {
                const it = &s.items[idx];
                it.prev = NIL;
                it.next = s.head;
                if (s.head != NIL) s.items[s.head].prev = idx else s.tail = idx;
                s.head = idx;
            }

            fn unlink(s: *Shard, idx: u32) void {
                const it = &s.items[idx];
                if (it.prev != NIL) s.items[it.prev].next = it.next else s.head = it.next;
                if (it.next != NIL) s.items[it.next].prev = it.prev else s.tail = it.prev;
                it.prev = NIL;
                it.next = NIL;
            }

            fn touch(s: *Shard, idx: u32) void {
                if (s.head == idx) return;
                s.unlink(idx);
                s.linkHead(idx);
            }

            fn inoLink(s: *Shard, idx: u32) void {
                const it = &s.items[idx];
                const ino = it.entry.ino;
                const gop = s.inos.getOrPutAssumeCapacity(ino);
                it.ino_prev = NIL;
                it.ino_next = if (gop.found_existing) gop.value_ptr.* else NIL;
                if (it.ino_next != NIL) s.items[it.ino_next].ino_prev = idx;
                gop.value_ptr.* = idx;
            }

            fn inoUnlink(s: *Shard, idx: u32) void {
                const it = &s.items[idx];
                if (it.ino_prev != NIL) s.items[it.ino_prev].ino_next = it.ino_next else {
                    if (it.ino_next != NIL) s.inos.putAssumeCapacity(it.entry.ino, it.ino_next) else assert(s.inos.remove(it.entry.ino));
                }
                if (it.ino_next != NIL) s.items[it.ino_next].ino_prev = it.ino_prev;
                it.ino_prev = NIL;
                it.ino_next = NIL;
            }

            /// The oldest live (not evicting) item from the tail.
            fn victim(s: *Shard) ?u32 {
                var idx = s.tail;
                while (idx != NIL) : (idx = s.items[idx].prev) {
                    if (s.items[idx].state == .live) return idx;
                }
                return null;
            }

            fn removeLocked(s: *Shard, idx: u32) void {
                const it = &s.items[idx];
                assert(s.map.remove(it.entry.key));
                s.inoUnlink(idx);
                s.unlink(idx);
                _ = s.bytes.fetchSub(it.entry.bytes, .monotonic);
                it.state = .free;
                s.free[s.free_len] = idx;
                s.free_len += 1;
            }
        };

        gpa: Allocator,
        sink: *Sink,
        shards: []Shard,
        mask: u64,
        bound: u64,
        epochs: ?EpochSource,
        stats: Stats = .{},

        /// `bound == 0` disables the instance (every lookup misses, every
        /// insert refuses). `per_shard_items` bounds the item table per
        /// shard: the instance derives it from its smallest entry.
        pub fn init(gpa: Allocator, sink: *Sink, bound: u64, shard_count: u32, per_shard_items: usize, epochs: ?EpochSource) Allocator.Error!*Self {
            assert(shard_count > 0 and std.math.isPowerOfTwo(shard_count) and per_shard_items > 0);
            const self = try gpa.create(Self);
            errdefer gpa.destroy(self);
            const shards = try gpa.alloc(Shard, shard_count);
            errdefer gpa.free(shards);
            var built: usize = 0;
            errdefer for (shards[0..built]) |*s| destroyShard(gpa, s);
            for (shards) |*s| {
                const items = try gpa.alloc(Item, per_shard_items);
                errdefer gpa.free(items);
                @memset(items, .{});
                const free = try gpa.alloc(u32, per_shard_items);
                errdefer gpa.free(free);
                var i: u32 = 0;
                while (i < per_shard_items) : (i += 1) free[i] = @intCast(per_shard_items - 1 - i);
                s.* = .{ .items = items, .free = free, .free_len = per_shard_items, .max_bytes = bound / shard_count };
                try s.map.ensureTotalCapacity(gpa, @intCast(per_shard_items));
                try s.inos.ensureTotalCapacity(gpa, @intCast(per_shard_items));
                built += 1;
            }
            self.* = .{ .gpa = gpa, .sink = sink, .shards = shards, .mask = shard_count - 1, .bound = bound, .epochs = epochs };
            self.stats.shards.set(shard_count);
            self.stats.bound_bytes.set(bound);
            return self;
        }

        fn destroyShard(gpa: Allocator, s: *Shard) void {
            s.map.deinit(gpa);
            s.inos.deinit(gpa);
            gpa.free(s.items);
            gpa.free(s.free);
        }

        /// Every live entry goes to the sink as `.deinit` (its answer is
        /// not consulted: the instance's storage is the instance's).
        pub fn deinit(self: *Self) void {
            for (self.shards) |*s| {
                for (s.items) |*it| if (it.state != .free) {
                    _ = self.sink.onEvict(&it.entry, .deinit);
                };
                destroyShard(self.gpa, s);
            }
            self.gpa.free(self.shards);
            self.gpa.destroy(self);
        }

        pub fn enabled(self: *const Self) bool {
            return self.bound != 0;
        }

        pub fn shardIndex(self: *const Self, key: Key) usize {
            return @intCast(key.hash() & self.mask);
        }

        fn shardOf(self: *Self, key: Key) *Shard {
            return &self.shards[self.shardIndex(key)];
        }

        fn stale(self: *Self, e: *const Entry) bool {
            const src = self.epochs orelse return false;
            return src.call(src.ctx, e.ino) != e.epoch;
        }

        // ---- lookups ----

        /// Find `key`: the visitor runs UNDER the shard lock with the entry
        /// (the instance pins its bytes there); the entry is touched to the
        /// head. A stale entry (its inode's epoch moved) is refused and
        /// evicted; absent ⇒ false.
        pub fn lookup(self: *Self, key: Key, ctx: anytype, comptime visit: fn (@TypeOf(ctx), *const Entry) void) bool {
            return self.lookupMany(&.{key}, ctx, visit) == 1;
        }

        /// `lookup` for keys of ONE shard under one lock: every key found
        /// and current ⇒ each visited (in order) and touched, `keys.len`;
        /// else nothing visited and the count of keys found. A stale key is
        /// refused and evicted like a single lookup.
        pub fn lookupMany(self: *Self, keys: []const Key, ctx: anytype, comptime visit: fn (@TypeOf(ctx), *const Entry) void) usize {
            if (!self.enabled() or keys.len == 0) return 0;
            const s = self.shardOf(keys[0]);
            var stale_idx: ?u32 = null;
            var found: usize = 0;
            {
                s.lock.lock();
                defer s.lock.unlock();
                for (keys) |k| {
                    assert(&self.shards[self.shardIndex(k)] == s);
                    const idx = s.map.get(k) orelse break;
                    const it = &s.items[idx];
                    if (self.stale(&it.entry)) {
                        self.stats.stale_refusals.inc();
                        if (it.state == .live) stale_idx = idx;
                        break;
                    }
                    found += 1;
                }
                if (found == keys.len) {
                    for (keys) |k| {
                        const idx = s.map.get(k).?;
                        s.touch(idx);
                        visit(ctx, &s.items[idx].entry);
                    }
                    self.stats.hits.inc();
                    return found;
                }
                if (stale_idx) |idx| {
                    if (under_lock) self.releaseLocked(s, idx, .stale) else s.items[idx].state = .evicting;
                }
            }
            if (!under_lock) if (stale_idx) |idx| self.finishOutside(s, idx, .stale);
            self.stats.misses.inc();
            return found;
        }

        pub fn contains(self: *Self, key: Key) bool {
            if (!self.enabled()) return false;
            const s = self.shardOf(key);
            s.lock.lock();
            defer s.lock.unlock();
            return s.map.contains(key);
        }

        // ---- the eviction protocol ----

        /// Under the lock, a sink that runs under it: release and remove.
        fn releaseLocked(self: *Self, s: *Shard, idx: u32, why: Why) void {
            comptime assert(under_lock);
            const out = self.sink.onEvict(&s.items[idx].entry, why);
            assert(out == .released);
            self.stats.sink_released.inc();
            s.removeLocked(idx);
            self.stats.evictions.inc();
            if (why == .invalidate or why == .stale) self.stats.invalidations.inc();
        }

        /// Outside the lock, a sink that may park: run it on the marked
        /// item, then finalize under the lock. Returns true on release.
        fn finishOutside(self: *Self, s: *Shard, idx: u32, why: Why) bool {
            comptime assert(!under_lock);
            const out = self.sink.onEvict(&s.items[idx].entry, why);
            s.lock.lock();
            defer s.lock.unlock();
            assert(s.items[idx].state == .evicting);
            switch (out) {
                .released => {
                    self.stats.sink_released.inc();
                    s.removeLocked(idx);
                    self.stats.evictions.inc();
                    if (why == .invalidate or why == .stale) self.stats.invalidations.inc();
                    return true;
                },
                .kept => {
                    self.stats.sink_kept.inc();
                    s.items[idx].state = .live;
                    s.touch(idx);
                    return false;
                },
            }
        }

        /// Evict `s`'s tails until `s.bytes + need ≤ bound` (or a sink keeps
        /// its entry: back off). Called WITHOUT the lock held.
        fn evictShardTo(self: *Self, s: *Shard, bound: u64, need: u64) Progress {
            var p: Progress = .{};
            while (true) {
                s.lock.lock();
                if (s.bytes.load(.monotonic) + need <= bound) {
                    s.lock.unlock();
                    return p;
                }
                const idx = s.victim() orelse {
                    s.lock.unlock();
                    return p;
                };
                if (under_lock) {
                    self.releaseLocked(s, idx, .bound);
                    s.lock.unlock();
                    p.released += 1;
                    continue;
                }
                s.items[idx].state = .evicting;
                s.lock.unlock();
                if (self.finishOutside(s, idx, .bound)) {
                    p.released += 1;
                } else {
                    p.kept += 1;
                    return p;
                }
            }
        }

        // ---- inserts ----

        /// Insert `entry` at the head of its shard, evicting tails to make
        /// room (within the shard's share; with `freeze`, within its bytes
        /// at entry). The validation runs under the lock at the visibility
        /// point. `.exists` when the key is held (invalidate first).
        pub fn insert(self: *Self, entry: Entry, opts: InsertOptions) InsertOutcome {
            if (!self.enabled()) return .refused;
            const s = self.shardOf(entry.key);
            if (self.contains(entry.key)) return .exists;
            const bound = if (opts.freeze) @min(s.bytes.load(.monotonic), s.max_bytes) else s.max_bytes;
            // Room first (the sink may park: never under the lock the
            // validation needs), then the validated insert. A sink that
            // keeps its victim backs the insert off: one refusal, never a
            // storm of failing promotions down the tail.
            var attempts: u32 = 0;
            while (attempts < 2) : (attempts += 1) {
                if (self.evictShardTo(s, @max(bound, entry.bytes), entry.bytes).kept > 0) break;
                s.lock.lock();
                if (s.map.contains(entry.key)) {
                    s.lock.unlock();
                    return .exists;
                }
                if (s.free_len == 0 or s.bytes.load(.monotonic) + entry.bytes > @max(bound, entry.bytes)) {
                    // A concurrent insert took the room: once more, then refuse.
                    s.lock.unlock();
                    continue;
                }
                defer s.lock.unlock();
                if (!opts.validate.run()) {
                    self.stats.insert_refusals.inc();
                    return .refused;
                }
                s.free_len -= 1;
                const idx = s.free[s.free_len];
                s.items[idx] = .{ .entry = entry, .state = .live };
                s.map.putAssumeCapacity(entry.key, idx);
                s.inoLink(idx);
                s.linkHead(idx);
                _ = s.bytes.fetchAdd(entry.bytes, .monotonic);
                self.stats.fills.inc();
                self.stats.bytes.set(self.currentBytes());
                return .inserted;
            }
            self.stats.insert_refusals.inc();
            return .refused;
        }

        // ---- invalidation ----

        /// Drop `key` (through the sink, `.invalidate`); true when held.
        pub fn invalidateKey(self: *Self, key: Key) bool {
            if (!self.enabled()) return false;
            const s = self.shardOf(key);
            return self.invalidateKeys(s, &.{key}) > 0;
        }

        /// Drop every key of ONE shard that is held (one lock for the
        /// under-lock sink); returns the entries released.
        pub fn invalidateKeys(self: *Self, s: *Shard, keys: []const Key) u32 {
            var released: u32 = 0;
            if (under_lock) {
                s.lock.lock();
                defer s.lock.unlock();
                for (keys) |k| {
                    const idx = s.map.get(k) orelse continue;
                    if (s.items[idx].state != .live) continue;
                    self.releaseLocked(s, idx, .invalidate);
                    released += 1;
                }
            } else {
                for (keys) |k| {
                    s.lock.lock();
                    const idx = s.map.get(k) orelse {
                        s.lock.unlock();
                        continue;
                    };
                    if (s.items[idx].state != .live) {
                        s.lock.unlock();
                        continue;
                    }
                    s.items[idx].state = .evicting;
                    s.lock.unlock();
                    if (self.finishOutside(s, idx, .invalidate)) released += 1;
                }
            }
            if (released > 0) self.stats.bytes.set(self.currentBytes());
            return released;
        }

        /// The shard of `key` (for `invalidateKeys` over keys the caller
        /// knows share one).
        pub fn shardFor(self: *Self, key: Key) *Shard {
            return self.shardOf(key);
        }

        /// Drop every entry of `ino` that `pick` selects (null = all),
        /// through the sink, in batches of 16 per lock hold (the chain is
        /// walked without mutation, then the batch is released — under the
        /// lock for an under-lock sink, outside it under the protocol); a
        /// kept entry ends the pass over its shard.
        pub fn invalidateIno(self: *Self, ino: u64, ctx: anytype, comptime pick: ?fn (@TypeOf(ctx), *const Entry) bool) Progress {
            var p: Progress = .{};
            if (!self.enabled()) return p;
            for (self.shards) |*s| {
                var batch: [16]u32 = undefined;
                while (true) {
                    var n: usize = 0;
                    s.lock.lock();
                    var idx = s.inos.get(ino) orelse NIL;
                    while (idx != NIL and n < batch.len) : (idx = s.items[idx].ino_next) {
                        const it = &s.items[idx];
                        if (it.state != .live) continue;
                        if (pick) |f| if (!f(ctx, &it.entry)) continue;
                        if (!under_lock) it.state = .evicting;
                        batch[n] = idx;
                        n += 1;
                    }
                    if (under_lock) {
                        for (batch[0..n]) |b| self.releaseLocked(s, b, .invalidate);
                        p.released += @intCast(n);
                        s.lock.unlock();
                        if (n < batch.len) break;
                        continue;
                    }
                    s.lock.unlock();
                    if (n == 0) break;
                    var kept = false;
                    for (batch[0..n]) |b| {
                        if (self.finishOutside(s, b, .invalidate)) p.released += 1 else {
                            p.kept += 1;
                            kept = true;
                        }
                    }
                    if (kept or n < batch.len) break;
                }
            }
            if (p.released > 0) self.stats.bytes.set(self.currentBytes());
            return p;
        }

        /// The census face: the next entry at or after `cursor` (the shard
        /// in the cursor's high half, the item slot in its low half) that
        /// `pick` selects, as a COPY, the cursor moved past it; null at the
        /// end. One shard lock at a time, nothing allocated.
        pub fn censusNext(self: *Self, cursor: *u64, ctx: anytype, comptime pick: fn (@TypeOf(ctx), *const Entry) bool) ?Entry {
            if (!self.enabled()) return null;
            var si: usize = @intCast(cursor.* >> 32);
            var slot: usize = @intCast(cursor.* & std.math.maxInt(u32));
            while (si < self.shards.len) : ({
                si += 1;
                slot = 0;
            }) {
                const s = &self.shards[si];
                s.lock.lock();
                defer s.lock.unlock();
                while (slot < s.items.len) : (slot += 1) {
                    const it = &s.items[slot];
                    if (it.state != .free and pick(ctx, &it.entry)) {
                        cursor.* = (@as(u64, si) << 32) | (slot + 1);
                        return it.entry;
                    }
                }
            }
            cursor.* = @as(u64, si) << 32;
            return null;
        }

        /// Evict the tail of `key`'s shard (one entry, through the sink);
        /// false when the shard is empty or its tail was kept.
        pub fn evictOne(self: *Self, key: Key) bool {
            if (!self.enabled()) return false;
            const s = self.shardOf(key);
            const bytes = s.bytes.load(.monotonic);
            if (bytes == 0) return false;
            return self.evictShardTo(s, bytes - 1, 0).released > 0;
        }

        // ---- the bound under pressure ----

        /// Evict tails across the shards until the entry bytes ≤ `target`
        /// (each shard to its proportional share), through the sink.
        pub fn evictUntil(self: *Self, target: u64) Progress {
            var p: Progress = .{};
            if (!self.enabled()) return p;
            const per_shard = target / self.shards.len;
            for (self.shards) |*s| {
                const q = self.evictShardTo(s, per_shard, 0);
                p.released += q.released;
                p.kept += q.kept;
            }
            self.stats.bytes.set(self.currentBytes());
            return p;
        }

        /// `evictUntil(current − bytes)`.
        pub fn shed(self: *Self, bytes: u64) Progress {
            return self.evictUntil(self.currentBytes() -| bytes);
        }

        pub fn currentBytes(self: *const Self) u64 {
            var n: u64 = 0;
            for (self.shards) |*s| n += s.bytes.load(.monotonic);
            return n;
        }

        /// The budget component: the instance's gauge (its RAM: resident
        /// slab bytes, a ring's segments) and trim; the shed is the core's
        /// `evictUntil`.
        pub fn component(self: *Self, name: []const u8, floor: u64, weight: u64, gauge: budget.Source, trim: ?budget.TrimFn) budget.Component {
            const c = budget.Component.init(name, @min(floor, self.bound), weight, gauge, .{ .ctx = self, .call = &shedFn });
            return if (trim) |t| c.withTrim(t) else c;
        }

        fn shedFn(ctx: *anyopaque, target: u64) void {
            const self: *Self = @ptrCast(@alignCast(ctx));
            _ = self.evictUntil(target);
        }
    };
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const TestKey = struct {
    ino: u64,
    off: u64,

    /// The shard covers the ino alone: one ino's entries share a shard.
    pub fn hash(self: TestKey) u64 {
        return std.hash.Wyhash.hash(0, std.mem.asBytes(&self.ino));
    }
    pub fn eql(a: TestKey, b: TestKey) bool {
        return a.ino == b.ino and a.off == b.off;
    }
};

const TestEntry = struct {
    pub const Key = TestKey;
    key: Key,
    ino: u64,
    epoch: u64,
    bytes: u32,
    /// The instance's storage handle (a slot).
    slot: u32,
};

/// The read direction's sink: releases a slot's reference under the lock.
const DropSink = struct {
    pub const runs_under_lock = true;
    released: std.ArrayList(u32) = .empty,
    whys: std.ArrayList(Why) = .empty,

    pub fn onEvict(self: *DropSink, e: *const TestEntry, why: Why) Outcome {
        self.released.append(testing.allocator, e.slot) catch unreachable;
        self.whys.append(testing.allocator, why) catch unreachable;
        return .released;
    }
    fn deinit(self: *DropSink) void {
        self.released.deinit(testing.allocator);
        self.whys.deinit(testing.allocator);
    }
};

/// The write direction's sink: "promotes" outside the lock, may refuse.
const PromoteSink = struct {
    pub const runs_under_lock = false;
    promoted: std.ArrayList(u32) = .empty,
    refuse_slot: ?u32 = null,
    /// Set by the test while the sink runs: the entry must still be
    /// indexed (servable) at that moment.
    cache: ?*Cache(TestEntry, PromoteSink) = null,
    visible_during_sink: ?bool = null,

    pub fn onEvict(self: *PromoteSink, e: *const TestEntry, why: Why) Outcome {
        if (why != .deinit) {
            if (self.cache) |c| self.visible_during_sink = c.contains(e.key);
            if (self.refuse_slot == e.slot) return .kept;
        }
        self.promoted.append(testing.allocator, e.slot) catch unreachable;
        return .released;
    }
    fn deinit(self: *PromoteSink) void {
        self.promoted.deinit(testing.allocator);
    }
};

fn mk(ino: u64, off: u64, epoch: u64, bytes: u32, slot: u32) TestEntry {
    return .{ .key = .{ .ino = ino, .off = off }, .ino = ino, .epoch = epoch, .bytes = bytes, .slot = slot };
}

const Seen = struct {
    slots: std.ArrayList(u32) = .empty,
    fn visit(self: *Seen, e: *const TestEntry) void {
        self.slots.append(testing.allocator, e.slot) catch unreachable;
    }
};

test "tie: the bound is pct % of system RAM (100 GiB ⇒ 25 GiB at 25 %), scaled by the budget's ceiling below the solo 70 % (an 8 GiB cage budget ⇒ 2.86 GiB), 0 % off; shards halve to the min shard" {
    const gib = derive.GiB;
    try testing.expectEqual(@as(u64, 25 * gib), resolveBound(25, derive.Facts.synthetic(32, 100 * gib), 70 * gib));
    try testing.expectEqual(@as(u64, 8 * gib / 7 * 10 / 4), derivedBound(25, 100 * gib, 8 * gib));
    try testing.expectEqual(@as(u64, 0), derivedBound(0, 100 * gib, 70 * gib));
    try testing.expectEqual(@as(u64, 50 * gib), derivedBound(50, 100 * gib, 70 * gib));
    try testing.expectEqual(@as(u32, 16), shardsFor(1, 16 * derive.MiB, derive.MiB));
    try testing.expectEqual(@as(u32, 64), shardsFor(64, derive.GiB, derive.MiB));
    try testing.expectEqual(@as(u32, 4), shardsFor(64, 4 * derive.MiB, derive.MiB));
    try testing.expectEqual(@as(u32, 1), shardsFor(64, 0, derive.MiB));
}

test "the core (drop sink): exact LRU order under a counting seam — the tail is the least recently touched; a lookup touches; insert over the bound evicts the tail through the sink before the room is reused; `.exists` on a held key; stats" {
    var sink: DropSink = .{};
    defer sink.deinit();
    const C = Cache(TestEntry, DropSink);
    // ONE shard, bound 4 entries of 1 byte.
    const c = try C.init(testing.allocator, &sink, 4, 1, 8, null);
    defer c.deinit();
    for (1..5) |i| try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(7, i, 0, 1, @intCast(i)), .{}));
    try testing.expectEqual(@as(u64, 4), c.currentBytes());
    try testing.expectEqual(InsertOutcome.exists, c.insert(mk(7, 2, 0, 1, 99), .{}));
    // Touch 1 (the oldest) so the order MRU→LRU is 1, 4, 3, 2.
    var seen: Seen = .{};
    defer seen.slots.deinit(testing.allocator);
    try testing.expect(c.lookup(.{ .ino = 7, .off = 1 }, &seen, Seen.visit));
    try testing.expectEqualSlices(u32, &.{1}, seen.slots.items);
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(7, 5, 0, 1, 5), .{}));
    try testing.expectEqualSlices(u32, &.{2}, sink.released.items);
    try testing.expectEqual(Why.bound, sink.whys.items[0]);
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(7, 6, 0, 1, 6), .{}));
    try testing.expectEqualSlices(u32, &.{ 2, 3 }, sink.released.items);
    try testing.expect(c.contains(.{ .ino = 7, .off = 1 }));
    try testing.expect(!c.contains(.{ .ino = 7, .off = 3 }));
    // lookupMany: all-or-nothing, touched in order.
    seen.slots.clearRetainingCapacity();
    try testing.expectEqual(@as(usize, 2), c.lookupMany(&.{ .{ .ino = 7, .off = 4 }, .{ .ino = 7, .off = 1 } }, &seen, Seen.visit));
    try testing.expectEqualSlices(u32, &.{ 4, 1 }, seen.slots.items);
    seen.slots.clearRetainingCapacity();
    try testing.expectEqual(@as(usize, 1), c.lookupMany(&.{ .{ .ino = 7, .off = 4 }, .{ .ino = 7, .off = 3 } }, &seen, Seen.visit));
    try testing.expectEqual(@as(usize, 0), seen.slots.items.len);
    // Now MRU→LRU is 1, 4, 6, 5: the next victim is 5.
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(7, 8, 0, 1, 8), .{}));
    try testing.expectEqualSlices(u32, &.{ 2, 3, 5 }, sink.released.items);
    try testing.expectEqual(@as(u64, 2), c.stats.hits.load());
    try testing.expectEqual(@as(u64, 1), c.stats.misses.load());
    try testing.expectEqual(@as(u64, 7), c.stats.fills.load());
    try testing.expectEqual(@as(u64, 3), c.stats.evictions.load());
    try testing.expectEqual(@as(u64, 3), c.stats.sink_released.load());
    try testing.expectEqual(@as(u64, 4), c.stats.bytes.load());
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try c.stats.writeJson(&w, "t");
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"t_hits\":2") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"t_bound_bytes\":4") != null);
}

test "the core: epoch validation on lookup — an entry filled under epoch E is served while its inode's epoch reads E and REFUSED (evicted through the sink as stale) once it moved; a freeze insert is net-zero growth; the validation refuses at the visibility point" {
    var sink: DropSink = .{};
    defer sink.deinit();
    const Epochs = struct {
        var table: [16]u64 = @splat(0);
        fn of(_: ?*anyopaque, ino: u64) u64 {
            return table[ino];
        }
    };
    Epochs.table = @splat(0);
    const C = Cache(TestEntry, DropSink);
    const c = try C.init(testing.allocator, &sink, 8, 1, 8, .{ .call = &Epochs.of });
    defer c.deinit();
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(3, 0, 0, 1, 30), .{}));
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(3, 1, 0, 1, 31), .{}));
    var seen: Seen = .{};
    defer seen.slots.deinit(testing.allocator);
    try testing.expect(c.lookup(.{ .ino = 3, .off = 0 }, &seen, Seen.visit));
    // A write on ino 3 bumps its epoch: both entries are stale on their next lookup.
    Epochs.table[3] = 1;
    try testing.expect(!c.lookup(.{ .ino = 3, .off = 0 }, &seen, Seen.visit));
    try testing.expectEqual(@as(u64, 1), c.stats.stale_refusals.load());
    try testing.expectEqualSlices(u32, &.{30}, sink.released.items);
    try testing.expectEqual(Why.stale, sink.whys.items[0]);
    try testing.expect(!c.contains(.{ .ino = 3, .off = 0 }));
    try testing.expect(c.contains(.{ .ino = 3, .off = 1 }));
    // A fill under the new epoch serves.
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(3, 0, 1, 1, 32), .{}));
    try testing.expect(c.lookup(.{ .ino = 3, .off = 0 }, &seen, Seen.visit));
    try testing.expectEqualSlices(u32, &.{ 30, 32 }, seen.slots.items);
    // Freeze: at 2 bytes, a frozen 1-byte insert evicts one first.
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(4, 0, 0, 1, 40), .{ .freeze = true }));
    try testing.expectEqual(@as(u64, 2), c.currentBytes());
    try testing.expectEqual(@as(usize, 2), sink.released.items.len);
    // The validation refuses: nothing inserted.
    const No = struct {
        fn no(_: ?*anyopaque) bool {
            return false;
        }
    };
    try testing.expectEqual(InsertOutcome.refused, c.insert(mk(5, 0, 0, 1, 50), .{ .validate = .{ .call = &No.no } }));
    try testing.expect(!c.contains(.{ .ino = 5, .off = 0 }));
    try testing.expectEqual(@as(u64, 1), c.stats.insert_refusals.load());
}

test "the core (promote sink): the sink runs OUTSIDE the lock with the entry still indexed and servable, BEFORE its memory is released; a `.kept` answer keeps the entry (relinked at the head) and the pass backs off; invalidate(ino) and a ranged invalidate go through the sink; evictUntil and shed; the component's shed" {
    var sink: PromoteSink = .{};
    defer sink.deinit();
    const C = Cache(TestEntry, PromoteSink);
    const c = try C.init(testing.allocator, &sink, 4, 1, 8, null);
    defer c.deinit();
    sink.cache = c;
    for (1..5) |i| try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(9, i * 4096, 0, 1, @intCast(i)), .{}));
    // The 5th insert evicts slot 1 (the tail): the sink saw it indexed.
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(9, 5 * 4096, 0, 1, 5), .{}));
    try testing.expectEqualSlices(u32, &.{1}, sink.promoted.items);
    try testing.expectEqual(@as(?bool, true), sink.visible_during_sink);
    try testing.expect(!c.contains(.{ .ino = 9, .off = 4096 }));
    // The sink refuses slot 2 (the next tail): the insert backs off — 2
    // stays (now at the head), the insert found no room and refused.
    sink.refuse_slot = 2;
    try testing.expectEqual(InsertOutcome.refused, c.insert(mk(9, 6 * 4096, 0, 1, 6), .{}));
    try testing.expect(c.contains(.{ .ino = 9, .off = 2 * 4096 }));
    try testing.expectEqual(@as(u64, 1), c.stats.sink_kept.load());
    try testing.expectEqual(@as(u64, 4), c.currentBytes());
    // With the refusal lifted the next victim is 3 (2 moved to the head).
    sink.refuse_slot = null;
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(9, 6 * 4096, 0, 1, 6), .{}));
    try testing.expectEqualSlices(u32, &.{ 1, 3 }, sink.promoted.items);
    // A ranged invalidate on ino 9: offsets in [4 × 4096, 6 × 4096) = slots 4, 5.
    const Range = struct {
        fn pick(_: *const u8, e: *const TestEntry) bool {
            return e.key.off >= 4 * 4096 and e.key.off < 6 * 4096;
        }
    };
    const dummy: u8 = 0;
    const p = c.invalidateIno(9, &dummy, Range.pick);
    try testing.expectEqual(@as(u32, 2), p.released);
    try testing.expectEqual(@as(u32, 0), p.kept);
    try testing.expect(!c.contains(.{ .ino = 9, .off = 4 * 4096 }) and !c.contains(.{ .ino = 9, .off = 5 * 4096 }));
    try testing.expect(c.contains(.{ .ino = 9, .off = 2 * 4096 }) and c.contains(.{ .ino = 9, .off = 6 * 4096 }));
    try testing.expectEqual(@as(usize, 4), sink.promoted.items.len);
    // invalidate(ino) for the rest; another ino untouched.
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(10, 0, 0, 1, 100), .{}));
    const all = c.invalidateIno(9, &dummy, null);
    try testing.expectEqual(@as(u32, 2), all.released);
    try testing.expect(c.contains(.{ .ino = 10, .off = 0 }));
    try testing.expectEqual(@as(u64, 1), c.currentBytes());
    try testing.expectEqual(@as(u64, 4), c.stats.invalidations.load());
    // evictUntil / shed through the sink; the component's shed is the same.
    for (1..4) |i| try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(11, i, 0, 1, @intCast(200 + i)), .{}));
    try testing.expectEqual(@as(u64, 4), c.currentBytes());
    _ = c.shed(2);
    try testing.expectEqual(@as(u64, 2), c.currentBytes());
    const comp = c.component("test_cache", 64 * derive.MiB, 4, .{ .gauge = &c.stats.bytes }, null);
    try testing.expectEqualStrings("test_cache", comp.name);
    try testing.expectEqual(@as(u64, 4), comp.floor);
    comp.shed.?.call(comp.shed.?.ctx, 0);
    try testing.expectEqual(@as(u64, 0), c.currentBytes());
    try testing.expectEqual(@as(u64, 0), comp.current());
}

test "the core: invalidateKey and invalidateKeys drop held keys through the sink and count; an absent key is nothing; a disabled instance (bound 0) misses, refuses and invalidates nothing" {
    var sink: DropSink = .{};
    defer sink.deinit();
    const C = Cache(TestEntry, DropSink);
    const c = try C.init(testing.allocator, &sink, 16, 2, 8, null);
    defer c.deinit();
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(1, 0, 0, 2, 10), .{}));
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(1, 1, 0, 2, 11), .{}));
    try testing.expectEqual(InsertOutcome.inserted, c.insert(mk(2, 0, 0, 2, 20), .{}));
    try testing.expect(c.invalidateKey(.{ .ino = 1, .off = 0 }));
    try testing.expect(!c.invalidateKey(.{ .ino = 1, .off = 0 }));
    try testing.expectEqual(@as(u32, 1), c.invalidateKeys(c.shardFor(.{ .ino = 1, .off = 1 }), &.{ .{ .ino = 1, .off = 1 }, .{ .ino = 1, .off = 7 } }));
    try testing.expectEqualSlices(u32, &.{ 10, 11 }, sink.released.items);
    try testing.expectEqual(Why.invalidate, sink.whys.items[1]);
    try testing.expectEqual(@as(u64, 2), c.currentBytes());
    const off = try C.init(testing.allocator, &sink, 0, 1, 1, null);
    defer off.deinit();
    try testing.expect(!off.enabled());
    try testing.expectEqual(InsertOutcome.refused, off.insert(mk(1, 0, 0, 1, 1), .{}));
    try testing.expect(!off.contains(.{ .ino = 1, .off = 0 }));
    try testing.expect(!off.invalidateKey(.{ .ino = 1, .off = 0 }));
}
