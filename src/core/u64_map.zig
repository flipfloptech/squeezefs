//! A map from u64 keys whose removals leave no tombstones: open addressing,
//! linear probing, backward-shift deletion (Knuth TAOCP 6.4, Algorithm R).
//!
//! Why not `std.HashMapUnmanaged`: its removal marks the slot a tombstone,
//! counts it available again and so never grows or rehashes past it; under
//! a churn of FRESH keys (inos are never reused) a miss — every insert of a
//! new key — walks to the first FREE slot, and the free slots only drain:
//! the probe length rises with the mount's history, not with its live
//! population (decision 0003 §CU). Here a removal pulls the cluster behind
//! the hole back over it, so the table only ever holds live entries and
//! empty slots and a probe's length is a function of the live load alone
//! (≤ `MAX_LOAD` by growth).
//!
//! Not thread-safe: the owner's lock covers every call. Nothing allocates
//! except a growth (doubling, amortized; a refused growth is the caller's
//! `error.OutOfMemory` and leaves the map intact). The capacity follows the
//! peak live count and never shrinks. Room is `std.HashMapUnmanaged`'s:
//! `ensureUnusedCapacity(n)` lets the next `n` new keys in with no
//! allocation (the `*AssumeCapacity` faces), a removal returns one. An
//! iteration sees every entry once only if nothing is inserted or removed
//! during it (a removal shifts the entries behind it back): collect, then
//! mutate.

const std = @import("std");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

/// The empty slot's key. A real key 0 lives in the side slot (`zero`).
const EMPTY: u64 = 0;

/// Grow before the table passes 3/4 live: linear probing's expected miss
/// is ½(1 + 1/(1−α)²) probes — 8.5 at α = 3/4, 2.5 at the 3/8 just after a
/// doubling (Knuth 6.4 (K)).
pub const MAX_LOAD_NUM = 3;
pub const MAX_LOAD_DEN = 4;
/// The first table: 16 slots (12 entries before the first doubling).
pub const MIN_SLOTS = 16;

/// What a full scan of the table finds (`census`; O(capacity) — the
/// tests' and the evidence rigs', never an op's).
pub const Census = struct {
    entries: u64 = 0,
    slots: u64 = 0,
    /// Σ over the table's entries of the distance from the home slot (a
    /// hit probes `distance + 1` slots).
    displacement: u64 = 0,
    max_displacement: u64 = 0,
    /// Σ over every start slot of the slots a miss starting there reads (to
    /// the first empty slot, inclusive): the mean miss is `/ slots`.
    miss_probes: u64 = 0,
};

