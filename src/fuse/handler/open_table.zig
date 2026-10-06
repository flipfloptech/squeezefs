//! Open count, kernel lookup references and `open_count_stranded`
//! (`docs/porting/fuse_handlers_lanes.md` §8; `fuse_abi_session.md` §7;
//! charter §4 Teardown: "`open_count_stranded` (a final FORGET with open
//! handles counted = a lost RELEASE)").
//!
//! One entry per ino the kernel knows: `{count, lookups, dirty}`.
//!
//! - `addOpen` (OPEN / OPENDIR / CREATE): `count 0 → 1` clears `dirty` — a
//!   fresh open generation starts clean (the `fuse_release_clean_fastpath`
//!   elision keys on it). `removeOpen` (RELEASE / RELEASEDIR) saturates at
//!   0. The open/reclaim handshake is count-FIRST: open counts, then
//!   refuses if a reclaim already claimed the slot; the reclaim side claims
//!   then re-checks the count — every interleaving safe.
//! - `noteLookup` records ONE kernel reference per entry reply sent —
//!   LOOKUP, CREATE, MKNOD, MKDIR, SYMLINK, LINK, each READDIRPLUS entry;
//!   the root (`ino ≤ 1`) is never counted (the kernel never forgets it).
//! - `returnLookups(ino, nlookup)`: `n = max(nlookup, 1)` ("the kernel never
//!   sends 0; treat it as one so a malformed forget can never make an inode
//!   unevictable"); `lookups −= n` saturating; EVICT iff it reached 0 — an
//!   UNTRACKED inode evicts on its first forget (a miscount can only cost
//!   cache retention, never correctness). **POSIX-14**: reaching zero is the
//!   kernel certifying no `struct file` is open (it cannot evict an inode a
//!   file holds), so `count > 0` here is a LOST RELEASE: the tripwire
//!   `open_count_stranded` fires, the count is NOT zeroed ("an unlinked-open
//!   file's live data must never be destroyed on the strength of an
//!   accounting we already know is wrong" — generic/795) and the entry
//!   stays until `count == 0` too. Zig has no unwinding, so a lost RELEASE
//!   can only be a logic error: the tripwire must read 0 on every teardown.
//!
//! Striped under spin locks (the metadata-transaction class; O(1)
//! sections) and pre-sized once from the transport geometry; the kernel's
//! inode cache can outgrow any fixed bound, so growth past the pre-size is
//! one allocation per doubling of a stripe, never per op (the
//! `economy.PendingTimes` posture).

const std = @import("std");
const assert = std.debug.assert;
const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const jc = @import("../../meta/journal_core.zig");

/// `open_count_stranded` (must stay 0).
pub var open_count_stranded: counters.Tripwire = .init("open_count_stranded");
/// `fuse_open_inodes`: inos with `count > 0` right now.
pub var fuse_open_inodes: counters.Gauge = .{};
/// `fuse_tracked_inodes`: entries in the table (kernel-known inos).
pub var fuse_tracked_inodes: counters.Gauge = .{};

pub const Entry = struct {
    count: u32 = 0,
    lookups: u64 = 0,
    dirty: bool = false,
};

/// 16 stripes: an inode-cache storm (FORGET sweeps, lookup storms) spreads
/// its O(1) sections over 16 lines.
const stripe_count = 16;

/// The pre-size: four entries per delivered slot, floor 4096 — a steady
/// state holds a handful of inos per in-flight op and the floor is one
/// typical working set.
pub fn derivedCapacity(nqueues: u32, q_depth: u32) u32 {
    return @max(4096, 4 * nqueues * q_depth);
}

pub const Verdict = struct {
    /// The last reference went: the caller invalidates caches, sweeps the
    /// per-inode side structures and queues the reclaim (iff not open).
    evicted: bool,
    /// The count found open at eviction (0 healthy).
    stranded_count: u32,
};

