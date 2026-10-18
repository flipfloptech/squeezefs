//! The per-inode SIZE EPOCH: the data plane's answer to "did a size-SETTING
//! commit of this ino land between two moments?" (charter §6.2 "SIZE NEVER
//! LEADS DATA"; `docs/evidence/rung-10-size-epoch.md`).
//!
//! Two actors raise a file's size from a snapshot they took before the raise
//! lands: a striped write's postlude parks `max(claimed size, its end)` after
//! its bytes landed, and a write-back unit (a RAM-pressure spill, a promotion)
//! carries `min_size = block · bs + app_end` from the moment it was formed. A
//! truncate that commits in between clips the map and the claim; the late
//! raise then resurrects the pre-truncate end (xfstests generic/075 and 112:
//! fsx's `Size error` right after a `TRUNCATE DOWN`), or re-inserts a block
//! at or past the new EOF that a later grow reads instead of zeros. The
//! epoch tells the two apart: a raiser reads it when its bytes land and asks
//! `setSince` when it raises; a size setter bumps it under its exclusive
//! guards before its commit.
//!
//! Shape: a fixed striped table of monotonic counters indexed like the
//! plane's shape rows (`splitmix64(ino) & mask` — the ONE mix the 4a tables
//! use; zero allocation per op, one acquire load on the write path), plus
//! the EXACT per-ino record of the counter value each ino's last size set
//! wrote, under one spin lock. The table alone would make a colliding ino's
//! truncate look like this ino's — and a false "stale" drops a legitimate
//! write's size growth — so `setSince` consults the exact record only when
//! the row moved (a truncate crossed the write's flight on that row: rare),
//! never on the common path. The bump takes the lock around the counter's
//! increment and the record's write, so a reader that saw the moved row and
//! then takes the lock sees the record that moved it.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const dlm = @import("../meta/dlm.zig");
const jc = @import("../meta/journal_core.zig");

pub const SizeEpochs = struct {
    gpa: Allocator,
    rows: []Atomic(u64),
    lock: jc.SpinLock = .{},
    /// `ino → the row value its last size set wrote` (empty = never set).
    last_set: std.AutoHashMapUnmanaged(u64, u64) = .empty,

    /// `stripes` is a power of two (the mask law).
    pub fn init(gpa: Allocator, stripes: usize) Allocator.Error!SizeEpochs {
        assert(stripes != 0 and (stripes & (stripes - 1)) == 0);
        const rows = try gpa.alloc(Atomic(u64), stripes);
        @memset(rows, .init(0));
        return .{ .gpa = gpa, .rows = rows };
    }

    pub fn deinit(self: *SizeEpochs) void {
        self.last_set.deinit(self.gpa);
        self.gpa.free(self.rows);
        self.* = undefined;
    }

    fn row(self: *SizeEpochs, ino: u64) *Atomic(u64) {
        return &self.rows[@intCast(dlm.splitmix64(ino) & (self.rows.len - 1))];
    }

    /// The ino's epoch as it stands: what a raiser reads when its bytes
    /// land (one acquire load; pairs with the bump's release).
    pub fn current(self: *SizeEpochs, ino: u64) u64 {
        return self.row(ino).load(.acquire);
    }

    /// A size-setting commit of `ino` is about to land (under the setter's
    /// exclusive guards, BEFORE its commit — a raiser that takes the guard
    /// after the commit must already see it). Monotonic: never undone, a
    /// failed commit included (its raisers re-read a size nothing changed).
    pub fn bump(self: *SizeEpochs, ino: u64) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        const moved = self.row(ino).fetchAdd(1, .release) + 1;
        try self.last_set.put(self.gpa, ino, moved);
    }

    /// Whether a size set of THIS ino landed since `seen` was read from
    /// `current`. Exact: a colliding ino's bump moves the row and nothing
    /// else, and the record answers for it.
    pub fn setSince(self: *SizeEpochs, ino: u64, seen: u64) bool {
        if (self.row(ino).load(.acquire) == seen) return false;
        self.lock.lock();
        defer self.lock.unlock();
        const last = self.last_set.get(ino) orelse return false;
        return last > seen;
    }

    /// The ino left the mount's view (its FORGET, its destroy): its record
    /// goes; the row keeps counting.
    pub fn forget(self: *SizeEpochs, ino: u64) void {
        self.lock.lock();
        defer self.lock.unlock();
        _ = self.last_set.remove(ino);
    }
};

// ---------------------------------------------------------------------------
// Tests — the rule as a tie: stale ⇔ a size set of the SAME ino after `seen`.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// Two inos on one row of a `stripes`-wide table (the collision case).
fn collidingIno(stripes: usize, a: u64) u64 {
    var b: u64 = a + 1;
    while (dlm.splitmix64(b) & (stripes - 1) != dlm.splitmix64(a) & (stripes - 1)) b += 1;
    return b;
}

test "size epoch tie: unchanged ⇒ not stale; a set of the same ino after `seen` ⇒ stale; a set before `seen` ⇒ not stale; a COLLIDING ino's set alone moves the row and is NOT stale (exact); forget drops the record, the row keeps counting" {
    var e = try SizeEpochs.init(testing.allocator, 16);
    defer e.deinit();
    const a: u64 = 7;
    const b = collidingIno(16, a);
    try testing.expect(a != b);
    // Nothing set: not stale at any read.
    const seen0 = e.current(a);
    try testing.expect(!e.setSince(a, seen0));
    // A set of `a` after the read: stale for that read, current afterwards.
    try e.bump(a);
    try testing.expect(e.setSince(a, seen0));
    const seen1 = e.current(a);
    try testing.expect(!e.setSince(a, seen1));
    // The colliding ino's set moves the shared row: `a` is NOT stale
    // (a false stale would drop a legitimate write's size growth).
    try e.bump(b);
    try testing.expect(e.current(a) != seen1);
    try testing.expect(!e.setSince(a, seen1));
    try testing.expect(e.setSince(b, seen1));
    // A second set of `a`: stale against seen1, not against a fresh read.
    try e.bump(a);
    try testing.expect(e.setSince(a, seen1));
    try testing.expect(!e.setSince(a, e.current(a)));
    // Forget: the record goes (a stale read of `a` answers "not set" — the
    // ino is gone from the mount's view), the row still counts.
    const before = e.current(a);
    e.forget(a);
    try testing.expect(!e.setSince(a, seen1));
    try e.bump(b);
    try testing.expectEqual(before + 1, e.current(a));
}

test "size epoch: a bump is monotonic across inos sharing a row; `current` never regresses" {
    var e = try SizeEpochs.init(testing.allocator, 4);
    defer e.deinit();
    var last: u64 = 0;
    var ino: u64 = 1;
    while (ino <= 64) : (ino += 1) {
        try e.bump(ino);
        const now = e.current(ino);
        try testing.expect(now >= 1);
        if (dlm.splitmix64(ino) & 3 == dlm.splitmix64(1) & 3) {
            try testing.expect(now > last);
            last = now;
        }
    }
}
