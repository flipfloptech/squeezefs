//! Models of the data plane's lock choreography (charter §2 "Lock order";
//! `data/plane.zig` module doc), explored exhaustively with
//! `model/explorer.zig`. Standalone value types: the locks are holder
//! words, the threads explicit program counters, one action per `step`.
//!
//! `PromotionOrderModel`: a staged promotion against an upload of a
//! sibling ino colliding on BOTH the (3.5) meta stripe and the (3.7) layout
//! lock (`layout.colliding_inos`). The upload's order is fixed by
//! `accumulate` + `plane.mapMerge`: (3.5) then (3.7). The promotion taking
//! (3.5) first HOLDS; the pre-fix order — the layout lock first, the stripe
//! inside `pack.commit` — FAILS with the ABBA wedge (0002 row 209).
//!
//! `HeadFoldModel`: a tree-mapped head `{size, cursor}` written by a
//! truncate (size-flip-first: `{new, min(cursor, new)}`) and by a running
//! sweep's chunk (`{size, max(cursor, size) + chunk}`). Both under the
//! layout lock HOLDS: every final head is one of the two serial outcomes.
//! The truncate outside the lock (the pre-fix kv_fs) FAILS: the sweep's
//! Put, computed from the head it read before the truncate landed, puts the
//! OLD size back — the inode says 8 blocks, the head 64, and the residue in
//! [8, 64) is never swept (0002 row 210).

const std = @import("std");
const explorer = @import("../model/explorer.zig");

/// Two threads, two locks. `t == 0` is the promotion, `t == 1` the upload.
pub fn PromotionOrderModel(comptime stripe_first: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        const Lock = enum(u8) { stripe, layout };
        const none: u8 = 0xFF;

        /// Holder thread per lock, `none` when free.
        holder: [2]u8 = @splat(none),
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 4;
        }

        /// The lock thread `t` touches at its current pc: acquire at pc
        /// 0/1, release at 2/3 in reverse order.
        fn lockAt(t: usize, pc: u8) Lock {
            const first: Lock = if (t == 1 or stripe_first) .stripe else .layout;
            const second: Lock = if (first == .stripe) .layout else .stripe;
            return switch (pc) {
                0, 3 => first,
                1, 2 => second,
                else => unreachable,
            };
        }

        fn blocked(self: *const Self, t: usize) bool {
            if (self.pc[t] >= 2) return false;
            const h = self.holder[@backingInt(lockAt(t, self.pc[t]))];
            return h != none and h != t;
        }

        pub fn step(self: *Self, t: usize) void {
            const l = @backingInt(lockAt(t, self.pc[t]));
            switch (self.pc[t]) {
                0, 1 => {
                    // A blocked acquire is a no-op step (the park).
                    if (self.holder[l] != none) return;
                    self.holder[l] = @intCast(t);
                },
                2, 3 => self.holder[l] = none,
                else => unreachable,
            }
            self.pc[t] += 1;
        }

        /// The wedge: every unfinished thread parked on a lock the other holds.
        pub fn invariant(self: *const Self) !void {
            var live: usize = 0;
            var parked: usize = 0;
            for (0..thread_count) |t| {
                if (self.done(t)) continue;
                live += 1;
                if (self.blocked(t)) parked += 1;
            }
            if (live != 0 and parked == live) return error.Deadlock;
        }

        pub fn final(self: *const Self) !void {
            if (self.holder[0] != none or self.holder[1] != none) return error.LockLeaked;
        }
    };
}

