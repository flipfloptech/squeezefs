//! MetaLV keys (charter §5.1 "Keys"; `docs/porting/kv_record_bset.md` §1).
//!
//! Every key is a big-endian composite so that memcmp order IS logical
//! order: the tree, the bset binary search and the n-way merge compare raw
//! bytes and never decode a key (`order`). The kind byte is the historical
//! tree id and it is PRESENT ON EVERY KEY — there is ONE key layout, the
//! charter's (format v4; decision 0002 row 35): INSERTED at offset 8 for the
//! ino-major family (an inode, its xattrs and its children's dentries share
//! one leaf region) and PREPENDED for block refs (refs sort by block so
//! `refcount(block)` is one range probe, charter §5.6), so one mixed-kind
//! tree per routing slot stays memcmp-ordered per kind and refs sort after
//! every ino-major key.
//!
//! Every length here is a format law pinned by byte-literal tests; the
//! decoders are total (any byte string yields a key or a typed refusal).

const std = @import("std");
const checksum = @import("../core/checksum.zig");
const counters = @import("../core/counters.zig");

const assert = std.debug.assert;

/// Structural corruption: a key the codec never produced.
pub const DecodeError = error{Corrupt};

// ---------------------------------------------------------------------------
// §1.1 Kind byte == historical tree id.
// ---------------------------------------------------------------------------

/// Interior record of a slot tree (legal iff level ≥ 1); also the node
/// header `tree_id` of every slot-tree node (a CLASS byte there — a slot
/// tree is named by its slot, never by this byte; the ledger names roots by
/// `ledger.RootId`).
pub const KIND_INTERIOR: u8 = 0;
pub const TREE_INODES: u8 = 1;
pub const TREE_DENTRIES: u8 = 2;
pub const TREE_XATTRS: u8 = 3;
/// Heap-allocator deltas (`alloc_ext`), journaled under 8 B extent keys.
pub const TREE_ALLOC_RESERVED: u8 = 4;
/// Reserved for the DATA-allocation deltas of rung 6 (decision 0002 row 44):
/// a distinct kind, never a key-length arm under kind 4.
pub const TREE_DATA_ALLOC_RESERVED: u8 = 5;
/// Durable block refs.
pub const TREE_BLOCK_REFS: u8 = 6;
/// Block-map tree (capability `BLOCK_MAP_TREE`).
pub const TREE_BLOCK_MAP: u8 = 7;
/// Tree 0 of the forest; NEVER a slot-tree kind.
pub const TREE_CONTROL: u8 = 8;
/// Reserved; its own tree, never a slot-tree kind.
pub const TREE_SHARED_INDEX: u8 = 9;
/// The journal tag's low nibble bounds tree ids at 15; 9 is the last one
/// assigned.
pub const TREE_ID_MAX: u8 = TREE_SHARED_INDEX;

comptime {
    // The tag byte packs `tree_id | level << 4`: ids must fit the nibble.
    assert(TREE_ID_MAX <= 0x0F);
}

/// Is `kind` a content family that lives INSIDE a slot tree? Exactly
/// {1,2,3,6,7} — this set IS the forest's meaning: a new slot-tree kind
/// needs a NEW incompat bit so an older binary refuses the volume instead
/// of skipping (and fsck-deleting) records it cannot name.
pub fn isSlotTreeKind(kind: u8) bool {
    return slotTreeKeyLen(kind) != null;
}

/// The key length of a slot-tree kind; null for any other kind.
pub fn slotTreeKeyLen(kind: u8) ?usize {
    return switch (kind) {
        TREE_INODES => INODE_KEY_LEN,
        TREE_DENTRIES => DENTRY_KEY_LEN,
        TREE_XATTRS => XATTR_KEY_LEN,
        TREE_BLOCK_MAP => BLOCK_MAP_KEY_LEN,
        TREE_BLOCK_REFS => BLOCK_REF_KEY_LEN,
        else => null,
    };
}

/// Journal record tag: `tree_id | (level << 4)`.
pub fn tagFor(tree_id: u8, level: u8) u8 {
    assert(tagTreeIdAdmissible(tree_id, level));
    // Level rides the high nibble; 15 levels address far more than the
    // depth-≤ 3 design envelope (≈ 7,000 fanout at 256 KiB).
    assert(level <= 0x0F);
    return tree_id | (level << 4);
}

/// A tag any writer produces: a tree id in `1..=TREE_ID_MAX` at any level,
/// or the interior marker at an interior level.
pub fn tagTreeIdAdmissible(tree_id: u8, level: u8) bool {
    return (tree_id >= TREE_INODES and tree_id <= TREE_ID_MAX) or (tree_id == KIND_INTERIOR and level >= 1);
}

pub const Tag = struct { tree_id: u8, level: u8 };

/// Inverse of `tagFor`: `(tag & 0x0F, tag >> 4)`.
pub fn untag(tag: u8) Tag {
    return .{ .tree_id = tag & 0x0F, .level = tag >> 4 };
}

// ---------------------------------------------------------------------------
// Memcmp order — the ONE comparison every key consumer uses.
// ---------------------------------------------------------------------------

/// Keys compare by memcmp (a proper prefix sorts first). Because every
/// field is big-endian, this equals the logical tuple order.
pub fn order(a: []const u8, b: []const u8) std.math.Order {
    return std.mem.order(u8, a, b);
}

pub fn lessThan(a: []const u8, b: []const u8) bool {
    return order(a, b) == .lt;
}

/// A fixed-capacity key the builders return without allocating. Capacity
/// = the longest key any encoder here produces: the 4-byte slot prefix of
/// an interior journal key plus the 32-byte `KEY_SPACE_MAX` separator.
pub const KEY_BUF_CAP: usize = INTERIOR_JOURNAL_SLOT_LEN + KEY_SPACE_MAX.len;

pub const KeyBuf = struct {
    buf: [KEY_BUF_CAP]u8 = undefined,
    len: u8 = 0,

    pub fn fromSlice(s: []const u8) KeyBuf {
        var k: KeyBuf = .{};
        k.appendSlice(s);
        return k;
    }

    pub fn slice(self: *const KeyBuf) []const u8 {
        return self.buf[0..self.len];
    }

    fn appendSlice(self: *KeyBuf, s: []const u8) void {
        assert(@as(usize, self.len) + s.len <= KEY_BUF_CAP);
        @memcpy(self.buf[self.len..][0..s.len], s);
        self.len += @intCast(s.len);
    }

    fn appendByte(self: *KeyBuf, b: u8) void {
        self.appendSlice(&.{b});
    }
};

fn readBe64(bytes: []const u8, off: usize) u64 {
    return std.mem.readInt(u64, bytes[off..][0..8], .big);
}

// ---------------------------------------------------------------------------
// §1.2 The keys — the kind byte at offset 8 (ino-major) or 0 (refs).
// ---------------------------------------------------------------------------

/// `ino ‖ 01`.
pub const INODE_KEY_LEN: usize = 8 + 1;
/// `parent ‖ 02 ‖ hash54 ‖ coll_seq`.
pub const DENTRY_KEY_LEN: usize = 8 + 1 + 8;
/// `ino ‖ 03 ‖ hash56 ‖ coll`.
pub const XATTR_KEY_LEN: usize = 8 + 1 + 8;
/// `owner ‖ 07 ‖ block_index`.
pub const BLOCK_MAP_KEY_LEN: usize = 8 + 1 + 4;
/// `06 ‖ vol_tag ‖ block_idx ‖ owner_ino ‖ block_index`.
pub const BLOCK_REF_KEY_LEN: usize = 1 + 8 + 8 + 8 + 4;
/// The refcount prefix `06 ‖ vol_tag ‖ block_idx` of a block-ref key:
/// `refcount(block)` = the population of this prefix (charter §5.6).
pub const BLOCK_REF_PREFIX_LEN: usize = 1 + 8 + 8;
/// The OWNER ino sits right after the refcount prefix; the forest routes a
/// reference by it.
pub const BLOCK_REF_OWNER_OFF: usize = BLOCK_REF_PREFIX_LEN;
/// The kind byte's offset in an ino-major key.
pub const INO_MAJOR_KIND_OFF: usize = 8;
/// `block_index` sentinel: "this ref is the indirect map blob". u32 indices
/// address 2^32 × 4 MiB = 16 PiB (charter §5.6 "Ceiling 16 PiB at 4 MiB
/// blocks"), so the top value is unreachable as a real index; the block-map
/// tree refuses it as a key for the same reason.
pub const BLOCK_INDEX_MAP_BLOB: u32 = std.math.maxInt(u32);

/// 54 hash bits leave room for `coll_seq` (8) + the readdir bias inside a
/// 62-bit, sign-clear i64 FUSE offset.
pub const HASH54_MAX: u64 = (1 << 54) - 1;
/// No cookie constraint on xattrs (`listxattr` has no resume): 56 bits.
pub const HASH56_MAX: u64 = (1 << 56) - 1;

