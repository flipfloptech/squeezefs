//! The delta tee (charter §5.4 "delta tee in the conveyor pass (keys only,
//! bounded, overflow ⇒ fresh snapshot)"; `docs/porting/dynamic_routing.md`
//! §3.3 "The tee", §5.3, §7 Q9). PORTABLE.
//!
//! WHY keys only: records carry no journal sequence numbers the engine
//! could replay from, and the ring wraps many times during a large copy,
//! so "replay since X" does not exist; a key is re-read from the
//! authoritative source tree at delta-apply time (per-key LWW means ONE
//! re-read covers any number of commits). WHY a bounded set: the side log
//! is charged to the node-cache budget as a non-sheddable component; past
//! the cap it latches OVERFLOWED and clears — the engine answers with a
//! fresh full snapshot pass (correct by construction: the snapshot
//! supersedes the lost keys), and three consecutive overflows abort loud.
//!
//! Shape (Q9: never an allocation on the commit path): two preallocated
//! open-addressing tables of fixed-width `key.KeyBuf` entries; the lane's
//! stage-B fiber inserts the migrating slot's keys of every ACKED tx
//! under a spin lock (a few hashes per window, on the ack path — not a
//! FUSE op path); `drainRound` swaps the active table under the same lock
//! and hands the engine the other one. Attribution is the ONE key law:
//! `route_table.slotOfKey` (refs by their owner ino) — no kind skip-list.

const std = @import("std");
const checksum = @import("../../core/checksum.zig");
const Allocator = std.mem.Allocator;
const key = @import("../key.zig");
const tx = @import("../tx.zig");
const jc = @import("../journal_core.zig");
const route_table = @import("route_table.zig");
const rstats = @import("stats.zig");

const assert = std.debug.assert;

/// The side log's cap in keys: `clamp(node_cache_budget / 32 ÷ KEY_BUF_CAP,
/// 64 Ki, 1 Mi)` — 1/32 of the cache budget in key slots (the design's
/// "≈ 40 MiB charged to the node-cache budget" at the shipped 512 MiB
/// floor is 1/32 × 512 MiB ÷ 36 B ≈ 466 Ki keys); the floor keeps a tiny
/// test budget from overflowing on every round, the ceiling is the
/// reference's measured 1 M-key posture.
pub const DELTA_CAP_MIN: usize = 64 * 1024;
pub const DELTA_CAP_MAX: usize = 1024 * 1024;
pub const DELTA_CAP_BUDGET_DIVISOR: u64 = 32;

pub fn deriveDeltaCap(node_cache_budget_bytes: u64) usize {
    const keys: u64 = node_cache_budget_bytes / DELTA_CAP_BUDGET_DIVISOR / key.KEY_BUF_CAP;
    return @intCast(std.math.clamp(keys, DELTA_CAP_MIN, DELTA_CAP_MAX));
}

/// A fixed-capacity open-addressing set of keys (linear probing, load ≤ ½).
const KeySet = struct {
    slots: []key.KeyBuf,
    used: []bool,
    len: usize = 0,

    fn init(gpa: Allocator, cap: usize) Allocator.Error!KeySet {
        const size = std.math.ceilPowerOfTwo(usize, @max(2 * cap, 16)) catch unreachable;
        const slots = try gpa.alloc(key.KeyBuf, size);
        errdefer gpa.free(slots);
        const used = try gpa.alloc(bool, size);
        @memset(used, false);
        return .{ .slots = slots, .used = used };
    }

    fn deinit(self: *KeySet, gpa: Allocator) void {
        gpa.free(self.slots);
        gpa.free(self.used);
    }

    fn clear(self: *KeySet) void {
        @memset(self.used, false);
        self.len = 0;
    }

    /// True iff inserted (absent before).
    fn insert(self: *KeySet, k: []const u8) bool {
        const mask = self.slots.len - 1;
        var i: usize = @intCast(checksum.hash64(k) & mask);
        while (self.used[i]) : (i = (i + 1) & mask) {
            if (std.mem.eql(u8, self.slots[i].slice(), k)) return false;
        }
        self.slots[i] = key.KeyBuf.fromSlice(k);
        self.used[i] = true;
        self.len += 1;
        return true;
    }
};

