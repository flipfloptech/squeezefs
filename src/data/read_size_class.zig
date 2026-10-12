//! The read path's ONE size-class module and the block key every tier is
//! addressed by (`docs/porting/data_read_path.md` §0, §2.2, §13 Q13;
//! charter §6.3). PORTABLE.
//!
//! Two boundaries exist on the read path and each has exactly one reason:
//! `SMALL_FILL_MAX` (256 KiB) is the read-LRU population bound — fills at
//! or under it land in the ≤ 256 KiB RAM LRU, above it in the hot tier —
//! and `INLINE_PUBLISH_MAX` (64 KiB) is the ranged bounce slab's slot
//! (`nvme_derive.ranged_buf_size`): a fill that fits one bounce slot is
//! published to the NVMe read cache inline, a larger one through the
//! deferred publish that rides the single-flight guard. The reference
//! carried the first as a "mirrored" constant pair and the second with no
//! reason on the line; here each is one declaration with its tie test.
//!
//! The block key is the device-offset pair `(vol_tag, offset)` — binary,
//! never a decorated string (decision 0002 row 105). A freed and
//! reallocated offset REUSES its key, which is why every tier serve is
//! incarnation- and binding-validated (`read_path.zig` §7).

const std = @import("std");
const derive = @import("../core/derive.zig");
const checksum = @import("../core/checksum.zig");
const nd = @import("../io/nvme_derive.zig");
const layout = @import("layout.zig");

/// The read-LRU population bound: one 10 GiB stream must not flush every
/// warm small entry, so whole-block fills (> this) never enter the small
/// LRU; EOF-short tails and small-block volumes take the small-fill path by
/// design (`routing.rs:677–681`; `docs/design-read-path.md:187, 221`).
pub const SMALL_FILL_MAX: u64 = 256 * derive.KiB;

/// Inline-publishable ⇔ fits the ranged bounce slab's slot: the publish
/// copies one slot's worth on the caller's thread and nothing larger.
pub const INLINE_PUBLISH_MAX: u64 = nd.ranged_buf_size;

/// The LBA grain of every ranged window and dest-lease alignment check:
/// `layout.GRAIN` (4096), the conservative O_DIRECT grain (approved OQ #1 —
/// no per-device `logical_block_size` probe until
/// `ranged_read_unaligned_bounces` shows real 512-native waste).
pub const LBA_GRAIN: u64 = layout.GRAIN;

/// A tier address: a block's device offset on its volume.
pub const BlockKey = extern struct {
    vol_tag: u64,
    offset: u64,

    pub fn init(vol_tag: u64, offset: u64) BlockKey {
        return .{ .vol_tag = vol_tag, .offset = offset };
    }

    pub fn fromMapping(m: layout.Mapping) BlockKey {
        return .{ .vol_tag = m.vol_tag, .offset = m.offset };
    }

    pub fn eql(a: BlockKey, b: BlockKey) bool {
        return a.vol_tag == b.vol_tag and a.offset == b.offset;
    }

    /// hash64 over the 16 key bytes: the shard index, the ghost slot and the
    /// cooldown slot all derive from this one hash.
    pub fn hash(self: BlockKey) u64 {
        return checksum.hash64(std.mem.asBytes(&self));
    }

    pub fn blockIdx(self: BlockKey, block_size: u64) u64 {
        return self.offset / block_size;
    }
};

comptime {
    std.debug.assert(@sizeOf(BlockKey) == 16);
}

/// The `std.AutoHashMap` context for `BlockKey` (hash64, not the default
/// Wyhash: one hash function per key across every tier and the ghost).
pub const KeyContext = struct {
    pub fn hash(_: KeyContext, k: BlockKey) u64 {
        return k.hash();
    }
    pub fn eql(_: KeyContext, a: BlockKey, b: BlockKey) bool {
        return a.eql(b);
    }
};

pub fn KeyMap(comptime V: type) type {
    return std.HashMapUnmanaged(BlockKey, V, KeyContext, std.hash_map.default_max_load_percentage);
}

/// `fill_len ≤ SMALL_FILL_MAX`: the read-LRU class.
pub fn isSmallFill(len: u64) bool {
    return len <= SMALL_FILL_MAX;
}

/// `fill_len ≤ INLINE_PUBLISH_MAX`: the inline-publish class.
pub fn isInlinePublish(len: u64) bool {
    return len <= INLINE_PUBLISH_MAX;
}

const testing = std.testing;

test "tie: the small-fill bound is 256 KiB (the read-LRU population bound); the inline-publish bound IS the ranged bounce slot" {
    try testing.expectEqual(@as(u64, 262_144), SMALL_FILL_MAX);
    try testing.expectEqual(@as(u64, nd.ranged_buf_size), INLINE_PUBLISH_MAX);
    try testing.expectEqual(@as(u64, 65_536), INLINE_PUBLISH_MAX);
    try testing.expect(INLINE_PUBLISH_MAX < SMALL_FILL_MAX);
    try testing.expectEqual(@as(u64, 4096), LBA_GRAIN);
    try testing.expect(isSmallFill(SMALL_FILL_MAX));
    try testing.expect(!isSmallFill(SMALL_FILL_MAX + 1));
    try testing.expect(isInlinePublish(INLINE_PUBLISH_MAX));
    try testing.expect(!isInlinePublish(INLINE_PUBLISH_MAX + 1));
}

test "BlockKey: equality, hash stability, map context" {
    const a = BlockKey.init(7, 4 << 20);
    const b = BlockKey.init(7, 4 << 20);
    const c = BlockKey.init(7, 8 << 20);
    try testing.expect(a.eql(b));
    try testing.expect(!a.eql(c));
    try testing.expectEqual(a.hash(), b.hash());
    try testing.expect(a.hash() != c.hash());
    try testing.expectEqual(@as(u64, 1), a.blockIdx(4 << 20));
    var m: KeyMap(u32) = .empty;
    defer m.deinit(testing.allocator);
    try m.put(testing.allocator, a, 1);
    try testing.expectEqual(@as(?u32, 1), m.get(b));
    try testing.expectEqual(@as(?u32, null), m.get(c));
    const from = BlockKey.fromMapping(layout.Mapping.wholeBlock(7, 4 << 20, 3));
    try testing.expect(from.eql(a));
}