/// Inode key: `[0..8) ino u64 BE ‖ [8] 0x01`.
pub const InodeKey = extern struct {
    ino_be: [8]u8,
    kind: u8,

    pub fn init(ino_: u64) InodeKey {
        var k: InodeKey = .{ .ino_be = undefined, .kind = TREE_INODES };
        std.mem.writeInt(u64, &k.ino_be, ino_, .big);
        return k;
    }

    pub fn ino(self: *const InodeKey) u64 {
        return std.mem.readInt(u64, &self.ino_be, .big);
    }

    pub fn decode(key: []const u8) DecodeError!InodeKey {
        if (key.len != INODE_KEY_LEN) return error.Corrupt;
        const k = std.mem.bytesToValue(InodeKey, key[0..INODE_KEY_LEN]);
        if (k.kind != TREE_INODES) return error.Corrupt;
        return k;
    }
};

/// Dentry key: `[0..8) parent_ino u64 BE ‖ [8] 0x02 ‖ [9..17) suffix u64
/// BE`, where `suffix = (hash54 << 8) | coll_seq` (62 bits; the top 2 bits
/// are structurally zero and refused on decode).
pub const DentryKey = extern struct {
    parent_ino_be: [8]u8,
    kind: u8,
    suffix_be: [8]u8,

    pub fn init(parent_ino_: u64, hash54_: u64, coll_seq: u8) DentryKey {
        var k: DentryKey = .{ .parent_ino_be = undefined, .kind = TREE_DENTRIES, .suffix_be = undefined };
        std.mem.writeInt(u64, &k.parent_ino_be, parent_ino_, .big);
        std.mem.writeInt(u64, &k.suffix_be, dentryKeySuffix(hash54_, coll_seq), .big);
        return k;
    }

    pub fn parentIno(self: *const DentryKey) u64 {
        return std.mem.readInt(u64, &self.parent_ino_be, .big);
    }

    pub fn suffix(self: *const DentryKey) u64 {
        return std.mem.readInt(u64, &self.suffix_be, .big);
    }

    pub fn hash54(self: *const DentryKey) u64 {
        return self.suffix() >> 8;
    }

    pub fn collSeq(self: *const DentryKey) u8 {
        return @truncate(self.suffix());
    }

    pub fn decode(key: []const u8) DecodeError!DentryKey {
        if (key.len != DENTRY_KEY_LEN) return error.Corrupt;
        const k = std.mem.bytesToValue(DentryKey, key[0..DENTRY_KEY_LEN]);
        if (k.kind != TREE_DENTRIES) return error.Corrupt;
        // 54 + 8 = 62 bits: the builder never sets the top two.
        if (k.suffix() >> 62 != 0) return error.Corrupt;
        return k;
    }
};

/// Xattr key: `[0..8) ino u64 BE ‖ [8] 0x03 ‖ [9..17) ((hash56 << 8) |
/// coll_seq) u64 BE`. 56 + 8 = 64: no structural-zero bits, hence no top-bit
/// check on decode.
pub const XattrKey = extern struct {
    ino_be: [8]u8,
    kind: u8,
    suffix_be: [8]u8,

    pub fn init(ino_: u64, hash56_: u64, coll_seq: u8) XattrKey {
        assert(hash56_ <= HASH56_MAX);
        var k: XattrKey = .{ .ino_be = undefined, .kind = TREE_XATTRS, .suffix_be = undefined };
        std.mem.writeInt(u64, &k.ino_be, ino_, .big);
        std.mem.writeInt(u64, &k.suffix_be, ((hash56_ & HASH56_MAX) << 8) | coll_seq, .big);
        return k;
    }

    pub fn ino(self: *const XattrKey) u64 {
        return std.mem.readInt(u64, &self.ino_be, .big);
    }

    pub fn hash56(self: *const XattrKey) u64 {
        return std.mem.readInt(u64, &self.suffix_be, .big) >> 8;
    }

    pub fn collSeq(self: *const XattrKey) u8 {
        return @truncate(std.mem.readInt(u64, &self.suffix_be, .big));
    }

    pub fn decode(key: []const u8) DecodeError!XattrKey {
        if (key.len != XATTR_KEY_LEN) return error.Corrupt;
        const k = std.mem.bytesToValue(XattrKey, key[0..XATTR_KEY_LEN]);
        if (k.kind != TREE_XATTRS) return error.Corrupt;
        return k;
    }
};

/// Block-map key: `[0..8) owner_ino u64 BE ‖ [8] 0x07 ‖ [9..13) block_index
/// u32 BE`. `block_index == u32::MAX` is RESERVED (the MAP_BLOB sentinel):
/// refused at encode AND decode; it appears only as a range END bound.
pub const BlockMapKey = extern struct {
    owner_ino_be: [8]u8,
    kind: u8,
    block_index_be: [4]u8,

    pub fn init(owner_ino_: u64, block_index_: u32) DecodeError!BlockMapKey {
        if (block_index_ == BLOCK_INDEX_MAP_BLOB) return error.Corrupt;
        return initRaw(owner_ino_, block_index_);
    }

    /// The unchecked form the range bounds use (the reserved index as an
    /// inclusive END is exact: no record carries it).
    fn initRaw(owner_ino_: u64, block_index_: u32) BlockMapKey {
        var k: BlockMapKey = .{ .owner_ino_be = undefined, .kind = TREE_BLOCK_MAP, .block_index_be = undefined };
        std.mem.writeInt(u64, &k.owner_ino_be, owner_ino_, .big);
        std.mem.writeInt(u32, &k.block_index_be, block_index_, .big);
        return k;
    }

    pub fn ownerIno(self: *const BlockMapKey) u64 {
        return std.mem.readInt(u64, &self.owner_ino_be, .big);
    }

    pub fn blockIndex(self: *const BlockMapKey) u32 {
        return std.mem.readInt(u32, &self.block_index_be, .big);
    }

    pub fn decode(key: []const u8) DecodeError!BlockMapKey {
        if (key.len != BLOCK_MAP_KEY_LEN) return error.Corrupt;
        const k = std.mem.bytesToValue(BlockMapKey, key[0..BLOCK_MAP_KEY_LEN]);
        if (k.kind != TREE_BLOCK_MAP) return error.Corrupt;
        if (k.blockIndex() == BLOCK_INDEX_MAP_BLOB) return error.Corrupt;
        return k;
    }
};

/// Block-ref key: `[0] 0x06 ‖ [1..9) vol_tag ‖ [9..17) block_idx ‖ [17..25)
/// owner_ino (all u64 BE) ‖ [25..29) block_index u32 BE`.
pub const BlockRefKey = extern struct {
    kind: u8,
    vol_tag_be: [8]u8,
    block_idx_be: [8]u8,
    owner_ino_be: [8]u8,
    block_index_be: [4]u8,

    pub fn init(vol_tag_: u64, block_idx_: u64, owner_ino_: u64, block_index_: u32) BlockRefKey {
        var k: BlockRefKey = .{ .kind = TREE_BLOCK_REFS, .vol_tag_be = undefined, .block_idx_be = undefined, .owner_ino_be = undefined, .block_index_be = undefined };
        std.mem.writeInt(u64, &k.vol_tag_be, vol_tag_, .big);
        std.mem.writeInt(u64, &k.block_idx_be, block_idx_, .big);
        std.mem.writeInt(u64, &k.owner_ino_be, owner_ino_, .big);
        std.mem.writeInt(u32, &k.block_index_be, block_index_, .big);
        return k;
    }

    pub fn volTag(self: *const BlockRefKey) u64 {
        return std.mem.readInt(u64, &self.vol_tag_be, .big);
    }

    pub fn blockIdx(self: *const BlockRefKey) u64 {
        return std.mem.readInt(u64, &self.block_idx_be, .big);
    }

    pub fn ownerIno(self: *const BlockRefKey) u64 {
        return std.mem.readInt(u64, &self.owner_ino_be, .big);
    }

    pub fn blockIndex(self: *const BlockRefKey) u32 {
        return std.mem.readInt(u32, &self.block_index_be, .big);
    }

    /// This reference is the owner's indirect map blob, not a map entry.
    pub fn isMapBlob(self: *const BlockRefKey) bool {
        return self.blockIndex() == BLOCK_INDEX_MAP_BLOB;
    }

    pub fn decode(key: []const u8) DecodeError!BlockRefKey {
        if (key.len != BLOCK_REF_KEY_LEN) return error.Corrupt;
        const k = std.mem.bytesToValue(BlockRefKey, key[0..BLOCK_REF_KEY_LEN]);
        if (k.kind != TREE_BLOCK_REFS) return error.Corrupt;
        return k;
    }
};

comptime {
    assert(@sizeOf(InodeKey) == INODE_KEY_LEN);
    assert(@sizeOf(DentryKey) == DENTRY_KEY_LEN);
    assert(@sizeOf(XattrKey) == XATTR_KEY_LEN);
    assert(@sizeOf(BlockMapKey) == BLOCK_MAP_KEY_LEN);
    assert(@sizeOf(BlockRefKey) == BLOCK_REF_KEY_LEN);
    assert(@offsetOf(InodeKey, "kind") == INO_MAJOR_KIND_OFF);
    assert(@offsetOf(DentryKey, "kind") == INO_MAJOR_KIND_OFF);
    assert(@offsetOf(XattrKey, "kind") == INO_MAJOR_KIND_OFF);
    assert(@offsetOf(BlockMapKey, "kind") == INO_MAJOR_KIND_OFF);
    assert(@offsetOf(BlockRefKey, "kind") == 0);
    assert(@offsetOf(BlockRefKey, "owner_ino_be") == BLOCK_REF_OWNER_OFF);
}

pub fn inodeKey(ino: u64) [INODE_KEY_LEN]u8 {
    return std.mem.toBytes(InodeKey.init(ino));
}

