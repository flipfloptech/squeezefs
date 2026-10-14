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
//! **The no-open posture** (`no_open`, decision 0003 §AF): the daemon
//! answers OPEN with ENOSYS, the kernel opens and releases regular files
//! locally and sends no RELEASE — for a CREATE-opened file either, once the
//! connection latched. Only OPENDIR counts then (CREATE counts nothing), so
//! `open_count_stranded` means a lost RELEASEDIR; a regular file's custody
//! needs no count at all — the last FORGET (`fuse_evict_inode`, the only
//! FORGET of a live inode) comes after every `struct file` let go of the
//! inode, which is the certificate the corpse destroy already keys on.
//! What RELEASE carried — the write-back drain at the end of a file's use
//! — rides the FLUSH of every close(2), and the table keeps what that
//! cannot see: the OWED set, the inos holding a write no FLUSH has covered
//! (`noteWrite`; a FLUSH removes the writes noted before it began —
//! `flushBegin` / `flushEnd`, ordered by the stripe's sequence under its
//! lock). A held-open writer's custody, a mapping written back after its
//! fd closed (`fuse_vma_close` at munmap), an async write landing after
//! its close: each stays owed until a FLUSH covers it or the reclaim
//! cadence drains it once idle (`takeIdleOwed`). A close burst coalesces:
//! a FLUSH on an ino a FLUSH drained within the window (`recentlyDrained`,
//! stamped by `markDrained`) only notes it owed with `deferred`
//! (`deferDrain`), which the cadence's next tick drains whatever its age.
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
/// `fuse_owed_drain_inos`: no-open inos holding a write no FLUSH covered.
pub var fuse_owed_drain_inos: counters.Gauge = .{};
/// `fuse_owed_idle_drains`: owed inos the reclaim cadence drained once idle
/// (custody no close covered: a held-open writer, a mapping or an async
/// write outliving its fd).
pub var fuse_owed_idle_drains: counters.ShardedCounter = .{};