pub fn U64Map(comptime V: type) type {
    return struct {
        const Self = @This();

        /// `EMPTY` or a key; `values[i]` is defined iff `keys[i] != EMPTY`.
        keys: []u64 = &.{},
        values: []V = &.{},
        /// Table entries (the side slot not counted).
        used: usize = 0,
        /// Σ distance from home over the table's entries — kept exact by
        /// every insert, shift and growth (the mean hit probe is
        /// `1 + displacement / used`).
        displacement: u64 = 0,
        /// The key 0 (no slot can hold it: 0 marks an empty slot).
        zero: ?V = null,

        pub const empty: Self = .{};

        pub const GetOrPutResult = struct { found_existing: bool, value_ptr: *V };
        pub const Entry = struct { key: u64, value_ptr: *V };

        pub fn deinit(self: *Self, gpa: Allocator) void {
            gpa.free(self.keys);
            gpa.free(self.values);
            self.* = undefined;
        }

        pub fn count(self: *const Self) usize {
            return self.used + @intFromBool(self.zero != null);
        }

        pub fn capacity(self: *const Self) usize {
            return self.keys.len;
        }

        inline fn mask(self: *const Self) usize {
            return self.keys.len - 1;
        }

        inline fn home(self: *const Self, k: u64) usize {
            return @intCast(std.hash.int(k) & self.mask());
        }

        inline fn distance(self: *const Self, from: usize, to: usize) usize {
            return (to -% from) & self.mask();
        }

        /// The slot holding `k`, or null. A miss stops at the first empty
        /// slot: the table always keeps one (the load bound).
        fn find(self: *const Self, k: u64) ?usize {
            assert(k != EMPTY);
            if (self.used == 0) return null;
            var i = self.home(k);
            while (true) : (i = (i + 1) & self.mask()) {
                const at = self.keys[i];
                if (at == k) return i;
                if (at == EMPTY) return null;
            }
        }

        pub fn get(self: *const Self, k: u64) ?V {
            if (k == EMPTY) return self.zero;
            const i = self.find(k) orelse return null;
            return self.values[i];
        }

        pub fn getPtr(self: *Self, k: u64) ?*V {
            if (k == EMPTY) return if (self.zero) |*z| z else null;
            const i = self.find(k) orelse return null;
            return &self.values[i];
        }

        pub fn contains(self: *const Self, k: u64) bool {
            return self.get(k) != null;
        }

        pub fn put(self: *Self, gpa: Allocator, k: u64, v: V) Allocator.Error!void {
            const r = try self.getOrPut(gpa, k);
            r.value_ptr.* = v;
        }

        /// `put` of a key the caller knows absent.
        pub fn putNoClobber(self: *Self, gpa: Allocator, k: u64, v: V) Allocator.Error!void {
            const r = try self.getOrPut(gpa, k);
            assert(!r.found_existing);
            r.value_ptr.* = v;
        }

        /// Room for `n` more new keys with no allocation.
        pub fn ensureUnusedCapacity(self: *Self, gpa: Allocator, n: usize) Allocator.Error!void {
            return self.ensureTotalCapacity(gpa, self.used + n);
        }

        /// Room for `n` entries in all with no allocation.
        pub fn ensureTotalCapacity(self: *Self, gpa: Allocator, n: usize) Allocator.Error!void {
            if (self.fits(n)) return;
            var slots: usize = @max(MIN_SLOTS, self.keys.len * 2);
            while (slots * MAX_LOAD_NUM < n * MAX_LOAD_DEN) slots *= 2;
            return self.growTo(gpa, slots);
        }

        inline fn fits(self: *const Self, entries: usize) bool {
            return entries * MAX_LOAD_DEN <= self.keys.len * MAX_LOAD_NUM;
        }

        /// `getOrPut` within the room `ensureUnusedCapacity` made.
        pub fn getOrPutAssumeCapacity(self: *Self, k: u64) GetOrPutResult {
            if (k == EMPTY) {
                const found = self.zero != null;
                if (!found) self.zero = @as(V, undefined);
                return .{ .found_existing = found, .value_ptr = &self.zero.? };
            }
            if (self.find(k)) |i| return .{ .found_existing = true, .value_ptr = &self.values[i] };
            assert(self.fits(self.used + 1));
            return .{ .found_existing = false, .value_ptr = &self.values[self.place(k)] };
        }

        pub fn putAssumeCapacityNoClobber(self: *Self, k: u64, v: V) void {
            const r = self.getOrPutAssumeCapacity(k);
            assert(!r.found_existing);
            r.value_ptr.* = v;
        }

        /// A found key's value, else a new slot (value undefined) — the
        /// growth, the only allocation, only for a key not present.
        pub fn getOrPut(self: *Self, gpa: Allocator, k: u64) Allocator.Error!GetOrPutResult {
            if (k != EMPTY and !self.fits(self.used + 1) and self.find(k) == null) try self.growTo(gpa, @max(MIN_SLOTS, self.keys.len * 2));
            return self.getOrPutAssumeCapacity(k);
        }

        /// The first empty slot from `k`'s home (k absent, room assured).
        fn place(self: *Self, k: u64) usize {
            const h = self.home(k);
            var i = h;
            while (self.keys[i] != EMPTY) i = (i + 1) & self.mask();
            self.keys[i] = k;
            self.used += 1;
            self.displacement += self.distance(h, i);
            return i;
        }

        fn growTo(self: *Self, gpa: Allocator, slots: usize) Allocator.Error!void {
            assert(std.math.isPowerOfTwo(slots) and slots > self.keys.len);
            const keys = try gpa.alloc(u64, slots);
            errdefer gpa.free(keys);
            const values = try gpa.alloc(V, slots);
            @memset(keys, EMPTY);
            const old_keys = self.keys;
            const old_values = self.values;
            self.keys = keys;
            self.values = values;
            self.used = 0;
            self.displacement = 0;
            for (old_keys, old_values) |k, v| {
                if (k != EMPTY) self.values[self.place(k)] = v;
            }
            gpa.free(old_keys);
            gpa.free(old_values);
        }

        /// True iff `k` was present. The hole is refilled from the cluster
        /// behind it: an entry moves back iff its home does not lie
        /// cyclically in (hole, its slot] — so every remaining entry stays
        /// reachable from its home without crossing an empty slot.
        pub fn remove(self: *Self, k: u64) bool {
            if (k == EMPTY) {
                const had = self.zero != null;
                self.zero = null;
                return had;
            }
            var hole = self.find(k) orelse return false;
            self.displacement -= self.distance(self.home(k), hole);
            self.used -= 1;
            var j = hole;
            while (true) {
                j = (j + 1) & self.mask();
                const at = self.keys[j];
                if (at == EMPTY) break;
                const h = self.home(at);
                // h in (hole, j] cyclically ⇔ distance(h, j) < distance(hole, j).
                if (self.distance(h, j) < self.distance(hole, j)) continue;
                self.keys[hole] = at;
                self.values[hole] = self.values[j];
                self.displacement -= self.distance(hole, j);
                hole = j;
            }
            self.keys[hole] = EMPTY;
            self.values[hole] = undefined;
            return true;
        }

        /// Every entry once (nothing inserted or removed meanwhile).
        pub fn iterator(self: *Self) Iterator {
            return .{ .map = self };
        }

        pub const Iterator = struct {
            map: *Self,
            index: usize = 0,
            zero_done: bool = false,

            pub fn next(it: *Iterator) ?Entry {
                while (it.index < it.map.keys.len) {
                    const i = it.index;
                    it.index += 1;
                    if (it.map.keys[i] != EMPTY) return .{ .key = it.map.keys[i], .value_ptr = &it.map.values[i] };
                }
                if (it.zero_done) return null;
                it.zero_done = true;
                return if (it.map.zero) |*z| .{ .key = EMPTY, .value_ptr = z } else null;
            }
        };

        /// The full scan (O(capacity)).
        pub fn census(self: *const Self) Census {
            var c: Census = .{ .entries = self.count(), .slots = self.keys.len };
            if (self.keys.len == 0) return c;
            var first_empty: ?usize = null;
            for (self.keys, 0..) |k, i| {
                if (k == EMPTY) {
                    if (first_empty == null) first_empty = i;
                    continue;
                }
                const d = self.distance(self.home(k), i);
                c.displacement += d;
                c.max_displacement = @max(c.max_displacement, d);
            }
            // Backwards from an empty slot: a start slot's miss reads the
            // run up to the next empty slot, inclusive.
            var i = first_empty.?;
            var run: u64 = 0;
            for (0..self.keys.len) |_| {
                run = if (self.keys[i] == EMPTY) 1 else run + 1;
                c.miss_probes += run;
                i = (i + self.keys.len - 1) & self.mask();
            }
            return c;
        }
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

/// Every entry reachable from its home without crossing an empty slot (the
/// lookup's law), the incremental displacement equal to the scan's.
fn expectSound(comptime V: type, m: *const U64Map(V)) !void {
    const c = m.census();
    try testing.expectEqual(c.displacement, m.displacement);
    var used: usize = 0;
    for (m.keys, 0..) |k, i| {
        if (k == EMPTY) continue;
        used += 1;
        var j = m.home(k);
        while (j != i) : (j = (j + 1) & m.mask()) try testing.expect(m.keys[j] != EMPTY);
    }
    try testing.expectEqual(m.used, used);
}

test "churn of fresh keys: K rounds of N inserts then N removes leave the table empty of anything but empty slots — the miss probe after each removal phase is 1, the peak miss stays under the load bound's 8.5, the capacity is the peak's" {
    const gpa = testing.allocator;
    var m: U64Map(u64) = .empty;
    defer m.deinit(gpa);
    const n: u64 = 20_000;
    var next: u64 = 2;
    for (0..12) |_| {
        const base = next;
        for (0..n) |_| {
            try m.put(gpa, next, next ^ 0x5a);
            next += 1;
        }
        const peak = m.census();
        try testing.expectEqual(n, peak.entries);
        // 20,000 live at ≤ 3/4: 32,768 slots, α ≈ 0.61, ½(1 + 1/(1−α)²) ≈ 3.8.
        try testing.expectEqual(@as(u64, 32_768), peak.slots);
        try testing.expect(peak.miss_probes <= peak.slots * 17 / 2);
        try expectSound(u64, &m);
        var k = base;
        while (k < next) : (k += 1) try testing.expect(m.remove(k));
        const after = m.census();
        try testing.expectEqual(@as(u64, 0), after.entries);
        try testing.expectEqual(@as(u64, 0), after.displacement);
        try testing.expectEqual(after.slots, after.miss_probes);
    }
}

test "property: lookups stay exact through random churn (inserts of fresh and present keys, removes of present and absent keys, the key 0) against a reference map; the probe law and the displacement hold throughout" {
    const gpa = testing.allocator;
    for ([_]u64{ 1, 0x5eed, 0xdead_beef, 42 }) |seed| {
        var prng = std.Random.DefaultPrng.init(seed);
        const r = prng.random();
        var m: U64Map(u32) = .empty;
        defer m.deinit(gpa);
        var ref: std.AutoHashMapUnmanaged(u64, u32) = .empty;
        defer ref.deinit(gpa);
        var live: std.ArrayList(u64) = .empty;
        defer live.deinit(gpa);
        var next: u64 = 1;
        for (0..40_000) |step| {
            switch (r.uintLessThan(u8, 10)) {
                // A fresh key (a window of clustered and scattered keys).
                0...3 => {
                    const k = if (r.boolean()) next else r.int(u64) | 1;
                    next += 1 + r.uintLessThan(u64, 3);
                    const v = r.int(u32);
                    try m.put(gpa, k, v);
                    const g = try ref.getOrPut(gpa, k);
                    if (!g.found_existing) try live.append(gpa, k);
                    g.value_ptr.* = v;
                },
                // A present key's value replaced.
                4 => if (live.items.len > 0) {
                    const k = live.items[r.uintLessThan(usize, live.items.len)];
                    const v = r.int(u32);
                    const g = try m.getOrPut(gpa, k);
                    try testing.expect(g.found_existing);
                    g.value_ptr.* = v;
                    ref.getPtr(k).?.* = v;
                },
                // A present key removed.
                5...7 => if (live.items.len > 0) {
                    const at = r.uintLessThan(usize, live.items.len);
                    const k = live.swapRemove(at);
                    try testing.expect(m.remove(k));
                    try testing.expect(ref.remove(k));
                },
                // An absent key removed; the key 0 in and out.
                8 => {
                    const k = r.int(u64) | 1;
                    if (!ref.contains(k)) try testing.expect(!m.remove(k));
                    if (r.boolean()) {
                        try m.put(gpa, 0, 7);
                        try testing.expectEqual(@as(?u32, 7), m.get(0));
                        try testing.expect(m.remove(0));
                    }
                    try testing.expectEqual(@as(?u32, null), m.get(0));
                },
                // Lookups: a present key, an absent key.
                else => {
                    if (live.items.len > 0) {
                        const k = live.items[r.uintLessThan(usize, live.items.len)];
                        try testing.expectEqual(ref.get(k), m.get(k));
                    }
                    const k = r.int(u64) | 1;
                    try testing.expectEqual(ref.get(k), m.get(k));
                },
            }
            try testing.expectEqual(@as(usize, ref.count()), m.count());
            if (step % 4096 == 0) try expectSound(u32, &m);
        }
        var it = ref.iterator();
        while (it.next()) |e| try testing.expectEqual(e.value_ptr.*, m.get(e.key_ptr.*).?);
        try expectSound(u32, &m);
        // The iterator: every entry once, each value the reference's.
        var seen: usize = 0;
        var mi = m.iterator();
        while (mi.next()) |e| {
            seen += 1;
            try testing.expectEqual(ref.get(e.key).?, e.value_ptr.*);
        }
        try testing.expectEqual(@as(usize, ref.count()), seen);
    }
}

test "room: ensureUnusedCapacity(n) admits new keys through the AssumeCapacity faces with no allocation up to the load bound, a removal returns one key's room; the iterator yields the key 0 too" {
    var failing = std.testing.FailingAllocator.init(testing.allocator, .{ .fail_index = 2 });
    const gpa = failing.allocator();
    var m: U64Map(u64) = .empty;
    defer m.deinit(gpa);
    // One growth (two allocations) straight to the size: 100 at ≤ 3/4.
    try m.ensureUnusedCapacity(gpa, 100);
    const slots = m.capacity();
    try testing.expectEqual(@as(usize, 256), slots);
    const room = slots * MAX_LOAD_NUM / MAX_LOAD_DEN;
    for (1..room + 1) |k| m.putAssumeCapacityNoClobber(k, k * 2);
    try testing.expectEqual(slots, m.capacity());
    // At the bound the next new key needs a growth (refused here); a
    // present key needs none.
    try testing.expectError(error.OutOfMemory, m.put(gpa, 1_000, 7));
    try m.put(gpa, 1, 2);
    try testing.expect(m.remove(50));
    const g = m.getOrPutAssumeCapacity(1_000);
    try testing.expect(!g.found_existing);
    g.value_ptr.* = 7;
    // The key 0 needs no room.
    m.putAssumeCapacityNoClobber(0, 9);
    try testing.expectEqual(room + 1, m.count());
    var sum: u64 = 0;
    var n: usize = 0;
    var it = m.iterator();
    while (it.next()) |e| {
        n += 1;
        sum += e.key;
        try testing.expectEqual(if (e.key == 1_000) 7 else if (e.key == 0) 9 else e.key * 2, e.value_ptr.*);
    }
    try testing.expectEqual(room + 1, n);
    try testing.expectEqual(@as(u64, room * (room + 1) / 2 - 50 + 1_000), sum);
    try expectSound(u64, &m);
    // ensureTotalCapacity within the room asks for nothing.
    try m.ensureTotalCapacity(gpa, 10);
    try testing.expectEqual(slots, m.capacity());
}

test "a refused growth leaves the map intact (the caller's memo drop)" {
    var failing = std.testing.FailingAllocator.init(testing.allocator, .{ .fail_index = 2 });
    const gpa = failing.allocator();
    var m: U64Map(u64) = .empty;
    defer m.deinit(gpa);
    for (2..14) |k| try m.put(gpa, k, k);
    try testing.expectEqual(@as(usize, MIN_SLOTS), m.capacity());
    try testing.expectError(error.OutOfMemory, m.put(gpa, 99, 99));
    try testing.expectEqual(@as(usize, 12), m.count());
    for (2..14) |k| try testing.expectEqual(@as(?u64, k), m.get(k));
    try testing.expectEqual(@as(?u64, null), m.get(99));
    try expectSound(u64, &m);
}