pub fn decodeInodeKey(key: []const u8) DecodeError!u64 {
    return (try InodeKey.decode(key)).ino();
}

/// The packed 62-bit dentry key suffix `(hash54 << 8) | coll_seq` — the
/// same word the readdir cookie biases by 3, which is what makes cookies
/// stable: they ARE the key.
pub fn dentryKeySuffix(hash54: u64, coll_seq: u8) u64 {
    assert(hash54 <= HASH54_MAX);
    return ((hash54 & HASH54_MAX) << 8) | coll_seq;
}

pub fn dentryKey(parent_ino: u64, hash54: u64, coll_seq: u8) [DENTRY_KEY_LEN]u8 {
    return std.mem.toBytes(DentryKey.init(parent_ino, hash54, coll_seq));
}

pub fn xattrKey(ino: u64, hash56: u64, coll_seq: u8) [XATTR_KEY_LEN]u8 {
    return std.mem.toBytes(XattrKey.init(ino, hash56, coll_seq));
}

pub fn blockMapKey(owner_ino: u64, block_index: u32) DecodeError![BLOCK_MAP_KEY_LEN]u8 {
    return std.mem.toBytes(try BlockMapKey.init(owner_ino, block_index));
}

pub fn blockRefKey(vol_tag: u64, block_idx: u64, owner_ino: u64, block_index: u32) [BLOCK_REF_KEY_LEN]u8 {
    return std.mem.toBytes(BlockRefKey.init(vol_tag, block_idx, owner_ino, block_index));
}

/// Inclusive `[lo, hi]` bounds over `owner_ino`'s block-map keys from
/// `from_index` upward. `hi` carries the RESERVED index raw: no record can
/// carry it, so the bound is exact.
pub const BlockMapRange = struct { lo: [BLOCK_MAP_KEY_LEN]u8, hi: [BLOCK_MAP_KEY_LEN]u8 };

pub fn indexRangeFrom(owner_ino: u64, from_index: u32) BlockMapRange {
    return .{
        .lo = std.mem.toBytes(BlockMapKey.initRaw(owner_ino, from_index)),
        .hi = std.mem.toBytes(BlockMapKey.initRaw(owner_ino, BLOCK_INDEX_MAP_BLOB)),
    };
}

/// A half-open `[lo, hi)` key range.
pub const KeyRange = struct { lo: KeyBuf, hi: KeyBuf };

/// Every reference on one data volume. `hi` is the next tag's first key,
/// or — for the last tag, which has no successor — a key one byte LONGER
/// than any real key and all 0xFF (memcmp: a prefix precedes its
/// extensions, so it sorts strictly after every 29-byte key).
pub fn volumeRange(vol_tag: u64) KeyRange {
    const lo = KeyBuf.fromSlice(&blockRefKey(vol_tag, 0, 0, 0));
    if (vol_tag == std.math.maxInt(u64)) {
        const all_ff: [BLOCK_REF_KEY_LEN + 1]u8 = @splat(0xFF);
        return .{ .lo = lo, .hi = KeyBuf.fromSlice(&all_ff) };
    }
    return .{ .lo = lo, .hi = KeyBuf.fromSlice(&blockRefKey(vol_tag + 1, 0, 0, 0)) };
}

/// Every reference on one block. `hi` is the next block's first key, or the
/// successor of the volume's maximal key when `block_idx` is the last.
pub fn blockRange(vol_tag: u64, block_idx: u64) KeyRange {
    const lo = KeyBuf.fromSlice(&blockRefKey(vol_tag, block_idx, 0, 0));
    if (block_idx == std.math.maxInt(u64)) {
        const max_key = blockRefKey(vol_tag, block_idx, std.math.maxInt(u64), std.math.maxInt(u32));
        return .{ .lo = lo, .hi = keySuccessor(&max_key) };
    }
    return .{ .lo = lo, .hi = KeyBuf.fromSlice(&blockRefKey(vol_tag, block_idx + 1, 0, 0)) };
}

/// The durable identity of a data volume as 8 key bytes: `vol-` + exactly
/// 16 hex digits decodes to that u64 verbatim (the tag IS the id — no hash,
/// no collision argument); any other id (grandfathered device basenames)
/// hashes with unseeded xxh3-64. A key component, so it must be identical on
/// every mount forever: it derives from the id string alone.
pub fn volumeTag(volume_id: []const u8) u64 {
    if (std.mem.startsWith(u8, volume_id, "vol-")) {
        const hex = volume_id["vol-".len..];
        if (hex.len == 16) {
            if (parseHexU64Strict(hex)) |v| return v;
        }
    }
    return checksum.xxh3(volume_id);
}

/// Strict hex: ≥ 1 hex digits of either case after an optional leading `+`
/// — no whitespace, no `_` separators (which `std.fmt.parseInt` would admit
/// and so mint a tag another parser hashes instead).
fn parseHexU64Strict(s: []const u8) ?u64 {
    const digits = if (s.len > 0 and s[0] == '+') s[1..] else s;
    if (digits.len == 0 or digits.len > 16) return null;
    var v: u64 = 0;
    for (digits) |c| {
        const d = std.fmt.charToDigit(c, 16) catch return null;
        v = (v << 4) | d;
    }
    return v;
}

// ---------------------------------------------------------------------------
// §1.4 Slot derivation & the guest keyspace.
// ---------------------------------------------------------------------------

/// Bits of native local-ino space per volume: ≥ 2^40 ≈ 1.1×10¹² inos, an
/// order of magnitude past the ≥ 100 M-inode design cap.
pub const GUEST_NS_SHIFT: u6 = 40;
/// First guest-namespaced local ino; native locals live strictly below.
pub const GUEST_NS_BASE: u64 = 1 << GUEST_NS_SHIFT;

pub const ForestSlot = u32;
/// The native keyspace's slot tree (local inos < 2^40).
pub const NATIVE_FOREST_SLOT: ForestSlot = 0;

/// The forest slot of guest slot `slot`: `(slot + 1) << 40` is its ino base.
pub fn guestForestSlot(slot: u16) ForestSlot {
    return @as(ForestSlot, slot) + 1;
}

/// The native slot plus the u16 guest-slot namespace (= 65 536, the derived
/// routing width). A key ino at or above `(FOREST_SLOT_MAX + 1) << 40`
/// belongs to no slot and is refused at the one codec in both directions.
pub const FOREST_SLOT_MAX: ForestSlot = guestForestSlot(std.math.maxInt(u16));

comptime {
    // The classification ladder's soundness: every routing ino is below
    // 2^57, so an ino-major key's first byte is ≤ 0x01 and can never read
    // as the refs kind byte (0x06).
    assert((@as(u64, FOREST_SLOT_MAX) + 1) << GUEST_NS_SHIFT <= @as(u64, 0x02) << 56);
}

/// The guest-namespace key ino of guest slot `slot`'s raw local `local`.
pub fn guestLocalIno(slot: u16, local: u64) u64 {
    assert(local < GUEST_NS_BASE);
    return ((@as(u64, slot) + 1) << GUEST_NS_SHIFT) | local;
}

pub const GuestLocal = struct { slot: u16, local: u64 };

/// Inverse of `guestLocalIno`; null for a native (un-namespaced) local.
pub fn splitGuestLocal(local: u64) ?GuestLocal {
    if (local < GUEST_NS_BASE) return null;
    const slot = (local >> GUEST_NS_SHIFT) - 1;
    assert(slot <= std.math.maxInt(u16));
    return .{ .slot = @intCast(slot), .local = local & (GUEST_NS_BASE - 1) };
}

/// The forest slot of a key ino: its top 24 bits, UNCHECKED (the range-
/// bound builders pad with 0xFF on purpose; `checkedForestSlot` refuses).
pub fn forestSlotOfIno(key_ino: u64) ForestSlot {
    return @intCast(key_ino >> GUEST_NS_SHIFT);
}

pub fn checkedForestSlot(key_ino: u64) error{SlotOutOfNamespace}!ForestSlot {
    const slot = forestSlotOfIno(key_ino);
    if (slot > FOREST_SLOT_MAX) return error.SlotOutOfNamespace;
    return slot;
}

// ---------------------------------------------------------------------------
// §1.3 The slot-tree key codec: kind and slot of a raw key.
// ---------------------------------------------------------------------------

/// How a key a slot tree holds fails the codec — the defect CLASSES as a
/// typed error set. fsck's raw C1 repair deletes ONLY `MalformedKnownKind`
/// and `SlotOutOfNamespace` (a KNOWN kind in a shape the encoder never
/// produces); `UnknownKind` may be a LATER binary's record and `Truncated`
/// names no kind, so both are report-only.
pub const ForestKeyError = error{
    MalformedKnownKind,
    UnknownKind,
    Truncated,
    SlotOutOfNamespace,
};

/// The defect with its evidence (what the repair report prints).
pub const RawKeyDefect = union(enum) {
    malformed_known_kind: struct { kind: u8, want: usize, got: usize },
    unknown_kind: struct { kind: u8 },
    truncated: struct { got: usize },
    slot_out_of_namespace: struct { kind: u8, slot: ForestSlot },

    pub fn toError(self: RawKeyDefect) ForestKeyError {
        return switch (self) {
            .malformed_known_kind => error.MalformedKnownKind,
            .unknown_kind => error.UnknownKind,
            .truncated => error.Truncated,
            .slot_out_of_namespace => error.SlotOutOfNamespace,
        };
    }

    /// The raw C1 repair line: delete iff the key is provably garbage.
    pub fn repairable(self: RawKeyDefect) bool {
        return switch (self) {
            .malformed_known_kind, .slot_out_of_namespace => true,
            .unknown_kind, .truncated => false,
        };
    }
};

