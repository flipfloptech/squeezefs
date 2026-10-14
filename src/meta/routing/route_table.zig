//! The slot → volume map (charter §5.4 "'Dynamic' = the slot→volume map
//! is the only thing that moves"; `docs/porting/dynamic_routing.md` §1.2,
//! §2.1, §7 Q16–Q18). PORTABLE: arithmetic over the frozen width and an
//! immutable 128 KiB table published through `sync/arc_swap`.
//!
//! Three laws live here:
//!
//! - **W is frozen.** `W = 2^16` is the u16 slot-id wire type (`inos.
//!   ROUTING_WIDTH`), baked into every global ino; the table has exactly W
//!   entries and names a volume INDEX (u16: the width bounds the member
//!   count) per slot.
//! - **The effective-ino law is a pure function of the global ino.** Slot
//!   0's records are the native keyspace of slot 0's host (`ledger.
//!   nativeSlotOf`: the root pin's member); every other slot's records are
//!   its guest keyspace `(slot + 1) << 40 | local` wherever the slot lives.
//!   So `effectiveIno(global)` needs NO table, a parked op's keys stay
//!   valid across a flip, and a key alone names its home: `key_ino >> 40`
//!   → the slot → `slot_to_volume`. The reference's per-volume
//!   `legacy_slot` test (Q18's two code paths) does not exist.
//! - **A publish is whole and validated.** `validate` refuses every
//!   malformed map with the reference's texts; the cutover gate guarantees
//!   no admitted mutation straddles a swap.

const std = @import("std");
const Allocator = std.mem.Allocator;
const key = @import("../key.zig");
const inos = @import("../inos.zig");
const ledger = @import("../ledger.zig");
const arc_swap = @import("../../sync/arc_swap.zig");

const assert = std.debug.assert;

/// The frozen routing width (`inos.ROUTING_WIDTH`), as the table's length.
pub const W: usize = inos.ROUTING_WIDTH;
/// The RAM table: W entries of a u16 volume index — 128 KiB per mount
/// (the design's own figure; the reference's `Vec<usize>` was 512 KiB).
pub const TABLE_BYTES: usize = W * @sizeOf(u16);

comptime {
    assert(W == 65_536);
    assert(TABLE_BYTES == 128 * 1024);
    assert(W <= @as(usize, std.math.maxInt(u16)) + 1);
}

/// The volume index type: the width bounds the member count at W.
pub const VolumeIndex = u16;

/// The members one MOUNT holds: the `sqmeta://` URI names at most 64 (one
/// argv word of 64 × PATH_MAX is past any shell — `cli/uri.MAX_MEMBERS`),
/// so the per-mint health vector and the per-op scratch live on the stack
/// at this size; a larger set is refused at the URI, never here.
pub const MAX_VOLUMES: usize = 64;

// ---------------------------------------------------------------------------
// The effective-ino law (table-free).
// ---------------------------------------------------------------------------

/// A global ino's routing slot (`(ino − 2) % W`; ino 1 → slot 0).
pub fn slotOf(global: u64) u16 {
    return @intCast(inos.routeIno(global, inos.ROUTING_WIDTH).slot);
}

/// The key ino a global ino's records are stored under: slot 0 → the raw
/// local (the native keyspace of slot 0's host); slot `s ≠ 0` → the guest
/// keyspace `guestLocalIno(s, local)`. The caller guarantees a ROUTABLE
/// ino (`isRoutable`): a minted ino's local is below the guest namespace
/// by construction.
pub fn effectiveIno(global: u64) u64 {
    const r = inos.routeIno(global, inos.ROUTING_WIDTH);
    if (r.slot == 0) return r.local;
    return key.guestLocalIno(@intCast(r.slot), r.local);
}

/// Whether a kernel nodeid lies in the space the mint produces (ino ≥ 1,
/// local below `key.GUEST_NS_BASE`): the virtual control inodes (`2⁶⁴ −
/// 2³² …`), nodeid 0 and a corrupt nodeid are not, and must never reach
/// the law — a transport-side probe declines them, a lane answers ENOENT.
pub fn isRoutable(global: u64) bool {
    if (global == 0) return false;
    return inos.routeIno(global, inos.ROUTING_WIDTH).local < key.GUEST_NS_BASE;
}

