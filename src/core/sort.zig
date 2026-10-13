//! The stable sort for short hot-path arrays (a lock set, a guard plan, a
//! window's write ops, a range page).
//!
//! `std.mem.sort` is `std.sort.block`, which declares a 512-element scratch
//! array BEFORE its small-array early return; ReleaseSafe — the product's
//! build — fills every `undefined` declaration, so every call paid
//! `512 × @sizeOf(T)` bytes of fill whatever the length: 4 KiB for a
//! pointer, 16 KiB for a `tree.KeyValue` (`docs/evidence/rung-10-dev-perf.md`
//! §3.6: the commit path's lock-set and guard-plan sorts, the journal's op
//! sort). Insertion sort is stable, needs no scratch, and at
//! `INSERTION_MAX` elements costs at most `n(n−1)/2 = 496` comparisons —
//! fewer element moves than one 512-element fill of the smallest `T`;
//! longer arrays take the block sort and its fill, amortized over them.

const std = @import("std");

/// The insertion arm's ceiling: n(n−1)/2 = 496 comparisons at 32, under one
/// 512-element scratch fill of the block arm (module doc).
pub const INSERTION_MAX = 32;

pub fn stable(comptime T: type, items: []T, context: anytype, comptime lessThan: fn (@TypeOf(context), lhs: T, rhs: T) bool) void {
    if (items.len <= INSERTION_MAX) return std.sort.insertion(T, items, context, lessThan);
    std.sort.block(T, items, context, lessThan);
}

const testing = std.testing;

const Pair = struct { k: u8, seq: u16 };

fn pairLess(_: void, a: Pair, b: Pair) bool {
    return a.k < b.k;
}

test "stable: equal keys keep their input order on both arms (insertion ≤ INSERTION_MAX < block), and both arms sort" {
    var prng = std.Random.DefaultPrng.init(0x50_47);
    const rnd = prng.random();
    for ([_]usize{ 0, 1, 2, 3, INSERTION_MAX, INSERTION_MAX + 1, 300 }) |n| {
        const items = try testing.allocator.alloc(Pair, n);
        defer testing.allocator.free(items);
        for (items, 0..) |*p, i| p.* = .{ .k = rnd.uintLessThan(u8, 8), .seq = @intCast(i) };
        stable(Pair, items, {}, pairLess);
        var i: usize = 1;
        while (i < items.len) : (i += 1) {
            try testing.expect(items[i - 1].k <= items[i].k);
            if (items[i - 1].k == items[i].k) try testing.expect(items[i - 1].seq < items[i].seq);
        }
    }
}