/// Offset of the ROUTING ino inside a key of `kind`: the owner for refs,
/// the leading ino otherwise.
pub fn routeOff(kind: u8) usize {
    return if (kind == TREE_BLOCK_REFS) BLOCK_REF_OWNER_OFF else 0;
}

const KeyShape = union(enum) { kind: u8, defect: RawKeyDefect };

/// The ONE refusal ladder: empty ⇒ Truncated; first byte 0x06 ⇒ refs;
/// first byte > 0x01 ⇒ neither family (a known ino-major kind there is
/// Malformed, anything else Unknown); else the kind at offset 8 (missing ⇒
/// Truncated; 0x06 there ⇒ Malformed, refs are never ino-major); not a
/// slot-tree kind ⇒ Unknown; wrong length ⇒ Malformed; routing slot above
/// the namespace ⇒ SlotOutOfNamespace.
fn classifyKey(key: []const u8) KeyShape {
    if (key.len == 0) return .{ .defect = .{ .truncated = .{ .got = 0 } } };
    const first = key[0];
    var kind: u8 = undefined;
    if (first == TREE_BLOCK_REFS) {
        kind = TREE_BLOCK_REFS;
    } else if (first > 0x01) {
        if (slotTreeKeyLen(first)) |want| {
            return .{ .defect = .{ .malformed_known_kind = .{ .kind = first, .want = want, .got = key.len } } };
        }
        return .{ .defect = .{ .unknown_kind = .{ .kind = first } } };
    } else {
        if (key.len <= INO_MAJOR_KIND_OFF) return .{ .defect = .{ .truncated = .{ .got = key.len } } };
        kind = key[INO_MAJOR_KIND_OFF];
        if (kind == TREE_BLOCK_REFS) {
            return .{ .defect = .{ .malformed_known_kind = .{ .kind = TREE_BLOCK_REFS, .want = BLOCK_REF_KEY_LEN, .got = key.len } } };
        }
    }
    const want = slotTreeKeyLen(kind) orelse return .{ .defect = .{ .unknown_kind = .{ .kind = kind } } };
    if (key.len != want) {
        return .{ .defect = .{ .malformed_known_kind = .{ .kind = kind, .want = want, .got = key.len } } };
    }
    const slot = forestSlotOfIno(readBe64(key, routeOff(kind)));
    if (slot > FOREST_SLOT_MAX) return .{ .defect = .{ .slot_out_of_namespace = .{ .kind = kind, .slot = slot } } };
    return .{ .kind = kind };
}

/// The kind byte of a slot-tree key, or the typed refusal.
pub fn slotTreeKeyKind(key: []const u8) ForestKeyError!u8 {
    return switch (classifyKey(key)) {
        .kind => |k| k,
        .defect => |d| d.toError(),
    };
}

/// Classify a key the codec refuses; null iff it decodes.
pub fn classifyRefusedKey(key: []const u8) ?RawKeyDefect {
    return switch (classifyKey(key)) {
        .kind => null,
        .defect => |d| d,
    };
}

/// The slot a slot-tree key routes to: `key_ino >> 40` of the routing ino
/// (charter §5.5 "a record's slot is `key_ino >> 40` (refs by their owner
/// ino)"). Interior separator keys are real keys of the tree and route the
/// same way.
pub fn slotTreeKeySlot(key: []const u8) ForestKeyError!ForestSlot {
    const kind = try slotTreeKeyKind(key);
    // The ladder already refused an ino above the namespace.
    return forestSlotOfIno(readBe64(key, routeOff(kind)));
}

/// The commit-side agreement check: a staged `(kind, key)` whose key does
/// not decode as that kind would route to a foreign slot or fail at every
/// later read, so it is refused at the one choke point every tx passes.
pub fn checkSlotTreeKey(kind: u8, key: []const u8) ForestKeyError!ForestSlot {
    const got = try slotTreeKeyKind(key);
    if (got != kind) return error.MalformedKnownKind;
    return forestSlotOfIno(readBe64(key, routeOff(kind)));
}

// ---------------------------------------------------------------------------
// §1.5 Interior records.
// ---------------------------------------------------------------------------

/// Exclusive upper sentinel of every tree's key space: greater (memcmp)
/// than any legal key, and the separator every rightmost child shares.
pub const KEY_SPACE_MAX: [32]u8 = @splat(0xFF);

/// Interior VALUE (16 B LE): `[0..8) child_addr ‖ [8..16) child_node_seq`.
pub const InteriorValue = extern struct {
    child_addr_le: [8]u8,
    child_seq_le: [8]u8,

    pub const LEN: usize = 16;

    pub fn init(child_addr_: u64, child_seq_: u64) InteriorValue {
        var v: InteriorValue = undefined;
        std.mem.writeInt(u64, &v.child_addr_le, child_addr_, .little);
        std.mem.writeInt(u64, &v.child_seq_le, child_seq_, .little);
        return v;
    }

    pub fn childAddr(self: *const InteriorValue) u64 {
        return std.mem.readInt(u64, &self.child_addr_le, .little);
    }

    pub fn childSeq(self: *const InteriorValue) u64 {
        return std.mem.readInt(u64, &self.child_seq_le, .little);
    }

    pub fn decode(v: []const u8) DecodeError!InteriorValue {
        if (v.len != LEN) return error.Corrupt;
        return std.mem.bytesToValue(InteriorValue, v[0..LEN]);
    }
};

comptime {
    assert(@sizeOf(InteriorValue) == InteriorValue.LEN);
}

pub fn encodeInteriorValue(child_addr: u64, child_seq: u64) [InteriorValue.LEN]u8 {
    return std.mem.toBytes(InteriorValue.init(child_addr, child_seq));
}

/// The slot prefix on a slot tree's interior JOURNAL record key: a
/// separator alone cannot name the tree (every tree's top separator is
/// `KEY_SPACE_MAX`, and the refs family makes a slot's key space
/// non-contiguous), so the producer prefixes the slot and replay strips it.
pub const INTERIOR_JOURNAL_SLOT_LEN: usize = 4;

/// `slot u32 BE ‖ separator`.
pub fn interiorJournalKey(slot: ForestSlot, separator: []const u8) KeyBuf {
    var k: KeyBuf = .{};
    var slot_be: [INTERIOR_JOURNAL_SLOT_LEN]u8 = undefined;
    std.mem.writeInt(u32, &slot_be, slot, .big);
    k.appendSlice(&slot_be);
    k.appendSlice(separator);
    return k;
}

pub const InteriorJournalKey = struct { slot: ForestSlot, separator: []const u8 };

/// Inverse of `interiorJournalKey`; refuses a key too short to carry a
/// separator (an interior separator is never empty).
pub fn splitInteriorJournalKey(key: []const u8) DecodeError!InteriorJournalKey {
    if (key.len <= INTERIOR_JOURNAL_SLOT_LEN) return error.Corrupt;
    return .{
        .slot = std.mem.readInt(u32, key[0..INTERIOR_JOURNAL_SLOT_LEN], .big),
        .separator = key[INTERIOR_JOURNAL_SLOT_LEN..],
    };
}

/// `key ‖ 0x00`: the smallest key strictly greater than `key`. Sibling
/// nodes partition the key space with it (`right.min = successor(left.max)`)
/// so the revalidation predicate `min ≤ key ≤ max` leaves no unroutable gap.
pub fn keySuccessor(key: []const u8) KeyBuf {
    var k = KeyBuf.fromSlice(key);
    k.appendByte(0);
    return k;
}

// ---------------------------------------------------------------------------
// §1.6 Seeded name hashes — the per-volume `hash_seed` keys them so names
// cannot be chosen off-line to collide.
// ---------------------------------------------------------------------------

/// `xxh3_64_with_seed(name, seed) >> 10`: the top 54 bits.
pub fn dentryNameHash54(name: []const u8, hash_seed: u64) u64 {
    return checksum.xxh3Seeded(hash_seed, name) >> 10;
}

/// `xxh3_64_with_seed(name, seed) >> 8`: the top 56 bits.
pub fn xattrNameHash56(name: []const u8, hash_seed: u64) u64 {
    return checksum.xxh3Seeded(hash_seed, name) >> 8;
}

// ---------------------------------------------------------------------------
// §1.7 `coll_seq` probing: same-hash names probe 0..=255 comparing FULL
// names; the 257th same-hash name fails clean. Expected colliding pairs in
// a 1 M-entry directory ≈ n²/2^55 ≈ 2.8×10⁻⁵, so the chain is a safety
// valve, never a hot path.
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// `meta_kv_dentry_collision_overflows`: a 257th same-`(parent, hash54)`
    /// name found every `coll_seq` occupied and the insert was refused.
    pub var meta_kv_dentry_collision_overflows: counters.ShardedCounter = .{};
};