/// `effectiveIno`'s inverse: a guest key ino splits into `(slot, local)`;
/// a native one is slot 0's local.
pub fn globalOfEffective(eff: u64) u64 {
    if (key.splitGuestLocal(eff)) |g| return inos.makeGlobalIno(g.local, g.slot, inos.ROUTING_WIDTH);
    return inos.makeGlobalIno(eff, 0, inos.ROUTING_WIDTH);
}

/// `globalOfEffective` for an ino the KERNEL can know (decision 0003
/// §AB.2's invalidations): null for a reserved local below the mint base
/// (a control object no FUSE reply names), the root excepted.
pub fn kernelGlobalOf(eff: u64) ?u64 {
    const local = if (key.splitGuestLocal(eff)) |g| g.local else eff;
    if (local != inos.ROOT_INO and local < inos.LOCAL_INO_BASE) return null;
    return globalOfEffective(eff);
}

/// The routing slot of a key ino (native ⇒ slot 0; guest ⇒ its slot).
pub fn slotOfEffective(eff: u64) u16 {
    if (key.splitGuestLocal(eff)) |g| return g.slot;
    return 0;
}

/// The forest slot (the tree identity inside a volume) of a routing slot:
/// slot 0 is the native tree (`key.NATIVE_FOREST_SLOT`); slot `s` is the
/// guest tree `s + 1`.
pub fn forestSlotOf(slot: u16) key.ForestSlot {
    return if (slot == 0) key.NATIVE_FOREST_SLOT else key.guestForestSlot(slot);
}

/// The routing slot a forest slot names (the inverse of `forestSlotOf`).
pub fn slotOfForestSlot(fs: key.ForestSlot) u16 {
    assert(fs <= key.FOREST_SLOT_MAX);
    return if (fs == key.NATIVE_FOREST_SLOT) 0 else @intCast(fs - 1);
}

/// The routing slot a slot-tree KEY belongs to (refs by their owner ino;
/// `key.slotTreeKeySlot` is the ONE codec).
pub fn slotOfKey(k: []const u8) key.ForestKeyError!u16 {
    return slotOfForestSlot(try key.slotTreeKeySlot(k));
}

// ---------------------------------------------------------------------------
// Validation (the reference's `validate_slot_map`, every text verbatim).
// ---------------------------------------------------------------------------

pub const Refusal = union(enum) {
    no_volumes,
    width_below_volumes: struct { volumes: u32 },
    wrong_length: struct { len: usize },
    volume_out_of_range: struct { slot: u32, volume: u32, volumes: u32 },
    hostless_volume: struct { volume: u32 },
    /// Z/'s own law: slot 0's host is the native member; the map must put
    /// slot 0 on the volume whose stamp carries `native_slot = 0`.
    native_not_slot_zero_host: struct { native_volume: u32, slot_zero_volume: u32 },

    pub fn write(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.writeAll("metadata slot map invalid: ");
        switch (self) {
            .no_volumes => try w.writeAll("no metadata volumes"),
            .width_below_volumes => |r| try w.print("routing width {d} below the volume count {d}", .{ W, r.volumes }),
            .wrong_length => |r| try w.print("map covers {d} slots but the frozen routing width is {d}", .{ r.len, W }),
            .volume_out_of_range => |r| try w.print("slot {d} maps to volume index {d} of {d}", .{ r.slot, r.volume, r.volumes }),
            .hostless_volume => |r| try w.print("volume index {d} hosts no slot — every member must host at least one (its native mint slot)", .{r.volume}),
            .native_not_slot_zero_host => |r| try w.print("volume index {d} carries the native keyspace but slot 0 (the root pin) maps to volume index {d} — a v4 set keeps the native keyspace with slot 0's host", .{ r.native_volume, r.slot_zero_volume }),
        }
    }
};

pub const Error = error{Refused};