pub const OpenTable = struct {
    gpa: std.mem.Allocator,
    stripes: [stripe_count]Stripe,

    const Stripe = struct {
        lock: jc.SpinLock = .{},
        map: std.AutoHashMapUnmanaged(u64, Entry) = .empty,
    };

    pub fn init(gpa: std.mem.Allocator, capacity: u32) std.mem.Allocator.Error!OpenTable {
        var self: OpenTable = .{ .gpa = gpa, .stripes = @splat(.{}) };
        errdefer self.deinit();
        for (&self.stripes) |*s| try s.map.ensureTotalCapacity(gpa, capacity / stripe_count + 1);
        return self;
    }

    pub fn deinit(self: *OpenTable) void {
        var tracked: u64 = 0;
        var open: u64 = 0;
        for (&self.stripes) |*s| {
            var it = s.map.valueIterator();
            while (it.next()) |e| {
                tracked += 1;
                if (e.count > 0) open += 1;
            }
            s.map.deinit(self.gpa);
        }
        fuse_tracked_inodes.sub(tracked);
        fuse_open_inodes.sub(open);
        self.* = undefined;
    }

    fn stripeOf(self: *OpenTable, ino: u64) *Stripe {
        // Fibonacci hashing: consecutive mints spread over the stripes.
        return &self.stripes[@intCast((ino *% 0x9E3779B97F4A7C15) >> 60)];
    }

    /// OPEN / OPENDIR / CREATE. Returns the new count.
    pub fn addOpen(self: *OpenTable, ino: u64) std.mem.Allocator.Error!u32 {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const gop = try s.map.getOrPut(self.gpa, ino);
        if (!gop.found_existing) {
            gop.value_ptr.* = .{};
            fuse_tracked_inodes.add(1);
        }
        if (gop.value_ptr.count == 0) {
            gop.value_ptr.dirty = false;
            fuse_open_inodes.add(1);
        }
        gop.value_ptr.count += 1;
        return gop.value_ptr.count;
    }

    /// RELEASE / RELEASEDIR: saturating at 0. Returns the new count (0 =
    /// the last close) and removes an entry the kernel no longer references.
    pub fn removeOpen(self: *OpenTable, ino: u64) u32 {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const e = s.map.getPtr(ino) orelse return 0;
        if (e.count == 0) return 0;
        e.count -= 1;
        const left = e.count;
        if (left == 0) {
            fuse_open_inodes.sub(1);
            if (e.lookups == 0) {
                _ = s.map.remove(ino);
                fuse_tracked_inodes.sub(1);
            }
        }
        return left;
    }

    pub fn isOpen(self: *OpenTable, ino: u64) bool {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const e = s.map.get(ino) orelse return false;
        return e.count > 0;
    }

    /// A WRITE through an open handle: the release path's clean-handle
    /// elision keys on it.
    pub fn markDirty(self: *OpenTable, ino: u64) void {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        if (s.map.getPtr(ino)) |e| e.dirty = true;
    }

    pub fn isDirty(self: *OpenTable, ino: u64) bool {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const e = s.map.get(ino) orelse return false;
        return e.dirty;
    }

    /// One kernel reference per entry reply; the root is never counted.
    pub fn noteLookup(self: *OpenTable, ino: u64) std.mem.Allocator.Error!void {
        if (ino <= 1) return;
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const gop = try s.map.getOrPut(self.gpa, ino);
        if (!gop.found_existing) {
            gop.value_ptr.* = .{};
            fuse_tracked_inodes.add(1);
        }
        gop.value_ptr.lookups += 1;
    }

    /// FORGET / one BATCH_FORGET entry (module doc).
    pub fn returnLookups(self: *OpenTable, ino: u64, nlookup: u64) Verdict {
        const n = @max(nlookup, 1);
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const e = s.map.getPtr(ino) orelse return .{ .evicted = true, .stranded_count = 0 };
        e.lookups -|= n;
        if (e.lookups != 0) return .{ .evicted = false, .stranded_count = 0 };
        const stranded = e.count;
        if (stranded > 0) {
            open_count_stranded.fire("final FORGET with open handles counted — a lost RELEASE; the count is kept, never zeroed");
            log.err("open_count_stranded: ino {d} reached nlookup 0 with {d} open handle(s) — a RELEASE was lost; the entry is kept until its count drains", .{ ino, stranded });
            return .{ .evicted = true, .stranded_count = stranded };
        }
        _ = s.map.remove(ino);
        fuse_tracked_inodes.sub(1);
        return .{ .evicted = true, .stranded_count = 0 };
    }

    pub fn get(self: *OpenTable, ino: u64) ?Entry {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        return s.map.get(ino);
    }

    /// Entries whose `count > 0` (the teardown sweep's census).
    pub fn openCount(self: *OpenTable) u64 {
        var n: u64 = 0;
        for (&self.stripes) |*s| {
            s.lock.lock();
            defer s.lock.unlock();
            var it = s.map.valueIterator();
            while (it.next()) |e| {
                if (e.count > 0) n += 1;
            }
        }
        return n;
    }
};