/// Lowest `coll_seq` absent from `occupied` (any order; holes are refilled),
/// or null when all 256 are taken. A 256-bit bitmap: bounded by the u8
/// domain by construction.
pub fn firstFreeCollSeq(occupied: []const u8) ?u8 {
    var bits: [4]u64 = @splat(0);
    for (occupied) |c| bits[c >> 6] |= @as(u64, 1) << @truncate(c);
    for (bits, 0..) |word, w| {
        const free = ~word;
        if (free != 0) return @intCast(w * 64 + @ctz(free));
    }
    return null;
}

/// Assign the `coll_seq` of a NEW same-hash dentry. Callers first probe the
/// chain by full-name comparison (an existing name is a replace, not a new
/// slot). Exhaustion is a counted clean error, never a panic.
pub fn assignDentryCollSeq(occupied: []const u8) error{DentryChainOverflow}!u8 {
    return firstFreeCollSeq(occupied) orelse {
        stats.meta_kv_dentry_collision_overflows.inc();
        return error.DentryChainOverflow;
    };
}

// ---------------------------------------------------------------------------
// §1.8 Readdir cookie: `3 + dentry key suffix`. The payload is ≤ 62 bits,
// so the bias never overflows and the sign bit is always clear when FUSE
// surfaces the offset as i64 — the classic readdir bug class foreclosed.
// ---------------------------------------------------------------------------

/// Synthetic `.` is emitted with offset 1.
pub const READDIR_OFFSET_DOT: u64 = 1;
/// Synthetic `..` is emitted with offset 2.
pub const READDIR_OFFSET_DOTDOT: u64 = 2;
/// Real entries are biased past the reserved offsets 0/1/2.
pub const READDIR_COOKIE_BIAS: u64 = 3;

pub const ReaddirPos = union(enum) {
    /// Offset 0: emit `.`, `..`, then every entry.
    start,
    /// Offset 1: emit `..`, then every entry.
    after_dot,
    /// Offset 2: every entry.
    after_dotdot,
    /// Offset ≥ 3: entries whose key suffix is STRICTLY greater.
    after_entry: struct { hash54: u64, coll_seq: u8 },
};

pub fn encodeReaddirCookie(hash54: u64, coll_seq: u8) u64 {
    return READDIR_COOKIE_BIAS + dentryKeySuffix(hash54, coll_seq);
}

/// A cookie whose biased payload exceeds the 62-bit suffix space was never
/// issued by this filesystem; the bound is checked before any shift.
pub fn decodeReaddirCookie(cookie: u64) error{InvalidReaddirCookie}!ReaddirPos {
    return switch (cookie) {
        0 => .start,
        READDIR_OFFSET_DOT => .after_dot,
        READDIR_OFFSET_DOTDOT => .after_dotdot,
        else => blk: {
            const payload = cookie - READDIR_COOKIE_BIAS;
            if (payload > dentryKeySuffix(HASH54_MAX, std.math.maxInt(u8))) return error.InvalidReaddirCookie;
            break :blk .{ .after_entry = .{ .hash54 = payload >> 8, .coll_seq = @truncate(payload) } };
        },
    };
}

// ---------------------------------------------------------------------------
// Tests — these pin the on-disk format.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "tree ids pin the on-disk format and fit the journal tag nibble" {
    try testing.expectEqual(@as(u8, 0), KIND_INTERIOR);
    try testing.expectEqual(@as(u8, 1), TREE_INODES);
    try testing.expectEqual(@as(u8, 2), TREE_DENTRIES);
    try testing.expectEqual(@as(u8, 3), TREE_XATTRS);
    try testing.expectEqual(@as(u8, 4), TREE_ALLOC_RESERVED);
    try testing.expectEqual(@as(u8, 5), TREE_DATA_ALLOC_RESERVED);
    try testing.expectEqual(@as(u8, 6), TREE_BLOCK_REFS);
    try testing.expectEqual(@as(u8, 7), TREE_BLOCK_MAP);
    try testing.expectEqual(@as(u8, 8), TREE_CONTROL);
    try testing.expectEqual(@as(u8, 9), TREE_SHARED_INDEX);
    try testing.expectEqual(TREE_SHARED_INDEX, TREE_ID_MAX);
    for (0..256) |k| {
        const kind: u8 = @intCast(k);
        const in_set = kind == 1 or kind == 2 or kind == 3 or kind == 6 or kind == 7;
        try testing.expectEqual(in_set, isSlotTreeKind(kind));
    }
}

test "journal tag packs tree_id | level << 4 and untag inverts it; admissibility" {
    try testing.expectEqual(@as(u8, 0x21), tagFor(TREE_INODES, 2));
    try testing.expectEqual(@as(u8, 0x10), tagFor(KIND_INTERIOR, 1));
    try testing.expectEqual(Tag{ .tree_id = 7, .level = 3 }, untag(0x37));
    try testing.expect(tagTreeIdAdmissible(TREE_SHARED_INDEX, 0));
    try testing.expect(tagTreeIdAdmissible(KIND_INTERIOR, 1));
    try testing.expect(!tagTreeIdAdmissible(KIND_INTERIOR, 0));
    try testing.expect(!tagTreeIdAdmissible(10, 0));
    try testing.expect(!tagTreeIdAdmissible(0x0F, 2));
}

test "v4 key lengths: the kind byte is on EVERY key — 9/17/17/13/29; the slot-tree lengths table agrees" {
    try testing.expectEqual(@as(usize, 9), INODE_KEY_LEN);
    try testing.expectEqual(@as(usize, 17), DENTRY_KEY_LEN);
    try testing.expectEqual(@as(usize, 17), XATTR_KEY_LEN);
    try testing.expectEqual(@as(usize, 13), BLOCK_MAP_KEY_LEN);
    try testing.expectEqual(@as(usize, 29), BLOCK_REF_KEY_LEN);
    try testing.expectEqual(@as(usize, 17), BLOCK_REF_OWNER_OFF);
    try testing.expectEqual(@as(usize, 17), BLOCK_REF_PREFIX_LEN);
    try testing.expectEqual(@as(?usize, 9), slotTreeKeyLen(TREE_INODES));
    try testing.expectEqual(@as(?usize, 17), slotTreeKeyLen(TREE_DENTRIES));
    try testing.expectEqual(@as(?usize, 17), slotTreeKeyLen(TREE_XATTRS));
    try testing.expectEqual(@as(?usize, 29), slotTreeKeyLen(TREE_BLOCK_REFS));
    try testing.expectEqual(@as(?usize, 13), slotTreeKeyLen(TREE_BLOCK_MAP));
    try testing.expectEqual(@as(?usize, null), slotTreeKeyLen(TREE_CONTROL));
    try testing.expectEqual(@as(?usize, null), slotTreeKeyLen(KIND_INTERIOR));
    try testing.expectEqual(@as(usize, 36), KEY_BUF_CAP);
}

test "inode key is ino BE ‖ 01, memcmp-ordered; wrong lengths and a wrong kind byte refuse" {
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 1, 0x01 }, &inodeKey(1));
    try testing.expectEqual(@as(u64, 0xDEAD_BEEF_F00D), try decodeInodeKey(&inodeKey(0xDEAD_BEEF_F00D)));
    var prng = std.Random.DefaultPrng.init(1);
    const r = prng.random();
    for (0..1000) |_| {
        const a = r.int(u64);
        const b = r.int(u64);
        try testing.expectEqual(std.math.order(a, b), order(&inodeKey(a), &inodeKey(b)));
    }
    try testing.expectError(error.Corrupt, decodeInodeKey(&@as([8]u8, @splat(0))));
    try testing.expectError(error.Corrupt, decodeInodeKey(&@as([10]u8, @splat(0))));
    var wrong_kind = inodeKey(5);
    wrong_kind[8] = TREE_DENTRIES;
    try testing.expectError(error.Corrupt, decodeInodeKey(&wrong_kind));
}

test "dentry key is parent ‖ 02 ‖ (hash54 << 8 | coll); orders parent, then hash54, then coll_seq; the suffix's top two bits are structurally clear" {
    const key = dentryKey(2, 0x2A5, 7);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 2 }, key[0..8]);
    try testing.expectEqual(TREE_DENTRIES, key[8]);
    var suffix_be: [8]u8 = undefined;
    std.mem.writeInt(u64, &suffix_be, (0x2A5 << 8) | 7, .big);
    try testing.expectEqualSlices(u8, &suffix_be, key[9..17]);
    try testing.expectEqual(@as(u64, 0), dentryKeySuffix(HASH54_MAX, 0xFF) >> 62);
    const d = try DentryKey.decode(&key);
    try testing.expectEqual(@as(u64, 2), d.parentIno());
    try testing.expectEqual(@as(u64, 0x2A5), d.hash54());
    try testing.expectEqual(@as(u8, 7), d.collSeq());

    var prng = std.Random.DefaultPrng.init(2);
    const r = prng.random();
    for (0..1000) |_| {
        const t1 = .{ r.int(u64), r.intRangeAtMost(u64, 0, HASH54_MAX), r.int(u8) };
        const t2 = .{ r.int(u64), r.intRangeAtMost(u64, 0, HASH54_MAX), r.int(u8) };
        var want = std.math.order(t1[0], t2[0]);
        if (want == .eq) want = std.math.order(t1[1], t2[1]);
        if (want == .eq) want = std.math.order(t1[2], t2[2]);
        try testing.expectEqual(want, order(&dentryKey(t1[0], t1[1], t1[2]), &dentryKey(t2[0], t2[1], t2[2])));
    }
    try testing.expectError(error.Corrupt, DentryKey.decode(key[0..16]));
    var bad = key;
    bad[9] |= 0x80;
    try testing.expectError(error.Corrupt, DentryKey.decode(&bad));
    bad = key;
    bad[8] = TREE_XATTRS;
    try testing.expectError(error.Corrupt, DentryKey.decode(&bad));
}