/// Refuse a malformed map (the order of the reference's checks). `native`
/// is the volume whose stamp carries a native slot, if any: it must be
/// slot 0's host.
pub fn validate(volumes: u32, map: []const VolumeIndex, native: ?VolumeIndex, diag: *Refusal) Error!void {
    if (volumes == 0) {
        diag.* = .no_volumes;
        return error.Refused;
    }
    if (W < volumes) {
        diag.* = .{ .width_below_volumes = .{ .volumes = volumes } };
        return error.Refused;
    }
    if (map.len != W) {
        diag.* = .{ .wrong_length = .{ .len = map.len } };
        return error.Refused;
    }
    var hosted: std.StaticBitSet(W) = .empty;
    for (map, 0..) |v, s| {
        if (v >= volumes) {
            diag.* = .{ .volume_out_of_range = .{ .slot = @intCast(s), .volume = v, .volumes = volumes } };
            return error.Refused;
        }
        hosted.set(v);
    }
    var v: u32 = 0;
    while (v < volumes) : (v += 1) {
        if (!hosted.isSet(v)) {
            diag.* = .{ .hostless_volume = .{ .volume = v } };
            return error.Refused;
        }
    }
    if (native) |n| if (n != map[0]) {
        diag.* = .{ .native_not_slot_zero_host = .{ .native_volume = n, .slot_zero_volume = map[0] } };
        return error.Refused;
    };
}

// ---------------------------------------------------------------------------
// The table.
// ---------------------------------------------------------------------------

/// The immutable snapshot a mount routes through. Built once from the
/// discovered map (validated), copied-and-patched at a flip, published
/// whole through `Published`.
pub const RouteTable = struct {
    slot_to_volume: [W]VolumeIndex,
    volumes: u16,

    /// A validated map (the caller ran `validate`).
    pub fn init(volumes: u16, map: *const [W]VolumeIndex) RouteTable {
        return .{ .slot_to_volume = map.*, .volumes = volumes };
    }

    /// The identity distribution of a fresh set: slot `s → s mod V`
    /// (`ledger.memberRun`'s stride runs read back).
    pub fn identity(volumes: u16) RouteTable {
        assert(volumes >= 1);
        var t: RouteTable = .{ .slot_to_volume = undefined, .volumes = volumes };
        for (&t.slot_to_volume, 0..) |*v, s| v.* = @intCast(s % volumes);
        return t;
    }

    pub fn volumeOf(self: *const RouteTable, slot: u16) VolumeIndex {
        return self.slot_to_volume[slot];
    }

    /// The home of a key ino: slot 0's host for the native keyspace, the
    /// guest slot's host otherwise.
    pub fn volumeOfEffective(self: *const RouteTable, eff: u64) VolumeIndex {
        return self.volumeOf(slotOfEffective(eff));
    }

    pub fn volumeOfGlobal(self: *const RouteTable, global: u64) VolumeIndex {
        return self.volumeOf(slotOf(global));
    }

    /// `self` with slot `s` re-homed to `volume` (the flip's RAM swap).
    pub fn withSlot(self: *const RouteTable, slot: u16, volume: VolumeIndex) RouteTable {
        assert(volume < self.volumes);
        var t = self.*;
        t.slot_to_volume[slot] = volume;
        return t;
    }

    pub fn hostedCount(self: *const RouteTable, volume: VolumeIndex) u32 {
        var n: u32 = 0;
        for (self.slot_to_volume) |v| n += @intFromBool(v == volume);
        return n;
    }

    /// The first `out.len` hosted slots of `volume`, ascending — the mint
    /// set's input (`router.MintRotor.init`).
    pub fn hostedAscending(self: *const RouteTable, volume: VolumeIndex, out: []u16) []u16 {
        var n: usize = 0;
        for (self.slot_to_volume, 0..) |v, s| {
            if (n == out.len) break;
            if (v == volume) {
                out[n] = @intCast(s);
                n += 1;
            }
        }
        return out[0..n];
    }

    /// Every hosted slot of `volume`, ascending (caller frees).
    pub fn hostedSlots(self: *const RouteTable, gpa: Allocator, volume: VolumeIndex) Allocator.Error![]u16 {
        const out = try gpa.alloc(u16, self.hostedCount(volume));
        _ = self.hostedAscending(volume, out);
        return out;
    }

    /// The hosted set of `volume` as stride runs — the stamp's spelling
    /// (`coalesceRuns`).
    pub fn runsOf(self: *const RouteTable, gpa: Allocator, volume: VolumeIndex) Allocator.Error![]ledger.SlotRun {
        const slots = try self.hostedSlots(gpa, volume);
        defer gpa.free(slots);
        return coalesceRuns(gpa, slots);
    }

    /// Apply every member's hosted set to a blank map: `slot → member`
    /// for every slot a member's runs name. Null entries are unclaimed
    /// slots (discovery's refusal 8).
    pub fn fillFromRuns(map: *[W]?VolumeIndex, volume: VolumeIndex, runs: []const ledger.SlotRun) void {
        for (runs) |r| {
            var k: u32 = 0;
            while (k < r.count) : (k += 1) {
                const s: usize = @intCast(@as(u64, r.start) + @as(u64, k) * @as(u64, r.stride));
                map[s] = volume;
            }
        }
    }
};