/// Two threads over one head and one lock. `t == 0` is the truncate,
/// `t == 1` the sweep chunk. Sizes and cursors in blocks.
pub fn HeadFoldModel(comptime truncate_under_lock: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        /// The head before either: 64 blocks with a live cursor at 64 (an
        /// earlier shrink's residue above the end, mid-sweep).
        pub const size0: u32 = 64;
        pub const cursor0: u32 = 64;
        /// The truncate's new size and the sweep's page width.
        pub const new_size: u32 = 8;
        pub const chunk: u32 = 16;
        const none: u8 = 0xFF;

        head_size: u32 = size0,
        head_cursor: u32 = cursor0,
        holder: u8 = none,
        /// Each thread's read of the head.
        seen_size: [2]u32 = @splat(0),
        seen_cursor: [2]u32 = @splat(0),
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        /// The truncate's program: [lock] read put [unlock]; the sweep's:
        /// lock read put unlock.
        fn steps(t: usize) u8 {
            return if (t == 0 and !truncate_under_lock) 2 else 4;
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == steps(t);
        }

        fn locked(t: usize) bool {
            return t == 1 or truncate_under_lock;
        }

        fn blocked(self: *const Self, t: usize) bool {
            return locked(t) and self.pc[t] == 0 and self.holder != none and self.holder != t;
        }

        pub fn step(self: *Self, t: usize) void {
            // Program counters with the lock: 0 lock, 1 read, 2 put, 3 unlock;
            // without: 0 read, 1 put.
            const pc = if (locked(t)) self.pc[t] else self.pc[t] + 1;
            switch (pc) {
                0 => {
                    if (self.holder != none) return;
                    self.holder = @intCast(t);
                },
                1 => {
                    self.seen_size[t] = self.head_size;
                    self.seen_cursor[t] = self.head_cursor;
                },
                2 => if (t == 0) {
                    // SIZE-FLIP-FIRST: {new, min(cursor, K)} (`plane.stageKvmapShrink`).
                    self.head_size = new_size;
                    self.head_cursor = @min(self.seen_cursor[t], new_size);
                } else {
                    // One chunk: start = max(cursor, size); cursor → start + chunk
                    // (`plane.sweepChunk`), the size restated from the read.
                    const start = @max(self.seen_cursor[t], self.seen_size[t]);
                    self.head_size = self.seen_size[t];
                    self.head_cursor = start + chunk;
                },
                3 => self.holder = none,
                else => unreachable,
            }
            self.pc[t] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            var live: usize = 0;
            var parked: usize = 0;
            for (0..thread_count) |t| {
                if (self.done(t)) continue;
                live += 1;
                if (self.blocked(t)) parked += 1;
            }
            if (live != 0 and parked == live) return error.Deadlock;
        }

        /// Linearizable: the head is the truncate-then-sweep outcome
        /// `{8, 24}` or the sweep-then-truncate outcome `{8, 8}`. Anything
        /// else is a lost update — the truncate's size overwritten by a Put
        /// computed from a stale read.
        pub fn final(self: *const Self) !void {
            if (self.holder != none) return error.LockLeaked;
            if (self.head_size != new_size) return error.TruncateSizeLost;
            if (self.head_cursor != new_size + chunk and self.head_cursor != new_size) return error.CursorNotSerial;
        }
    };
}

const testing = std.testing;

test "the promotion order (3.5) then (3.7) holds against the upload's; the pre-fix order (the layout lock first, the stripe inside the commit) deadlocks — the recorded weakening" {
    try explorer.expectHolds(PromotionOrderModel(true), .{});
    try explorer.expectFails(PromotionOrderModel(false), .{}, error.InvariantViolated);
}

test "the kvmap head fold holds with the truncate under the layout lock; the truncate outside it loses its size to a sweep chunk's Put — the recorded weakening" {
    try explorer.expectHolds(HeadFoldModel(true), .{});
    try explorer.expectFails(HeadFoldModel(false), .{}, error.FinalViolated);
}

test "the models' tie: the two serial outcomes the fold accepts" {
    const M = HeadFoldModel(true);
    // Truncate then sweep: {8, 8} → start 8 → {8, 24}.
    var m = M.init();
    m.step(0);
    m.step(0);
    m.step(0);
    m.step(0);
    try testing.expectEqual(M.new_size, m.head_size);
    try testing.expectEqual(M.new_size, m.head_cursor);
    m.step(1);
    m.step(1);
    m.step(1);
    m.step(1);
    try testing.expectEqual(M.new_size, m.head_size);
    try testing.expectEqual(M.new_size + M.chunk, m.head_cursor);
    try m.final();
    // Sweep then truncate: {64, 80} → {8, min(80, 8)} = {8, 8}.
    var n = M.init();
    for (0..4) |_| n.step(1);
    try testing.expectEqual(M.size0, n.head_size);
    try testing.expectEqual(M.cursor0 + M.chunk, n.head_cursor);
    for (0..4) |_| n.step(0);
    try testing.expectEqual(M.new_size, n.head_size);
    try testing.expectEqual(M.new_size, n.head_cursor);
    try n.final();
}