test "xattr key is ino ‖ 03 ‖ (hash56 << 8 | coll); orders ino, then hash56, then coll_seq; len 16 and a wrong kind refuse" {
    const key = xattrKey(9, 0xBEEF, 3);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 9 }, key[0..8]);
    try testing.expectEqual(TREE_XATTRS, key[8]);
    var suffix_be: [8]u8 = undefined;
    std.mem.writeInt(u64, &suffix_be, (0xBEEF << 8) | 3, .big);
    try testing.expectEqualSlices(u8, &suffix_be, key[9..17]);
    const x = try XattrKey.decode(&key);
    try testing.expectEqual(@as(u64, 9), x.ino());
    try testing.expectEqual(@as(u64, 0xBEEF), x.hash56());
    try testing.expectEqual(@as(u8, 3), x.collSeq());
    var prng = std.Random.DefaultPrng.init(3);
    const r = prng.random();
    for (0..1000) |_| {
        const t1 = .{ r.int(u64), r.intRangeAtMost(u64, 0, HASH56_MAX), r.int(u8) };
        const t2 = .{ r.int(u64), r.intRangeAtMost(u64, 0, HASH56_MAX), r.int(u8) };
        var want = std.math.order(t1[0], t2[0]);
        if (want == .eq) want = std.math.order(t1[1], t2[1]);
        if (want == .eq) want = std.math.order(t1[2], t2[2]);
        try testing.expectEqual(want, order(&xattrKey(t1[0], t1[1], t1[2]), &xattrKey(t2[0], t2[1], t2[2])));
    }
    try testing.expectError(error.Corrupt, XattrKey.decode(key[0..16]));
    // 56 + 8 = 64: a top bit set is a legal hash, not a structural refusal.
    var top = key;
    top[9] |= 0x80;
    _ = try XattrKey.decode(&top);
    var bad = key;
    bad[8] = TREE_DENTRIES;
    try testing.expectError(error.Corrupt, XattrKey.decode(&bad));
}

test "block-map key is owner BE ‖ 07 ‖ index BE; the reserved index refuses at encode and decode but bounds a range" {
    const key = try blockMapKey(0x0102, 0x0A0B0C0D);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 1, 2, 0x07, 0x0A, 0x0B, 0x0C, 0x0D }, &key);
    const k = try BlockMapKey.decode(&key);
    try testing.expectEqual(@as(u64, 0x0102), k.ownerIno());
    try testing.expectEqual(@as(u32, 0x0A0B0C0D), k.blockIndex());
    try testing.expectError(error.Corrupt, blockMapKey(1, BLOCK_INDEX_MAP_BLOB));
    try testing.expectError(error.Corrupt, BlockMapKey.decode(&[_]u8{ 0, 0, 0, 0, 0, 0, 0, 1, 0x07, 0xFF, 0xFF, 0xFF, 0xFF }));
    try testing.expectError(error.Corrupt, BlockMapKey.decode(key[0..12]));
    var bad = key;
    bad[8] = TREE_INODES;
    try testing.expectError(error.Corrupt, BlockMapKey.decode(&bad));
    const range = indexRangeFrom(5, 100);
    try testing.expectEqualSlices(u8, &(try blockMapKey(5, 100)), &range.lo);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 5, 0x07, 0xFF, 0xFF, 0xFF, 0xFF }, &range.hi);
    // Every legal index of ino 5 lies inside; nothing of ino 6 does.
    try testing.expect(order(&range.lo, &(try blockMapKey(5, 0xFFFF_FFFE))) == .lt);
    try testing.expect(order(&(try blockMapKey(5, 0xFFFF_FFFE)), &range.hi) == .lt);
    try testing.expect(order(&range.hi, &(try blockMapKey(6, 0))) == .lt);
}

test "block-ref key is 06 ‖ vol_tag ‖ block_idx ‖ owner ‖ index, all BE; the owner sits at offset 17; the refcount prefix is 17 B" {
    const key = blockRefKey(0x11, 0x22, 0x33, 0x44);
    try testing.expectEqual(@as(usize, 29), key.len);
    try testing.expectEqual(TREE_BLOCK_REFS, key[0]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 0x11 }, key[1..9]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 0x22 }, key[9..17]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 0x33 }, key[BLOCK_REF_OWNER_OFF..25]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0x44 }, key[25..29]);
    const r = try BlockRefKey.decode(&key);
    try testing.expectEqual(@as(u64, 0x11), r.volTag());
    try testing.expectEqual(@as(u64, 0x22), r.blockIdx());
    try testing.expectEqual(@as(u64, 0x33), r.ownerIno());
    try testing.expectEqual(@as(u32, 0x44), r.blockIndex());
    try testing.expect(!r.isMapBlob());
    try testing.expect((try BlockRefKey.decode(&blockRefKey(1, 2, 3, BLOCK_INDEX_MAP_BLOB))).isMapBlob());
    try testing.expectError(error.Corrupt, BlockRefKey.decode(key[0..28]));
    var bad = key;
    bad[0] = TREE_BLOCK_MAP;
    try testing.expectError(error.Corrupt, BlockRefKey.decode(&bad));
    // Two refs of one block share the 17 B refcount prefix whatever their owner.
    try testing.expectEqualSlices(u8, key[0..BLOCK_REF_PREFIX_LEN], blockRefKey(0x11, 0x22, 0x99, 0)[0..BLOCK_REF_PREFIX_LEN]);
}

test "volume and block ranges are half-open and cover exactly their prefix" {
    const vr = volumeRange(7);
    try testing.expectEqualSlices(u8, &blockRefKey(7, 0, 0, 0), vr.lo.slice());
    try testing.expectEqualSlices(u8, &blockRefKey(8, 0, 0, 0), vr.hi.slice());
    try testing.expect(order(&blockRefKey(7, std.math.maxInt(u64), std.math.maxInt(u64), std.math.maxInt(u32)), vr.hi.slice()) == .lt);
    // The last tag has no successor: 30 × 0xFF sorts after every 29-byte key.
    const last = volumeRange(std.math.maxInt(u64));
    try testing.expectEqual(@as(usize, 30), last.hi.slice().len);
    try testing.expect(order(&blockRefKey(std.math.maxInt(u64), std.math.maxInt(u64), std.math.maxInt(u64), std.math.maxInt(u32)), last.hi.slice()) == .lt);
    const br = blockRange(7, 9);
    try testing.expectEqualSlices(u8, &blockRefKey(7, 9, 0, 0), br.lo.slice());
    try testing.expectEqualSlices(u8, &blockRefKey(7, 10, 0, 0), br.hi.slice());
    const last_block = blockRange(7, std.math.maxInt(u64));
    const max_key = blockRefKey(7, std.math.maxInt(u64), std.math.maxInt(u64), std.math.maxInt(u32));
    try testing.expectEqualSlices(u8, keySuccessor(&max_key).slice(), last_block.hi.slice());
    try testing.expect(order(&max_key, last_block.hi.slice()) == .lt);
}

test "volume tag: vol-<16 hex> decodes verbatim; anything else hashes with unseeded xxh3" {
    try testing.expectEqual(@as(u64, 0x0123_4567_89AB_CDEF), volumeTag("vol-0123456789abcdef"));
    try testing.expectEqual(checksum.xxh3("vol-0123456789abcde"), volumeTag("vol-0123456789abcde"));
    try testing.expectEqual(checksum.xxh3("vol-0123456789abcdefg"), volumeTag("vol-0123456789abcdefg"));
    try testing.expectEqual(checksum.xxh3("vol-zz23456789abcdef"), volumeTag("vol-zz23456789abcdef"));
    try testing.expectEqual(checksum.xxh3("nvme0n1"), volumeTag("nvme0n1"));
    // Strict hex acceptance: either case; a leading `+`; never `_`
    // separators or whitespace.
    try testing.expectEqual(@as(u64, 0x0123_4567_89AB_CDEF), volumeTag("vol-0123456789ABCDEF"));
    try testing.expectEqual(@as(u64, 0x0_0123_4567_89AB_CDE), volumeTag("vol-+0123456789abcde"));
    try testing.expectEqual(checksum.xxh3("vol-0123_456789abcde"), volumeTag("vol-0123_456789abcde"));
    try testing.expectEqual(checksum.xxh3("vol- 123456789abcdef"), volumeTag("vol- 123456789abcdef"));
}