/// Arithmetic-progression coalescing of an ascending slot list: a run
/// opens at the first unconsumed slot with the gap to its successor as the
/// stride and extends while the progression holds — unless the run that
/// would open at the SUCCESSOR is longer, in which case the slot is a
/// singleton (so one slot moved out of a stride run leaves the run intact
/// beside it instead of fragmenting it). Singletons are stride 1 (one
/// spelling). The identity distribution and every stride-doubling growth
/// coalesce to ONE run per member; scattered hand-picked migrations
/// consume runs and a stamp past `STAMP_MAX_RUNS` refuses at the engine's
/// preflight.
pub fn coalesceRuns(gpa: Allocator, slots_ascending: []const u16) Allocator.Error![]ledger.SlotRun {
    var out: std.ArrayList(ledger.SlotRun) = .empty;
    errdefer out.deinit(gpa);
    var i: usize = 0;
    while (i < slots_ascending.len) {
        const here = runLengthAt(slots_ascending, i);
        const next = if (i + 1 < slots_ascending.len) runLengthAt(slots_ascending, i + 1) else 0;
        if (here == 1 or next > here) {
            try out.append(gpa, .{ .start = slots_ascending[i], .stride = 1, .count = 1 });
            i += 1;
            continue;
        }
        const stride: u16 = slots_ascending[i + 1] - slots_ascending[i];
        try out.append(gpa, .{ .start = slots_ascending[i], .stride = stride, .count = here });
        i += here;
    }
    return out.toOwnedSlice(gpa);
}

/// The length of the progression opening at `i` with the stride to its
/// successor (1 when `i` is the last slot).
fn runLengthAt(slots: []const u16, i: usize) u32 {
    if (i + 1 >= slots.len) return 1;
    assert(slots[i + 1] > slots[i]);
    const stride: u32 = slots[i + 1] - slots[i];
    var count: u32 = 2;
    var last: u32 = slots[i + 1];
    var j = i + 2;
    while (j < slots.len and @as(u32, slots[j]) == last + stride) : (j += 1) {
        last += stride;
        count += 1;
    }
    return count;
}