// ---------------------------------------------------------------------------
// Tests (the reference's lookup_refcount_tests)
// ---------------------------------------------------------------------------

const testing = std.testing;

test "capacity derivation: four per delivered slot, floor 4096" {
    try testing.expectEqual(@as(u32, 4096), derivedCapacity(1, 4));
    try testing.expectEqual(@as(u32, 4096), derivedCapacity(32, 32));
    try testing.expectEqual(@as(u32, 8192), derivedCapacity(64, 32));
}

test "entry replies count and a partial forget does not evict; the root is never counted" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    try t.noteLookup(1);
    try testing.expectEqual(@as(?Entry, null), t.get(1));
    try t.noteLookup(10);
    try t.noteLookup(10);
    try t.noteLookup(10);
    try testing.expectEqual(@as(u64, 3), t.get(10).?.lookups);
    var v = t.returnLookups(10, 2);
    try testing.expect(!v.evicted);
    try testing.expectEqual(@as(u64, 1), t.get(10).?.lookups);
    v = t.returnLookups(10, 1);
    try testing.expect(v.evicted);
    try testing.expectEqual(@as(u32, 0), v.stranded_count);
    try testing.expectEqual(@as(?Entry, null), t.get(10));
}

test "an uncounted inode still evicts on its first forget; nlookup 0 is treated as one" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    try testing.expect(t.returnLookups(77, 5).evicted);
    try t.noteLookup(78);
    try testing.expect(t.returnLookups(78, 0).evicted);
    try testing.expectEqual(@as(?Entry, null), t.get(78));
}

test "open count: 0→1 clears dirty, saturating release, the last close removes an unreferenced entry" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    try testing.expectEqual(@as(u32, 1), try t.addOpen(5));
    t.markDirty(5);
    try testing.expect(t.isDirty(5));
    try testing.expectEqual(@as(u32, 2), try t.addOpen(5));
    try testing.expect(t.isDirty(5));
    try testing.expectEqual(@as(u32, 1), t.removeOpen(5));
    try testing.expectEqual(@as(u32, 0), t.removeOpen(5));
    try testing.expect(!t.isOpen(5));
    try testing.expectEqual(@as(?Entry, null), t.get(5));
    try testing.expectEqual(@as(u32, 0), t.removeOpen(5));
    // A fresh open generation starts clean.
    try testing.expectEqual(@as(u32, 1), try t.addOpen(5));
    try testing.expect(!t.isDirty(5));
    try testing.expectEqual(@as(u64, 1), t.openCount());
    _ = t.removeOpen(5);
}

test "POSIX-14: a lost release leaves a stranded open count that is detected, counted and NEVER zeroed" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    try t.noteLookup(9);
    _ = try t.addOpen(9);
    const before = counters.tripwireCount("open_count_stranded");
    open_count_stranded.resetLogBudgetForTest();
    // The tripwire's own line plus the table's.
    log.testing.expectErrorLogs(2);
    const v = t.returnLookups(9, 1);
    try log.testing.expectAllFired();
    try testing.expect(v.evicted);
    try testing.expectEqual(@as(u32, 1), v.stranded_count);
    try testing.expectEqual(before + 1, counters.tripwireCount("open_count_stranded"));
    // The entry survives with its count intact until the count drains.
    try testing.expectEqual(@as(u32, 1), t.get(9).?.count);
    try testing.expectEqual(@as(u64, 0), t.get(9).?.lookups);
    try testing.expectEqual(@as(u32, 0), t.removeOpen(9));
    try testing.expectEqual(@as(?Entry, null), t.get(9));
}

test "a healthy open/lookup lifecycle never trips: lookup, open, release, forget" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    const before = counters.tripwireCount("open_count_stranded");
    try t.noteLookup(20);
    _ = try t.addOpen(20);
    try testing.expectEqual(@as(u32, 0), t.removeOpen(20));
    // Still referenced by the kernel's lookup: the entry stays.
    try testing.expectEqual(@as(u64, 1), t.get(20).?.lookups);
    try testing.expect(t.returnLookups(20, 1).evicted);
    try testing.expectEqual(before, counters.tripwireCount("open_count_stranded"));
    try testing.expectEqual(@as(u64, 0), t.openCount());
}
