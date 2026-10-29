//! The interception mount's per-inode ring words (decision 0003 §CE),
//! shared by the filesystem — a kernel size set — and the IPC host's
//! coherence limiter (`host/inval.zig`). PORTABLE.
//!
//! An `-o interception` mount is ISOLATED for the TTL law (§AB.2) as a
//! plain mount is: every class is `T_owner`. The ring changes the bytes,
//! size and times of regular files alone, through a binding, and the
//! limiter tells the kernel every such change by invalidation (`inval.
//! zig`); this table holds the limiter's word per stripe.
//!
//! Stripes, not inodes (`splitmix64(ino) & mask`, the op-guard width): a
//! collision costs one suppressed leading fire (the window's trailing
//! sweep covers it) or one spurious size announcement, never a stale view
//! past the window. Memory is a constant.
//!
//! `gen` moves at every size set the kernel made (a truncate, an `O_TRUNC`
//! open, a collapse or insert): a ring write raises the announced
//! high-water only if no size set landed between its admission and its
//! announcement, so a write that regrows a truncated file past its new size
//! is announced as the grow it is.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;

const dlm = @import("../meta/dlm.zig");

/// The limiter's word: the announced high-water in BYTES (48 bits =
/// 256 TiB; past it every write is a grow), the size-set generation, the
/// coarse second of the last leading invalidation (13 bits: wraps every
/// 2.3 h; `inval.WINDOW_S_MAX` keeps the window inside half of it).
pub const Word = packed struct(u64) {
    hwm: u48,
    gen: u3,
    last_fire_s: u13,
};

pub const Table = struct {
    words: []Atomic(u64),
    mask: u64,

    pub fn init(gpa: Allocator, stripes: usize) Allocator.Error!Table {
        std.debug.assert(std.math.isPowerOfTwo(stripes));
        const words = try gpa.alloc(Atomic(u64), stripes);
        @memset(words, .init(0));
        return .{ .words = words, .mask = stripes - 1 };
    }

    pub fn deinit(self: *Table, gpa: Allocator) void {
        gpa.free(self.words);
        self.* = undefined;
    }

    pub fn stripeOf(self: *const Table, ino: u64) usize {
        return @intCast(dlm.splitmix64(ino) & self.mask);
    }

    pub fn word(self: *const Table, ino: u64) *Atomic(u64) {
        return &self.words[self.stripeOf(ino)];
    }

    /// The stripe's size-set generation, read by a ring write before its
    /// admission (`inval.Invalidator.onWrite`).
    pub fn genOf(self: *const Table, ino: u64) u3 {
        const w: Word = @bitCast(self.word(ino).load(.seq_cst));
        return w.gen;
    }

    /// The kernel set `ino`'s size to `size` (under the op's exclusive
    /// guards, before its commit): the high-water is what the kernel now
    /// knows, and the generation moves so a ring write admitted before it
    /// raises nothing past it.
    pub fn sizeSet(self: *const Table, ino: u64, size: u64) void {
        const cell = self.word(ino);
        var cur = cell.load(.seq_cst);
        while (true) {
            var w: Word = @bitCast(cur);
            w.hwm = @intCast(@min(size, std.math.maxInt(u48)));
            w.gen +%= 1;
            cur = cell.cmpxchgWeak(cur, @bitCast(w), .seq_cst, .seq_cst) orelse return;
        }
    }
};

const testing = std.testing;

test "the ring table: a size set lowers the stripe's high-water to the kernel's size and moves its generation (wrapping in three bits), the window's second untouched; another stripe is untouched" {
    var t = try Table.init(testing.allocator, 64);
    defer t.deinit(testing.allocator);
    var other: u64 = 8;
    while (t.stripeOf(other) == t.stripeOf(7)) other += 1;
    t.word(7).store(@bitCast(Word{ .hwm = 8192, .gen = 7, .last_fire_s = 5 }), .seq_cst);
    const g0 = t.genOf(7);
    t.sizeSet(7, 100);
    const w: Word = @bitCast(t.word(7).load(.seq_cst));
    try testing.expectEqual(@as(u48, 100), w.hwm);
    try testing.expectEqual(g0 +% 1, t.genOf(7));
    try testing.expectEqual(@as(u3, 0), w.gen);
    try testing.expectEqual(@as(u13, 5), w.last_fire_s);
    try testing.expectEqual(@as(u64, 0), t.word(other).load(.seq_cst));
}