test "guest keyspace: (slot+1) << 40 | local, split inverts, natives are below 2^40" {
    try testing.expectEqual(@as(u64, 1 << 40), GUEST_NS_BASE);
    try testing.expectEqual(@as(u64, (1 << 40) | 5), guestLocalIno(0, 5));
    try testing.expectEqual(@as(u64, (@as(u64, 65536) << 40) | 0xFF_FFFF_FFFF), guestLocalIno(std.math.maxInt(u16), 0xFF_FFFF_FFFF));
    try testing.expectEqual(@as(?GuestLocal, null), splitGuestLocal(GUEST_NS_BASE - 1));
    try testing.expectEqual(GuestLocal{ .slot = 3, .local = 77 }, splitGuestLocal(guestLocalIno(3, 77)).?);
    try testing.expectEqual(@as(ForestSlot, 0), NATIVE_FOREST_SLOT);
    try testing.expectEqual(@as(ForestSlot, 65536), FOREST_SLOT_MAX);
    try testing.expectEqual(FOREST_SLOT_MAX, guestForestSlot(std.math.maxInt(u16)));
    try testing.expectEqual(@as(ForestSlot, 4), forestSlotOfIno(guestLocalIno(3, 0)));
    try testing.expectEqual(@as(ForestSlot, 0), forestSlotOfIno(GUEST_NS_BASE - 1));
    try testing.expectEqual(FOREST_SLOT_MAX, try checkedForestSlot(@as(u64, FOREST_SLOT_MAX) << 40));
    try testing.expectError(error.SlotOutOfNamespace, checkedForestSlot(@as(u64, FOREST_SLOT_MAX + 1) << 40));
}

test "every key names its kind and routes by its ino: ino-major kinds by the leading ino, refs by the owner; refs sort after every ino-major key" {
    const ino = guestLocalIno(1, 0x42);
    const ik = inodeKey(ino);
    const dk = dentryKey(ino, 0x1234, 9);
    const xk = xattrKey(ino, 0x55, 1);
    const mk = try blockMapKey(ino, 3);
    const rk = blockRefKey(0xAAAA, 0xBBBB, ino, 7);
    try testing.expectEqualSlices(u8, &inodeKey(ino), rk[BLOCK_REF_OWNER_OFF..][0..8] ++ [_]u8{TREE_INODES});
    inline for (.{ .{ TREE_INODES, &ik }, .{ TREE_DENTRIES, &dk }, .{ TREE_XATTRS, &xk }, .{ TREE_BLOCK_MAP, &mk }, .{ TREE_BLOCK_REFS, &rk } }) |pair| {
        try testing.expectEqual(@as(u8, pair[0]), try slotTreeKeyKind(pair[1]));
        try testing.expectEqual(@as(ForestSlot, 2), try slotTreeKeySlot(pair[1]));
        try testing.expectEqual(@as(ForestSlot, 2), try checkSlotTreeKey(pair[0], pair[1]));
        try testing.expectEqual(@as(?RawKeyDefect, null), classifyRefusedKey(pair[1]));
    }
    // ino 1's records lead and sit together: inode < dentries < xattrs < map.
    try testing.expect(order(&inodeKey(1), &dentryKey(1, 0, 0)) == .lt);
    try testing.expect(order(&dentryKey(1, HASH54_MAX, 0xFF), &xattrKey(1, 0, 0)) == .lt);
    try testing.expect(order(&xattrKey(1, HASH56_MAX, 0xFF), &(try blockMapKey(1, 0))) == .lt);
    try testing.expect(order(&(try blockMapKey(1, 0xFFFF_FFFE)), &inodeKey(2)) == .lt);
    // Refs sort after every ino-major key.
    try testing.expect(order(&xattrKey(std.math.maxInt(u64) >> 7, HASH56_MAX, 0xFF), &blockRefKey(0, 0, 0, 0)) == .lt);
    try testing.expect(order(&xk, &rk) == .lt);
    // The kind/key agreement check refuses a mismatched pair.
    try testing.expectError(error.MalformedKnownKind, checkSlotTreeKey(TREE_DENTRIES, &ik));
    try testing.expectError(error.MalformedKnownKind, checkSlotTreeKey(TREE_INODES, &rk));
    try testing.expectError(error.UnknownKind, checkSlotTreeKey(TREE_CONTROL, "slot_state:7"));
}

test "the refusal ladder yields the defect classes: truncated, malformed known kind, unknown kind, slot out of namespace; the repair line" {
    // Empty ⇒ Truncated{0}.
    try testing.expectError(error.Truncated, slotTreeKeyKind(&.{}));
    try testing.expectEqual(RawKeyDefect{ .truncated = .{ .got = 0 } }, classifyRefusedKey(&.{}).?);
    // First byte > 0x01 that is a known ino-major kind: a shape it never takes.
    const bad_prefix = [_]u8{0x07} ++ @as([12]u8, @splat(0));
    try testing.expectError(error.MalformedKnownKind, slotTreeKeyKind(&bad_prefix));
    try testing.expectEqual(RawKeyDefect{ .malformed_known_kind = .{ .kind = 7, .want = 13, .got = 13 } }, classifyRefusedKey(&bad_prefix).?);
    // First byte > 0x01 that no kind claims.
    const unknown_prefix = [_]u8{0x09} ++ @as([8]u8, @splat(0));
    try testing.expectError(error.UnknownKind, slotTreeKeyKind(&unknown_prefix));
    try testing.expectEqual(RawKeyDefect{ .unknown_kind = .{ .kind = 9 } }, classifyRefusedKey(&unknown_prefix).?);
    // Ino-major with no kind byte (the pre-v4 kindless shape).
    const kindless = inodeKey(5)[0..8];
    try testing.expectError(error.Truncated, slotTreeKeyKind(kindless));
    try testing.expectEqual(RawKeyDefect{ .truncated = .{ .got = 8 } }, classifyRefusedKey(kindless).?);
    // 0x06 at offset 8: refs are by-block, never ino-major.
    const refs_at_8 = inodeKey(5)[0..8].* ++ [_]u8{0x06};
    try testing.expectError(error.MalformedKnownKind, slotTreeKeyKind(&refs_at_8));
    try testing.expectEqual(RawKeyDefect{ .malformed_known_kind = .{ .kind = 6, .want = 29, .got = 9 } }, classifyRefusedKey(&refs_at_8).?);
    // Kind byte that is another tree's id.
    const control = inodeKey(5)[0..8].* ++ [_]u8{TREE_CONTROL};
    try testing.expectError(error.UnknownKind, slotTreeKeyKind(&control));
    try testing.expectEqual(RawKeyDefect{ .unknown_kind = .{ .kind = 8 } }, classifyRefusedKey(&control).?);
    // Known kind, wrong length.
    const short_dentry = inodeKey(5)[0..8].* ++ [_]u8{TREE_DENTRIES} ++ @as([3]u8, @splat(0));
    try testing.expectError(error.MalformedKnownKind, slotTreeKeyKind(&short_dentry));
    try testing.expectEqual(RawKeyDefect{ .malformed_known_kind = .{ .kind = 2, .want = 17, .got = 12 } }, classifyRefusedKey(&short_dentry).?);
    const short_refs = [_]u8{0x06} ++ @as([20]u8, @splat(0));
    try testing.expectError(error.MalformedKnownKind, slotTreeKeyKind(&short_refs));
    try testing.expectEqual(RawKeyDefect{ .malformed_known_kind = .{ .kind = 6, .want = 29, .got = 21 } }, classifyRefusedKey(&short_refs).?);
    // Right kind, right length, routing ino above the namespace.
    const too_high = @as(u64, FOREST_SLOT_MAX + 1) << 40;
    const high_inode = inodeKey(too_high);
    try testing.expectError(error.SlotOutOfNamespace, slotTreeKeyKind(&high_inode));
    try testing.expectEqual(RawKeyDefect{ .slot_out_of_namespace = .{ .kind = 1, .slot = FOREST_SLOT_MAX + 1 } }, classifyRefusedKey(&high_inode).?);
    const high_refs = blockRefKey(1, 2, too_high, 3);
    try testing.expectError(error.SlotOutOfNamespace, slotTreeKeyKind(&high_refs));
    try testing.expectError(error.SlotOutOfNamespace, slotTreeKeySlot(&high_refs));
    try testing.expectError(error.SlotOutOfNamespace, checkSlotTreeKey(TREE_INODES, &high_inode));
    // The highest slot still decodes.
    try testing.expectEqual(FOREST_SLOT_MAX, try slotTreeKeySlot(&inodeKey(@as(u64, FOREST_SLOT_MAX) << 40)));
    // The repair line: only the two "known kind, impossible shape" classes.
    try testing.expect(classifyRefusedKey(&high_inode).?.repairable());
    try testing.expect(classifyRefusedKey(&short_dentry).?.repairable());
    try testing.expect(!classifyRefusedKey(&control).?.repairable());
    try testing.expect(!classifyRefusedKey(kindless).?.repairable());
}

test "key classification is total: every byte string yields a kind or a defect, the two views agree, and every decoded key is exactly its typed codec's" {
    var prng = std.Random.DefaultPrng.init(77);
    const r = prng.random();
    var buf: [40]u8 = undefined;
    for (0..20_000) |_| {
        const len = r.intRangeAtMost(usize, 0, buf.len);
        r.bytes(buf[0..len]);
        // Bias the first and kind bytes toward the interesting values.
        if (len > 0 and r.boolean()) buf[0] = r.intRangeAtMost(u8, 0, 8);
        if (len > 8 and r.boolean()) buf[8] = r.intRangeAtMost(u8, 0, 8);
        const key = buf[0..len];
        if (slotTreeKeyKind(key)) |kind| {
            try testing.expect(isSlotTreeKind(kind));
            try testing.expectEqual(@as(?RawKeyDefect, null), classifyRefusedKey(key));
            try testing.expectEqual(try slotTreeKeySlot(key), try checkSlotTreeKey(kind, key));
            // The typed codec of that kind accepts it (the dentry's two
            // structural-zero bits and the map-blob sentinel are the only
            // extra refusals a typed codec adds).
            switch (kind) {
                TREE_INODES => _ = try InodeKey.decode(key),
                TREE_XATTRS => _ = try XattrKey.decode(key),
                TREE_BLOCK_REFS => _ = try BlockRefKey.decode(key),
                TREE_DENTRIES => if (DentryKey.decode(key)) |_| {} else |_| try testing.expect(key[9] >> 6 != 0),
                TREE_BLOCK_MAP => if (BlockMapKey.decode(key)) |_| {} else |_| try testing.expectEqual(BLOCK_INDEX_MAP_BLOB, std.mem.readInt(u32, key[9..13], .big)),
                else => unreachable,
            }
        } else |err| {
            try testing.expectEqual(err, classifyRefusedKey(key).?.toError());
        }
    }
}