pub const MigrationTee = struct {
    gpa: Allocator,
    /// The routing slot whose keys are captured.
    slot: u16,
    cap: usize,
    lock: jc.SpinLock = .{},
    sets: [2]KeySet,
    active: u8 = 0,
    overflowed: bool = false,
    /// Keys inserted since arming (dedup'd), for the report.
    captured: u64 = 0,

    pub fn init(gpa: Allocator, slot: u16, cap: usize) Allocator.Error!*MigrationTee {
        const self = try gpa.create(MigrationTee);
        errdefer gpa.destroy(self);
        var a = try KeySet.init(gpa, cap);
        errdefer a.deinit(gpa);
        const b = try KeySet.init(gpa, cap);
        self.* = .{ .gpa = gpa, .slot = slot, .cap = cap, .sets = .{ a, b } };
        return self;
    }

    pub fn deinit(self: *MigrationTee) void {
        self.sets[0].deinit(self.gpa);
        self.sets[1].deinit(self.gpa);
        self.gpa.destroy(self);
    }

    /// Re-arm for another slot: both tables cleared, the latch and the
    /// tally reset (the tee object outlives one migration).
    pub fn reset(self: *MigrationTee, slot: u16) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.slot = slot;
        self.sets[0].clear();
        self.sets[1].clear();
        self.active = 0;
        self.overflowed = false;
        self.captured = 0;
    }

    /// The conveyor's hook: every record of an ACKED tx whose key routes
    /// to the migrating slot joins the active set; a full set latches
    /// overflow and clears.
    pub fn noteCommitted(self: *MigrationTee, recs: []const tx.Rec) void {
        self.lock.lock();
        defer self.lock.unlock();
        const set = &self.sets[self.active];
        for (recs) |r| {
            const s = route_table.slotOfKey(r.key) catch continue;
            if (s != self.slot) continue;
            if (set.len >= self.cap) {
                self.overflowed = true;
                set.clear();
                rstats.stats.migrate_delta_overflows.inc();
            }
            if (set.insert(r.key)) self.captured += 1;
        }
    }

    pub const Round = struct {
        /// The captured keys (a view of the tee's inactive table — valid
        /// until the next `drainRound`).
        keys: []const key.KeyBuf,
        overflowed: bool,
    };

    /// Swap tables and hand out the filled one. `overflowed` reports and
    /// clears the latch: the keys are then INCOMPLETE and the engine
    /// takes a fresh snapshot instead of applying them.
    pub fn drainRound(self: *MigrationTee, out: *std.ArrayList(key.KeyBuf)) Allocator.Error!Round {
        self.lock.lock();
        const filled = self.active;
        self.active = 1 - self.active;
        const overflowed = self.overflowed;
        self.overflowed = false;
        self.lock.unlock();
        // The filled table is now the engine's alone: the lane inserts into
        // the other one.
        const set = &self.sets[filled];
        out.clearRetainingCapacity();
        try out.ensureTotalCapacity(self.gpa, set.len);
        for (set.slots, set.used) |k, u| if (u) out.appendAssumeCapacity(k);
        set.clear();
        return .{ .keys = out.items, .overflowed = overflowed };
    }

    /// The recheck after the final delta: anything captured since?
    pub fn pendingLen(self: *MigrationTee) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return self.sets[self.active].len;
    }

    pub fn hasOverflowed(self: *MigrationTee) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.overflowed;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the delta cap derivation: budget/32 ÷ KEY_BUF_CAP clamped to [64 Ki, 1 Mi]; 512 MiB ≈ 466 Ki keys" {
    try testing.expectEqual(DELTA_CAP_MIN, deriveDeltaCap(1 << 20));
    try testing.expectEqual(@as(usize, (512 << 20) / 32 / key.KEY_BUF_CAP), deriveDeltaCap(512 << 20));
    try testing.expect(deriveDeltaCap(512 << 20) > 400_000 and deriveDeltaCap(512 << 20) < 500_000);
    try testing.expectEqual(DELTA_CAP_MAX, deriveDeltaCap(1 << 40));
}

fn rec(k: []const u8) tx.Rec {
    return .{ .tree_id = key.TREE_INODES, .key = k, .kind = .put, .value = "" };
}

test "the tee captures only the migrating slot's keys (refs by owner), dedups across commits, drains by swapping tables, and latches overflow at the cap (clear + counted) so the engine takes a fresh snapshot" {
    const gpa = testing.allocator;
    const t = try MigrationTee.init(gpa, 5, 4);
    defer t.deinit();
    const g5a = key.inodeKey(key.guestLocalIno(5, 10));
    const g5b = key.xattrKey(key.guestLocalIno(5, 10), 1, 0);
    const g6 = key.inodeKey(key.guestLocalIno(6, 10));
    const nat = key.inodeKey(10);
    const ref5 = key.blockRefKey(1, 2, key.guestLocalIno(5, 11), 0);
    const ref6 = key.blockRefKey(1, 2, key.guestLocalIno(6, 11), 0);
    t.noteCommitted(&.{ rec(&g5a), rec(&g6), rec(&nat), rec(&ref5), rec(&ref6), rec(&g5a) });
    t.noteCommitted(&.{rec(&g5b)});
    try testing.expectEqual(@as(usize, 3), t.pendingLen());
    try testing.expectEqual(@as(u64, 3), t.captured);
    var out: std.ArrayList(key.KeyBuf) = .empty;
    defer out.deinit(gpa);
    const r = try t.drainRound(&out);
    try testing.expect(!r.overflowed);
    try testing.expectEqual(@as(usize, 3), r.keys.len);
    var seen_a = false;
    var seen_ref = false;
    for (r.keys) |k| {
        if (std.mem.eql(u8, k.slice(), &g5a)) seen_a = true;
        if (std.mem.eql(u8, k.slice(), &ref5)) seen_ref = true;
        try testing.expectEqual(@as(u16, 5), try route_table.slotOfKey(k.slice()));
    }
    try testing.expect(seen_a and seen_ref);
    try testing.expectEqual(@as(usize, 0), t.pendingLen());
    // Overflow: a fifth distinct key past cap 4 latches, clears, counts.
    const o0 = rstats.stats.migrate_delta_overflows.load();
    var keys: [5][key.INODE_KEY_LEN]u8 = undefined;
    for (&keys, 0..) |*k, i| k.* = key.inodeKey(key.guestLocalIno(5, 100 + i));
    for (keys) |k| t.noteCommitted(&.{rec(&k)});
    try testing.expect(t.hasOverflowed());
    try testing.expectEqual(o0 + 1, rstats.stats.migrate_delta_overflows.load());
    try testing.expectEqual(@as(usize, 1), t.pendingLen());
    const r2 = try t.drainRound(&out);
    try testing.expect(r2.overflowed);
    try testing.expect(!t.hasOverflowed());
    const r3 = try t.drainRound(&out);
    try testing.expect(!r3.overflowed);
    try testing.expectEqual(@as(usize, 0), r3.keys.len);
}
