//! The per-ino mutation witness: ONE striped generation table the node
//! cache bumps AFTER every record of an ino-major kind (the inode record,
//! the xattr records — the layout among them) becomes visible in a leaf
//! snapshot, by any path a record takes into RAM (a conveyor commit, a
//! replay, a shipped publish served here, a rollback). It is the data
//! plane's read view's staleness proof (`data/read_view.zig`): a view
//! filled under generation `g` is current exactly while `genOf(ino) == g`,
//! and the check is one acquire load on a line only mutators write.
//!
//! The protocol is post-visibility, and that is enough: the filler reads
//! the generation BEFORE its lookups and keeps the view only if the
//! generation is unchanged AFTER them, so any record it could have missed
//! bumps after its fill and the view reads stale from then on; a record
//! applied but not yet bumped belongs to a commit nobody has been acked
//! for yet (the bump runs under the leaf's write lock, before the journal
//! write), so a view that lags it is a read concurrent with the write.
//! Two inos of one stripe invalidate each other (a spurious refill, never
//! a wrong byte). PORTABLE.

const std = @import("std");
const key = @import("key.zig");
const dlm = @import("dlm.zig");

const Atomic = std.atomic.Value;

/// 1,024 stripes, one cache line each (64 KiB of .bss): a mutator's RMW
/// invalidates the line in every reader's cache, so a stripe shares its
/// line with no neighbour; 1,024 keeps two hot files on one stripe to a
/// collision in a thousand (a refill per publish, no worse).
pub const STRIPES: usize = 1024;

const Stripe = struct {
    gen: Atomic(u64) align(std.atomic.cache_line) = .init(0),
};

var stripes: [STRIPES]Stripe = @splat(.{});

fn stripeOf(ino: u64) *Stripe {
    return &stripes[@intCast(dlm.splitmix64(ino) & (STRIPES - 1))];
}

/// The generation a reader compares its view against. Acquire pairs with
/// `touch`'s release: a reader that sees the new generation sees the
/// snapshot the bump followed.
pub fn genOf(ino: u64) u64 {
    return stripeOf(ino).gen.load(.acquire);
}

/// A record of `ino` became visible (or left): the stripe moves on.
pub fn touch(ino: u64) void {
    _ = stripeOf(ino).gen.fetchAdd(1, .release);
}

/// `touch` for a leaf record's key: the ino-major kinds whose records a
/// view caches (inode, xattr); every other key is nobody's view.
pub fn touchKey(k: []const u8) void {
    if (inoOfViewedKey(k)) |ino| touch(ino);
}

/// Every stripe (a batch too wide to name its inos one by one).
pub fn touchAll() void {
    for (&stripes) |*s| _ = s.gen.fetchAdd(1, .release);
}

/// The ino of an inode or xattr key (`ino ‖ kind ‖ …`), null for any
/// other key shape.
pub fn inoOfViewedKey(k: []const u8) ?u64 {
    if (k.len <= key.INO_MAJOR_KIND_OFF) return null;
    const kind = k[key.INO_MAJOR_KIND_OFF];
    if (kind != key.TREE_INODES and kind != key.TREE_XATTRS) return null;
    return std.mem.readInt(u64, k[0..8], .big);
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the witness moves on an inode or xattr key of the ino and on nothing else; touchAll moves every stripe" {
    const ino: u64 = 0x5107_1234;
    const g0 = genOf(ino);
    touchKey(&key.inodeKey(ino));
    try testing.expectEqual(g0 + 1, genOf(ino));
    touchKey(&key.xattrKey(ino, 77, 0));
    try testing.expectEqual(g0 + 2, genOf(ino));
    // A dentry under the ino, a block-map key, a ref key: not a viewed record.
    touchKey(&key.dentryKey(ino, 5, 0));
    touchKey(&(key.blockMapKey(ino, 3) catch unreachable));
    touchKey(&key.blockRefKey(1, 2, ino, 0));
    touchKey(&.{});
    try testing.expectEqual(g0 + 2, genOf(ino));
    try testing.expectEqual(@as(?u64, ino), inoOfViewedKey(&key.inodeKey(ino)));
    try testing.expectEqual(@as(?u64, null), inoOfViewedKey(&key.dentryKey(ino, 5, 0)));
    const other: u64 = 0x5107_4321;
    const h0 = genOf(other);
    touchAll();
    try testing.expectEqual(h0 + 1, genOf(other));
    try testing.expectEqual(g0 + 3, genOf(ino));
}