test "interior value is 16 B LE (child_addr ‖ child_seq); len ≠ 16 refuses" {
    const v = encodeInteriorValue(0x0102030405060708, 0x1112131415161718);
    try testing.expectEqualSlices(u8, &.{ 8, 7, 6, 5, 4, 3, 2, 1, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11 }, &v);
    const d = try InteriorValue.decode(&v);
    try testing.expectEqual(@as(u64, 0x0102030405060708), d.childAddr());
    try testing.expectEqual(@as(u64, 0x1112131415161718), d.childSeq());
    try testing.expectError(error.Corrupt, InteriorValue.decode(v[0..15]));
    try testing.expectError(error.Corrupt, InteriorValue.decode(&(v ++ [_]u8{0})));
}

test "KEY_SPACE_MAX is 32 × 0xFF and sorts above every leaf key; key successor appends 0x00" {
    try testing.expectEqual(@as(usize, 32), KEY_SPACE_MAX.len);
    for (KEY_SPACE_MAX) |b| try testing.expectEqual(@as(u8, 0xFF), b);
    try testing.expect(order(&blockRefKey(std.math.maxInt(u64), std.math.maxInt(u64), std.math.maxInt(u64), std.math.maxInt(u32)), &KEY_SPACE_MAX) == .lt);
    const s = keySuccessor(&inodeKey(5));
    try testing.expectEqualSlices(u8, &(inodeKey(5) ++ [_]u8{0}), s.slice());
    try testing.expect(order(&inodeKey(5), s.slice()) == .lt);
    try testing.expect(order(s.slice(), &inodeKey(6)) == .lt);
}

test "interior journal key is slot u32 BE ‖ separator; split refuses len ≤ 4" {
    const k = interiorJournalKey(0x01020304, &KEY_SPACE_MAX);
    try testing.expectEqual(@as(usize, 36), k.slice().len);
    try testing.expectEqualSlices(u8, &.{ 1, 2, 3, 4 }, k.slice()[0..4]);
    try testing.expectEqualSlices(u8, &KEY_SPACE_MAX, k.slice()[4..]);
    const split = try splitInteriorJournalKey(k.slice());
    try testing.expectEqual(@as(ForestSlot, 0x01020304), split.slot);
    try testing.expectEqualSlices(u8, &KEY_SPACE_MAX, split.separator);
    try testing.expectError(error.Corrupt, splitInteriorJournalKey(&.{ 0, 0, 0, 1 }));
    try testing.expectError(error.Corrupt, splitInteriorJournalKey(&.{}));
    _ = try splitInteriorJournalKey(&.{ 0, 0, 0, 1, 0xAB });
}

test "hash54/hash56 are the seeded xxh3's top bits; pinned against xxhash-rust vectors" {
    // xxhash-rust: xxh3_64("") and xxh3_64_with_seed("hello", 42), ("user.layout", 42).
    try testing.expectEqual(@as(u64, 0x2d06800538d394c2), checksum.xxh3(""));
    try testing.expectEqual(@as(u64, 0xbafa072f07db7937), checksum.xxh3Seeded(42, "hello"));
    try testing.expectEqual(@as(u64, 0x886617549b8fab20), checksum.xxh3Seeded(42, "user.layout"));
    try testing.expectEqual(@as(u64, 0xbafa072f07db7937 >> 10), dentryNameHash54("hello", 42));
    try testing.expectEqual(@as(u64, 0x886617549b8fab20 >> 8), xattrNameHash56("user.layout", 42));
    try testing.expect(dentryNameHash54("hello", 1) != dentryNameHash54("hello", 2));
    try testing.expect(xattrNameHash56("user.layout", 1) != xattrNameHash56("user.layout", 2));
    var name: [32]u8 = undefined;
    for (0..4096) |i| {
        const n = try std.fmt.bufPrint(&name, "name-{d}", .{i});
        try testing.expect(dentryNameHash54(n, 0xFEED) <= HASH54_MAX);
        try testing.expect(xattrNameHash56(n, 0xFEED) <= HASH56_MAX);
    }
}

test "first free coll_seq fills the lowest hole regardless of input order; all 256 ⇒ null" {
    try testing.expectEqual(@as(?u8, 0), firstFreeCollSeq(&.{}));
    try testing.expectEqual(@as(?u8, 2), firstFreeCollSeq(&.{ 0, 1 }));
    try testing.expectEqual(@as(?u8, 1), firstFreeCollSeq(&.{ 0, 2 }));
    try testing.expectEqual(@as(?u8, 0), firstFreeCollSeq(&.{5}));
    try testing.expectEqual(@as(?u8, 4), firstFreeCollSeq(&.{ 3, 1, 0, 2 }));
    var all: [256]u8 = undefined;
    for (&all, 0..) |*c, i| c.* = @intCast(i);
    try testing.expectEqual(@as(?u8, 255), firstFreeCollSeq(all[0..255]));
    try testing.expectEqual(@as(?u8, null), firstFreeCollSeq(&all));
    try testing.expectEqual(@as(?u8, 64), firstFreeCollSeq(all[0..64]));
}

test "dentry chain overflow at 256 is a clean counted error" {
    var all: [256]u8 = undefined;
    for (&all, 0..) |*c, i| c.* = @intCast(i);
    const before = stats.meta_kv_dentry_collision_overflows.load();
    try testing.expectEqual(@as(u8, 255), try assignDentryCollSeq(all[0..255]));
    try testing.expectEqual(before, stats.meta_kv_dentry_collision_overflows.load());
    try testing.expectError(error.DentryChainOverflow, assignDentryCollSeq(&all));
    try testing.expectEqual(before + 1, stats.meta_kv_dentry_collision_overflows.load());
}

test "readdir cookie: reserved offsets, bias 3, sign bit always clear, roundtrip == key suffix" {
    try testing.expectEqual(@as(u64, 3), encodeReaddirCookie(0, 0));
    try testing.expectEqual(ReaddirPos.start, try decodeReaddirCookie(0));
    try testing.expectEqual(ReaddirPos.after_dot, try decodeReaddirCookie(READDIR_OFFSET_DOT));
    try testing.expectEqual(ReaddirPos.after_dotdot, try decodeReaddirCookie(READDIR_OFFSET_DOTDOT));
    try testing.expectEqual(ReaddirPos{ .after_entry = .{ .hash54 = 0, .coll_seq = 0 } }, try decodeReaddirCookie(3));
    const max = encodeReaddirCookie(HASH54_MAX, 0xFF);
    try testing.expectEqual(@as(u64, (1 << 62) + 2), max);
    try testing.expect(@as(i64, @bitCast(max)) > 0);
    var prng = std.Random.DefaultPrng.init(4);
    const r = prng.random();
    for (0..10_000) |_| {
        const h = r.intRangeAtMost(u64, 0, HASH54_MAX);
        const c = r.int(u8);
        const cookie = encodeReaddirCookie(h, c);
        try testing.expect(cookie >= READDIR_COOKIE_BIAS);
        try testing.expect(@as(i64, @bitCast(cookie)) > 0);
        try testing.expectEqual(ReaddirPos{ .after_entry = .{ .hash54 = h, .coll_seq = c } }, try decodeReaddirCookie(cookie));
        // The cookie payload IS the dentry key's [9..17) word.
        try testing.expectEqual(dentryKeySuffix(h, c), cookie - READDIR_COOKIE_BIAS);
        const key = dentryKey(9, h, c);
        try testing.expectEqual(cookie - READDIR_COOKIE_BIAS, std.mem.readInt(u64, key[9..17], .big));
    }
    // Order-preserving: resume-by-strictly-greater needs cookie order == key order.
    for (0..1000) |_| {
        const t1 = .{ r.intRangeAtMost(u64, 0, HASH54_MAX), r.int(u8) };
        const t2 = .{ r.intRangeAtMost(u64, 0, HASH54_MAX), r.int(u8) };
        var want = std.math.order(t1[0], t2[0]);
        if (want == .eq) want = std.math.order(t1[1], t2[1]);
        try testing.expectEqual(want, std.math.order(encodeReaddirCookie(t1[0], t1[1]), encodeReaddirCookie(t2[0], t2[1])));
    }
}

test "readdir cookie decode rejects payloads we never issue" {
    try testing.expectError(error.InvalidReaddirCookie, decodeReaddirCookie(std.math.maxInt(u64)));
    const first_bad = READDIR_COOKIE_BIAS + (1 << 62);
    try testing.expectError(error.InvalidReaddirCookie, decodeReaddirCookie(first_bad));
    _ = try decodeReaddirCookie(first_bad - 1);
}