/// The published table: readers `load` an `Arc` snapshot (latch-free);
/// the migration engine `publish`es a whole new table.
pub const Published = struct {
    swap: arc_swap.ArcSwap(RouteTable),

    pub const Snapshot = arc_swap.ArcSwap(RouteTable).Ptr;

    pub fn init(gpa: Allocator, table: RouteTable) Allocator.Error!Published {
        return .{ .swap = .init(try arc_swap.Arc(RouteTable).create(gpa, table)) };
    }

    pub fn deinit(self: *Published) void {
        self.swap.deinit();
    }

    /// Caller releases.
    pub fn load(self: *Published) Snapshot {
        return self.swap.load();
    }

    /// One volume lookup without holding a snapshot.
    pub fn volumeOf(self: *Published, slot: u16) VolumeIndex {
        const s = self.load();
        defer s.release();
        return s.value.volumeOf(slot);
    }

    pub fn volumeOfEffective(self: *Published, eff: u64) VolumeIndex {
        return self.volumeOf(slotOfEffective(eff));
    }

    pub fn publish(self: *Published, gpa: Allocator, table: RouteTable) Allocator.Error!void {
        self.swap.store(try arc_swap.Arc(RouteTable).create(gpa, table));
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the effective-ino law: slot 0 is the native keyspace, every other slot its guest keyspace; inverse and slot agree with inos.routeIno over the ino space" {
    try testing.expectEqual(@as(u64, 1), effectiveIno(1));
    try testing.expectEqual(@as(u64, 2), effectiveIno(2));
    try testing.expectEqual(key.guestLocalIno(1, 2), effectiveIno(3));
    try testing.expectEqual(@as(u64, 3), effectiveIno(2 + inos.ROUTING_WIDTH));
    try testing.expectEqual(@as(u16, 0), slotOfEffective(1));
    try testing.expectEqual(@as(u16, 1), slotOfEffective(key.guestLocalIno(1, 2)));
    try testing.expectEqual(key.NATIVE_FOREST_SLOT, forestSlotOf(0));
    try testing.expectEqual(@as(key.ForestSlot, 8), forestSlotOf(7));
    try testing.expectEqual(@as(u16, 7), slotOfForestSlot(8));
    try testing.expectEqual(@as(u16, 0), slotOfForestSlot(0));
    // The routable space: every mintable ino (local < 2^40) is; nodeid 0,
    // the virtual control range (2⁶⁴ − 2³² …) and the first ino whose
    // local reaches the guest namespace are not.
    try testing.expect(isRoutable(1) and isRoutable(2) and isRoutable(3));
    try testing.expect(isRoutable(inos.makeGlobalIno(key.GUEST_NS_BASE - 1, W - 1, inos.ROUTING_WIDTH)));
    try testing.expect(!isRoutable(inos.makeGlobalIno(key.GUEST_NS_BASE, 0, inos.ROUTING_WIDTH)));
    try testing.expect(!isRoutable(0));
    try testing.expect(!isRoutable(0xffff_ffff_0000_0000));
    var prng = std.Random.DefaultPrng.init(0x5107);
    const rnd = prng.random();
    var i: usize = 0;
    while (i < 20_000) : (i += 1) {
        const g = rnd.intRangeAtMost(u64, 1, 1 << 48);
        const eff = effectiveIno(g);
        try testing.expectEqual(g, globalOfEffective(eff));
        try testing.expectEqual(slotOf(g), slotOfEffective(eff));
        try testing.expectEqual(forestSlotOf(slotOf(g)), key.forestSlotOfIno(eff));
        try testing.expectEqual(slotOf(g), try slotOfKey(&key.inodeKey(eff)));
        try testing.expectEqual(slotOf(g), try slotOfKey(&key.blockRefKey(9, 4, eff, 0)));
    }
}

test "validate: every refusal with the reference's text; the identity table validates" {
    var buf: [256]u8 = undefined;
    var diag: Refusal = undefined;
    const id = RouteTable.identity(3);
    try validate(3, &id.slot_to_volume, 0, &diag);
    try testing.expectError(error.Refused, validate(0, &id.slot_to_volume, null, &diag));
    try testing.expect(diag == .no_volumes);
    var w: std.Io.Writer = .fixed(&buf);
    try diag.write(&w);
    try testing.expectEqualStrings("metadata slot map invalid: no metadata volumes", w.buffered());
    try testing.expectError(error.Refused, validate(W + 1, &id.slot_to_volume, null, &diag));
    try testing.expect(diag == .width_below_volumes);
    try testing.expectError(error.Refused, validate(3, id.slot_to_volume[0..100], null, &diag));
    try testing.expect(diag == .wrong_length);
    w = .fixed(&buf);
    try diag.write(&w);
    try testing.expectEqualStrings("metadata slot map invalid: map covers 100 slots but the frozen routing width is 65536", w.buffered());
    try testing.expectError(error.Refused, validate(2, &id.slot_to_volume, null, &diag));
    try testing.expectEqual(Refusal{ .volume_out_of_range = .{ .slot = 2, .volume = 2, .volumes = 2 } }, diag);
    var hostless = RouteTable.identity(2);
    for (&hostless.slot_to_volume) |*v| v.* = 0;
    try testing.expectError(error.Refused, validate(2, &hostless.slot_to_volume, null, &diag));
    w = .fixed(&buf);
    try diag.write(&w);
    try testing.expectEqualStrings("metadata slot map invalid: volume index 1 hosts no slot — every member must host at least one (its native mint slot)", w.buffered());
    // The native keyspace must sit with slot 0's host.
    try testing.expectError(error.Refused, validate(3, &id.slot_to_volume, 1, &diag));
    try testing.expect(diag == .native_not_slot_zero_host);
}

test "the table: identity distribution, hosted counts and ascending sets, withSlot, runs round-trip through fillFromRuns; coalescing spells one run per member and singletons stride 1" {
    const gpa = testing.allocator;
    const t = RouteTable.identity(4);
    try testing.expectEqual(@as(VolumeIndex, 2), t.volumeOf(6));
    try testing.expectEqual(@as(VolumeIndex, 1), t.volumeOfGlobal(3));
    try testing.expectEqual(@as(VolumeIndex, 0), t.volumeOfEffective(5));
    try testing.expectEqual(@as(VolumeIndex, 3), t.volumeOfEffective(key.guestLocalIno(7, 9)));
    try testing.expectEqual(@as(u32, 16384), t.hostedCount(1));
    var buf: [8]u16 = undefined;
    try testing.expectEqualSlices(u16, &.{ 2, 6, 10, 14, 18, 22, 26, 30 }, t.hostedAscending(2, &buf));
    const moved = t.withSlot(6, 0);
    try testing.expectEqual(@as(VolumeIndex, 0), moved.volumeOf(6));
    try testing.expectEqual(@as(u32, 16383), moved.hostedCount(2));
    try testing.expectEqual(@as(u32, 16385), moved.hostedCount(0));
    // Runs: the identity member is ONE run; after one slot moved away the
    // donor's set is three runs (before, the gap, after) and the receiver's two.
    const r1 = try t.runsOf(gpa, 1);
    defer gpa.free(r1);
    try testing.expectEqualSlices(ledger.SlotRun, &.{ledger.memberRun(1, 4)}, r1);
    const r2 = try moved.runsOf(gpa, 2);
    defer gpa.free(r2);
    try testing.expectEqualSlices(ledger.SlotRun, &.{ .{ .start = 2, .stride = 1, .count = 1 }, .{ .start = 10, .stride = 4, .count = 16382 } }, r2);
    const r0 = try moved.runsOf(gpa, 0);
    defer gpa.free(r0);
    try testing.expectEqualSlices(ledger.SlotRun, &.{ .{ .start = 0, .stride = 1, .count = 1 }, .{ .start = 4, .stride = 2, .count = 3 }, .{ .start = 12, .stride = 4, .count = 16381 } }, r0);
    var map: [W]?VolumeIndex = @splat(null);
    inline for (.{ 0, 1, 2, 3 }) |v| {
        const runs = try moved.runsOf(gpa, v);
        defer gpa.free(runs);
        RouteTable.fillFromRuns(&map, v, runs);
    }
    for (map, 0..) |m, s| try testing.expectEqual(moved.slot_to_volume[s], m.?);
    // Coalescing: scattered slots.
    const sc = try coalesceRuns(gpa, &.{ 1, 2, 3, 10, 20, 30, 31, 100 });
    defer gpa.free(sc);
    try testing.expectEqualSlices(ledger.SlotRun, &.{ .{ .start = 1, .stride = 1, .count = 3 }, .{ .start = 10, .stride = 10, .count = 3 }, .{ .start = 31, .stride = 69, .count = 2 } }, sc);
    // A slot migrated OUT of a stride run leaves the run whole beside a
    // singleton; a stride-doubling growth is one run per member.
    const gap = try coalesceRuns(gpa, &.{ 2, 10, 14, 18, 22 });
    defer gpa.free(gap);
    try testing.expectEqualSlices(ledger.SlotRun, &.{ .{ .start = 2, .stride = 1, .count = 1 }, .{ .start = 10, .stride = 4, .count = 4 } }, gap);
    const one = try coalesceRuns(gpa, &.{65535});
    defer gpa.free(one);
    try testing.expectEqualSlices(ledger.SlotRun, &.{.{ .start = 65535, .stride = 1, .count = 1 }}, one);
    const none = try coalesceRuns(gpa, &.{});
    defer gpa.free(none);
    try testing.expectEqual(@as(usize, 0), none.len);
}

test "Published: a load sees the stored table; a publish swaps whole and the old snapshot stays valid for its holder" {
    const gpa = testing.allocator;
    var p = try Published.init(gpa, RouteTable.identity(2));
    defer p.deinit();
    try testing.expectEqual(@as(VolumeIndex, 1), p.volumeOf(3));
    const old = p.load();
    defer old.release();
    try p.publish(gpa, old.value.withSlot(3, 0));
    try testing.expectEqual(@as(VolumeIndex, 0), p.volumeOf(3));
    try testing.expectEqual(@as(VolumeIndex, 1), old.value.volumeOf(3));
    try testing.expectEqual(@as(VolumeIndex, 0), p.volumeOfEffective(key.guestLocalIno(3, 2)));
}