pub const Entry = struct {
    count: u32 = 0,
    lookups: u64 = 0,
    dirty: bool = false,
    /// No-open: the monotonic stamp of the last FLUSH that drained this
    /// ino's custody (the burst coalescing's clock; 0 = never).
    drained_ns: u64 = 0,
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

/// A write no FLUSH has covered yet (the no-open posture): the stripe
/// sequence of its note (a FLUSH begun at `s` covers every note `≤ s`), its
/// monotonic stamp (the idle drain's clock), and `deferred`: a close whose
/// drain was coalesced into the next reclaim tick (`deferDrain`) — harvested
/// at that tick whatever its idle age.
pub const Owed = struct { seq: u64, last_ns: u64, deferred: bool = false };

pub const OpenTable = struct {
    gpa: std.mem.Allocator,
    stripes: [stripe_count]Stripe,
    /// The no-open posture (module doc), set once at mount before the
    /// session serves its first request.
    no_open: bool = false,

    const Stripe = struct {
        lock: jc.SpinLock = .{},
        map: std.AutoHashMapUnmanaged(u64, Entry) = .empty,
        /// No-open: ino → its latest uncovered write. Independent of `map`:
        /// an evicted ino's custody is still owed its drain.
        owed: std.AutoHashMapUnmanaged(u64, Owed) = .empty,
        /// Bumped by every `noteWrite` under the lock: the order a FLUSH's
        /// begin and a write's note are judged by (both sections hold the
        /// stripe lock, so the order is total).
        seq: u64 = 0,
    };

    pub fn init(gpa: std.mem.Allocator, capacity: u32) std.mem.Allocator.Error!OpenTable {
        var self: OpenTable = .{ .gpa = gpa, .stripes = @splat(.{}) };
        errdefer self.deinit();
        for (&self.stripes) |*s| {
            try s.map.ensureTotalCapacity(gpa, capacity / stripe_count + 1);
            // A handful of files written per in-flight op: the same pre-size.
            try s.owed.ensureTotalCapacity(gpa, capacity / stripe_count + 1);
        }
        return self;
    }

    pub fn deinit(self: *OpenTable) void {
        var tracked: u64 = 0;
        var open: u64 = 0;
        var owed: u64 = 0;
        for (&self.stripes) |*s| {
            var it = s.map.valueIterator();
            while (it.next()) |e| {
                tracked += 1;
                if (e.count > 0) open += 1;
            }
            owed += s.owed.count();
            s.map.deinit(self.gpa);
            s.owed.deinit(self.gpa);
        }
        fuse_tracked_inodes.sub(tracked);
        fuse_open_inodes.sub(open);
        fuse_owed_drain_inos.sub(owed);
        self.* = undefined;
    }

    /// A WRITE landed on `ino` (after the filesystem recorded its custody).
    /// The control arm marks the open generation dirty (the release path's
    /// clean-handle elision keys on it); the no-open posture notes the ino
    /// OWED a drain — the note follows the custody, so a FLUSH whose
    /// custody check missed this write began after it and leaves it owed.
    /// `error.OutOfMemory`: the owed set could not grow; the caller drains.
    pub fn noteWrite(self: *OpenTable, ino: u64, now_ns: u64) std.mem.Allocator.Error!void {
        if (!self.no_open) return self.markDirty(ino);
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const gop = try s.owed.getOrPut(self.gpa, ino);
        const deferred = gop.found_existing and gop.value_ptr.deferred;
        if (!gop.found_existing) fuse_owed_drain_inos.add(1);
        s.seq += 1;
        gop.value_ptr.* = .{ .seq = s.seq, .last_ns = now_ns, .deferred = deferred };
    }

    /// A close of `ino` whose drain is coalesced (a FLUSH-drain of the ino
    /// is younger than the coalescing window — a burst of closes): the ino
    /// is owed with `deferred`, so the reclaim cadence drains it at its next
    /// tick; the FLUSH replies without a drain.
    pub fn deferDrain(self: *OpenTable, ino: u64, now_ns: u64) std.mem.Allocator.Error!void {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const gop = try s.owed.getOrPut(self.gpa, ino);
        if (!gop.found_existing) fuse_owed_drain_inos.add(1);
        s.seq += 1;
        gop.value_ptr.* = .{ .seq = s.seq, .last_ns = now_ns, .deferred = true };
    }

    /// Whether a FLUSH drained `ino`'s custody less than `window_ns` ago.
    pub fn recentlyDrained(self: *OpenTable, ino: u64, now_ns: u64, window_ns: u64) bool {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const e = s.map.get(ino) orelse return false;
        return e.drained_ns != 0 and now_ns -| e.drained_ns < window_ns;
    }

    /// A FLUSH drained `ino`'s custody at `now_ns` (the coalescing clock).
    pub fn markDrained(self: *OpenTable, ino: u64, now_ns: u64) void {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        if (s.map.getPtr(ino)) |e| e.drained_ns = now_ns;
    }

    /// A write that leaves no custody behind (the direct-drive lane's W1
    /// patch, durable at its completion): the control arm's dirty mark;
    /// nothing is owed a drain.
    pub fn noteDurableWrite(self: *OpenTable, ino: u64) void {
        if (!self.no_open) self.markDirty(ino);
    }

    /// A FLUSH on `ino` begins (no-open): the stripe's sequence BEFORE its
    /// custody check — every write noted at or below it recorded its
    /// custody before the check runs.
    pub fn flushBegin(self: *OpenTable, ino: u64) u64 {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        return s.seq;
    }

    /// The FLUSH begun at `begun` drained (or found nothing): the owed note
    /// it covers goes; a write noted after it began stays owed.
    pub fn flushEnd(self: *OpenTable, ino: u64, begun: u64) void {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const o = s.owed.get(ino) orelse return;
        if (o.seq > begun) return;
        _ = s.owed.remove(ino);
        fuse_owed_drain_inos.sub(1);
    }

    /// The idle drain's harvest: every owed ino whose latest write is at
    /// least `idle_ns` old — or whose close was deferred — removed from the
    /// set, up to `out.len` (the rest wait for the next call). A write after
    /// the harvest notes the ino again; the caller drains each one harvested.
    pub fn takeIdleOwed(self: *OpenTable, now_ns: u64, idle_ns: u64, out: []u64) []u64 {
        var n: usize = 0;
        for (&self.stripes) |*s| {
            if (n == out.len) break;
            s.lock.lock();
            defer s.lock.unlock();
            const start = n;
            var it = s.owed.iterator();
            while (n < out.len) {
                const kv = it.next() orelse break;
                if (!kv.value_ptr.deferred and now_ns -| kv.value_ptr.last_ns < idle_ns) continue;
                out[n] = kv.key_ptr.*;
                n += 1;
            }
            // Removed after the walk: the iterator never sees a removal.
            for (out[start..n]) |ino| {
                if (s.owed.remove(ino)) fuse_owed_drain_inos.sub(1);
            }
        }
        return out[0..n];
    }

    pub fn owedCount(self: *OpenTable) u64 {
        var n: u64 = 0;
        for (&self.stripes) |*s| {
            s.lock.lock();
            defer s.lock.unlock();
            n += s.owed.count();
        }
        return n;
    }

    /// Every ino the kernel holds a lookup reference on, copied out one
    /// stripe at a time (the caller notifies with no stripe lock held) —
    /// the kernel view's whole-cache invalidation (decision 0003 §AB.2).
    pub fn trackedInos(self: *OpenTable, gpa: std.mem.Allocator) std.mem.Allocator.Error![]u64 {
        var out: std.ArrayList(u64) = .empty;
        errdefer out.deinit(gpa);
        for (&self.stripes) |*s| {
            // The room is taken with no lock held (a spin lock never covers
            // an allocation); a stripe that grew between retries.
            while (true) {
                s.lock.lock();
                const n = s.map.count();
                if (out.unusedCapacitySlice().len >= n) {
                    var it = s.map.keyIterator();
                    while (it.next()) |k| out.appendAssumeCapacity(k.*);
                    s.lock.unlock();
                    break;
                }
                s.lock.unlock();
                try out.ensureUnusedCapacity(gpa, n + 16);
            }
        }
        return out.toOwnedSlice(gpa);
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

    /// A WRITE through an open handle (the control arm's `noteWrite`): the
    /// release path's clean-handle elision keys on it.
    fn markDirty(self: *OpenTable, ino: u64) void {
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
            open_count_stranded.fire("final FORGET with open handles counted — a lost RELEASE (no-open: a lost RELEASEDIR); the count is kept, never zeroed");
            log.err("open_count_stranded: ino {d} reached nlookup 0 with {d} open handle(s) — a RELEASE / RELEASEDIR was lost; the entry is kept until its count drains", .{ ino, stranded });
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

test "the tracked inos (decision 0003 §AB.2's whole-view invalidation): every ino a lookup reference holds, the root never" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    for ([_]u64{ 1, 2, 3, 500, 2, 77 }) |ino| try t.noteLookup(ino);
    const got = try t.trackedInos(testing.allocator);
    defer testing.allocator.free(got);
    std.mem.sort(u64, got, {}, std.sort.asc(u64));
    try testing.expectEqualSlices(u64, &.{ 2, 3, 77, 500 }, got);
}

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

test "no-open (decision 0003 §AF): a write is OWED until a FLUSH that began after its note ends; a write noted after the FLUSH began stays owed; the owed note outlives the ino's eviction; the control arm owes nothing and marks the generation dirty" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    // The control arm: the open generation's dirty bit, no owed set.
    try t.noteLookup(7);
    _ = try t.addOpen(7);
    try t.noteWrite(7, 100);
    try testing.expect(t.isDirty(7));
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    _ = t.removeOpen(7);
    _ = t.returnLookups(7, 1);

    t.no_open = true;
    const g0 = fuse_owed_drain_inos.load();
    try t.noteLookup(9);
    try t.noteWrite(9, 100);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    try testing.expectEqual(g0 + 1, fuse_owed_drain_inos.load());
    // write → FLUSH: the FLUSH began after the note — covered.
    var b = t.flushBegin(9);
    t.flushEnd(9, b);
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    try testing.expectEqual(g0, fuse_owed_drain_inos.load());
    // FLUSH begins → a write lands (its custody after the FLUSH's check) →
    // the FLUSH ends: the write stays owed.
    b = t.flushBegin(9);
    try t.noteWrite(9, 200);
    t.flushEnd(9, b);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    // The kernel evicts the ino (its last FORGET): the drain is still owed.
    try testing.expect(t.returnLookups(9, 1).evicted);
    try testing.expectEqual(@as(?Entry, null), t.get(9));
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    // A FLUSH of another ino covers nothing here.
    b = t.flushBegin(9);
    t.flushEnd(9 + 16, b);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    t.flushEnd(9, b);
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    try testing.expectEqual(g0, fuse_owed_drain_inos.load());
}

test "no-open: the idle harvest takes every owed ino whose latest write is at least the idle bound old — and only those — removes them, bounded by the caller's room; a later write notes the ino again" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    t.no_open = true;
    const idle: u64 = 1000;
    for ([_]u64{ 10, 11, 12, 13, 14 }) |ino| try t.noteWrite(ino, 0);
    try t.noteWrite(12, 900); // rewritten recently
    var out: [8]u64 = undefined;
    // Nothing is idle yet at 999 for anyone.
    try testing.expectEqual(@as(usize, 0), t.takeIdleOwed(999, idle, &out).len);
    // At 1000 four are idle; 12 (written at 900) is not.
    var small: [3]u64 = undefined;
    const first = t.takeIdleOwed(1000, idle, &small);
    try testing.expectEqual(@as(usize, 3), first.len);
    const rest = t.takeIdleOwed(1000, idle, &out);
    try testing.expectEqual(@as(usize, 1), rest.len);
    var got: [4]u64 = undefined;
    @memcpy(got[0..3], first);
    got[3] = rest[0];
    std.mem.sort(u64, &got, {}, std.sort.asc(u64));
    try testing.expectEqualSlices(u64, &.{ 10, 11, 13, 14 }, &got);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    try testing.expectEqual(@as(usize, 0), t.takeIdleOwed(1000, idle, &out).len);
    const late = t.takeIdleOwed(1900, idle, &out);
    try testing.expectEqualSlices(u64, &.{12}, late);
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    try t.noteWrite(10, 2000);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    try testing.expectEqualSlices(u64, &.{10}, t.takeIdleOwed(3000, idle, &out));
}

test "no-open close bursts coalesce: `recentlyDrained` is true within the window after `markDrained` and false outside it (and for an untracked ino); a deferred close is harvested at the next call whatever its idle age, a later write keeps it deferred, a FLUSH that drains covers it" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    t.no_open = true;
    const window: u64 = 100;
    try testing.expect(!t.recentlyDrained(5, 1000, window));
    try t.noteLookup(5);
    try testing.expect(!t.recentlyDrained(5, 1000, window));
    t.markDrained(5, 1000);
    try testing.expect(t.recentlyDrained(5, 1050, window));
    try testing.expect(!t.recentlyDrained(5, 1100, window));
    // A deferred close: owed, harvested at once (idle bound far away).
    try t.deferDrain(5, 1050);
    var out: [4]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{5}, t.takeIdleOwed(1051, std.math.maxInt(u64), &out));
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    // Deferred, then written again: still deferred.
    try t.deferDrain(5, 1060);
    try t.noteWrite(5, 1061);
    try testing.expectEqualSlices(u64, &.{5}, t.takeIdleOwed(1062, std.math.maxInt(u64), &out));
    // A plain write is not harvested before its idle bound.
    try t.noteWrite(5, 1070);
    try testing.expectEqual(@as(usize, 0), t.takeIdleOwed(1071, std.math.maxInt(u64), &out).len);
    // A deferred note a later draining FLUSH covers.
    try t.deferDrain(6, 1080);
    const b = t.flushBegin(6);
    t.flushEnd(6, b);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    _ = t.returnLookups(5, 1);
    try testing.expectEqualSlices(u64, &.{5}, t.takeIdleOwed(std.math.maxInt(u64), 0, &out));
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
