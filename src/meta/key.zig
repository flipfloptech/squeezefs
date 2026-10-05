//! MetaLV keys (charter §5.1 "Keys"; `docs/porting/kv_record_bset.md` §1).
//!
//! Every key is a big-endian composite so that memcmp order IS logical
//! order: the tree, the bset binary search and the n-way merge compare raw
//! bytes and never decode a key (`order`). The kind byte is the historical
//! tree id. Two layouts exist on disk:
//!
//!   - LEGACY (per-kind trees, bit 17 absent): the composite alone;
//!   - FOREST (bit 17): the kind byte INSERTED at offset 8 for the ino-major
//!     family and PREPENDED for block refs, so one mixed-kind tree per
//!     routing slot stays memcmp-ordered per kind and refs sort after every
//!     ino-major key (their first byte is 0x06; an ino-major key's first byte
//!     is ≤ 0x01 because `(slot + 1) << 40 ≤ 2^56`).
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
/// header `tree_id` of every slot-tree node.
pub const KIND_INTERIOR: u8 = 0;
pub const TREE_INODES: u8 = 1;
pub const TREE_DENTRIES: u8 = 2;
pub const TREE_XATTRS: u8 = 3;
/// Reserved (snapshots).
pub const TREE_ALLOC_RESERVED: u8 = 4;
/// Reserved.
pub const TREE_BACKPTR_RESERVED: u8 = 5;
/// Durable block refs (incompat bit 8).
pub const TREE_BLOCK_REFS: u8 = 6;
/// Block-map tree (incompat bit 16).
pub const TREE_BLOCK_MAP: u8 = 7;
/// Tree 0 of a forest volume (incompat bit 17); NEVER a slot-tree kind.
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
/// {1,2,3,6,7} — this set IS incompat bit 17's meaning: a new slot-tree kind
/// needs a NEW incompat bit so an older binary refuses the volume instead
/// of skipping (and fsck-deleting) records it cannot name.
pub fn isSlotTreeKind(kind: u8) bool {
    return legacyKeyLen(kind) != null;
}

/// The legacy key length of a slot-tree kind; null for any other kind.
pub fn legacyKeyLen(kind: u8) ?usize {
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
// §1.2 Legacy (per-kind) keys — the shapes the per-kind codecs read.
// ---------------------------------------------------------------------------

pub const INODE_KEY_LEN: usize = 8;
pub const DENTRY_KEY_LEN: usize = 16;
pub const XATTR_KEY_LEN: usize = 16;
pub const BLOCK_MAP_KEY_LEN: usize = 12;
pub const BLOCK_REF_KEY_LEN: usize = 28;
/// The refcount prefix `vol_tag ‖ block_idx` of a block-ref key.
pub const BLOCK_REF_PREFIX_LEN: usize = 16;
/// The OWNER ino sits right after the refcount prefix; the forest routes a
/// reference by it.
pub const BLOCK_REF_OWNER_OFF: usize = BLOCK_REF_PREFIX_LEN;
/// `block_index` sentinel: "this ref is the indirect map blob". u32 indices
/// address 16 EiB at 4 MiB blocks, so the top value is unreachable as a real
/// index; the block-map tree refuses it as a key for the same reason.
pub const BLOCK_INDEX_MAP_BLOB: u32 = std.math.maxInt(u32);

/// 54 hash bits leave room for `coll_seq` (8) + the readdir bias inside a
/// 62-bit, sign-clear i64 FUSE offset.
pub const HASH54_MAX: u64 = (1 << 54) - 1;
/// No cookie constraint on xattrs (`listxattr` has no resume): 56 bits.
pub const HASH56_MAX: u64 = (1 << 56) - 1;

/// Inode key: `[0..8) ino u64 BE`.
pub const InodeKey = extern struct {
    ino_be: [8]u8,

    pub fn init(ino_: u64) InodeKey {
        var k: InodeKey = undefined;
        std.mem.writeInt(u64, &k.ino_be, ino_, .big);
        return k;
    }

    pub fn ino(self: *const InodeKey) u64 {
        return std.mem.readInt(u64, &self.ino_be, .big);
    }

    pub fn decode(key: []const u8) DecodeError!InodeKey {
        if (key.len != INODE_KEY_LEN) return error.Corrupt;
        return std.mem.bytesToValue(InodeKey, key[0..INODE_KEY_LEN]);
    }
};

/// Dentry key: `[0..8) parent_ino u64 BE ‖ [8..16) suffix u64 BE`, where
/// `suffix = (hash54 << 8) | coll_seq` (62 bits; the top 2 bits are
/// structurally zero and refused on decode).
pub const DentryKey = extern struct {
    parent_ino_be: [8]u8,
    suffix_be: [8]u8,

    pub fn init(parent_ino_: u64, hash54_: u64, coll_seq: u8) DentryKey {
        var k: DentryKey = undefined;
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
        // 54 + 8 = 62 bits: the builder never sets the top two.
        if (k.suffix() >> 62 != 0) return error.Corrupt;
        return k;
    }
};

/// Xattr key: `[0..8) ino u64 BE ‖ [8..16) ((hash56 << 8) | coll_seq) u64 BE`.
/// 56 + 8 = 64: no structural-zero bits, hence no top-bit check on decode.
pub const XattrKey = extern struct {
    ino_be: [8]u8,
    suffix_be: [8]u8,

    pub fn init(ino_: u64, hash56_: u64, coll_seq: u8) XattrKey {
        assert(hash56_ <= HASH56_MAX);
        var k: XattrKey = undefined;
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
        return std.mem.bytesToValue(XattrKey, key[0..XATTR_KEY_LEN]);
    }
};

/// Block-map key: `[0..8) owner_ino u64 BE ‖ [8..12) block_index u32 BE`.
/// `block_index == u32::MAX` is RESERVED (the MAP_BLOB sentinel): refused at
/// encode AND decode; it appears only as a range END bound.
pub const BlockMapKey = extern struct {
    owner_ino_be: [8]u8,
    block_index_be: [4]u8,

    pub fn init(owner_ino_: u64, block_index_: u32) DecodeError!BlockMapKey {
        if (block_index_ == BLOCK_INDEX_MAP_BLOB) return error.Corrupt;
        return initRaw(owner_ino_, block_index_);
    }

    /// The unchecked form the range bounds use (the reserved index as an
    /// inclusive END is exact: no record carries it).
    fn initRaw(owner_ino_: u64, block_index_: u32) BlockMapKey {
        var k: BlockMapKey = undefined;
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
        if (k.blockIndex() == BLOCK_INDEX_MAP_BLOB) return error.Corrupt;
        return k;
    }
};

/// Block-ref key: `[0..8) vol_tag ‖ [8..16) block_idx ‖ [16..24) owner_ino
/// (all u64 BE) ‖ [24..28) block_index u32 BE`.
pub const BlockRefKey = extern struct {
    vol_tag_be: [8]u8,
    block_idx_be: [8]u8,
    owner_ino_be: [8]u8,
    block_index_be: [4]u8,

    pub fn init(vol_tag_: u64, block_idx_: u64, owner_ino_: u64, block_index_: u32) BlockRefKey {
        var k: BlockRefKey = undefined;
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
        return std.mem.bytesToValue(BlockRefKey, key[0..BLOCK_REF_KEY_LEN]);
    }
};

comptime {
    assert(@sizeOf(InodeKey) == INODE_KEY_LEN);
    assert(@sizeOf(DentryKey) == DENTRY_KEY_LEN);
    assert(@sizeOf(XattrKey) == XATTR_KEY_LEN);
    assert(@sizeOf(BlockMapKey) == BLOCK_MAP_KEY_LEN);
    assert(@sizeOf(BlockRefKey) == BLOCK_REF_KEY_LEN);
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
/// extensions, so it sorts strictly after every 28-byte key).
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

/// Exactly Rust's `u64::from_str_radix(s, 16)` acceptance: an optional
/// leading `+`, then ≥ 1 hex digits of either case — no whitespace, no `_`
/// separators (which `std.fmt.parseInt` would admit and so mint a tag the
/// reference hashes instead).
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
// §1.3 Forest keys — the kind byte inserted.
// ---------------------------------------------------------------------------

pub const FOREST_INODE_KEY_LEN: usize = INODE_KEY_LEN + 1;
pub const FOREST_DENTRY_KEY_LEN: usize = DENTRY_KEY_LEN + 1;
pub const FOREST_XATTR_KEY_LEN: usize = XATTR_KEY_LEN + 1;
pub const FOREST_BLOCK_MAP_KEY_LEN: usize = BLOCK_MAP_KEY_LEN + 1;
pub const FOREST_BLOCK_REF_KEY_LEN: usize = BLOCK_REF_KEY_LEN + 1;
/// Owner ino of a forest block-ref key: one kind byte after the legacy
/// offset.
pub const FOREST_BLOCK_REF_OWNER_OFF: usize = 1 + BLOCK_REF_OWNER_OFF;

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

/// Offset of the ROUTING ino inside a forest key of `kind`: the owner for
/// refs, the leading ino otherwise.
pub fn forestRouteOff(kind: u8) usize {
    return if (kind == TREE_BLOCK_REFS) FOREST_BLOCK_REF_OWNER_OFF else 0;
}

fn legacyRouteOff(kind: u8) usize {
    return if (kind == TREE_BLOCK_REFS) BLOCK_REF_OWNER_OFF else 0;
}

fn checkLegacyShape(kind: u8, legacy: []const u8) ForestKeyError!void {
    const want = legacyKeyLen(kind) orelse return error.UnknownKind;
    if (legacy.len != want) return error.MalformedKnownKind;
}

/// Frame a legacy key as its forest key. Refuses a kind outside the
/// slot-tree set, a legacy key of the wrong length for its kind, and a
/// routing ino above the slot namespace — a mis-framed key would route to
/// a foreign slot or fail at every later read, so it is refused at the one
/// encoder.
pub fn forestKey(kind: u8, legacy: []const u8) ForestKeyError!KeyBuf {
    try checkLegacyShape(kind, legacy);
    _ = try checkedForestSlot(readBe64(legacy, legacyRouteOff(kind)));
    var out: KeyBuf = .{};
    if (kind == TREE_BLOCK_REFS) {
        out.appendByte(kind);
        out.appendSlice(legacy);
    } else {
        out.appendSlice(legacy[0..8]);
        out.appendByte(kind);
        out.appendSlice(legacy[8..]);
    }
    return out;
}

/// The slot a `(kind, LEGACY key)` pair routes to, without framing it —
/// the commit path's per-record probe.
pub fn legacyKeySlot(kind: u8, legacy: []const u8) ForestKeyError!ForestSlot {
    try checkLegacyShape(kind, legacy);
    return checkedForestSlot(readBe64(legacy, legacyRouteOff(kind)));
}

const KeyShape = union(enum) { kind: u8, defect: RawKeyDefect };

/// The ONE refusal ladder, in the reference's order: empty ⇒ Truncated;
/// first byte 0x06 ⇒ refs; first byte > 0x01 ⇒ neither family (a known
/// ino-major kind there is Malformed, anything else Unknown); else the kind
/// at offset 8 (missing ⇒ Truncated; 0x06 there ⇒ Malformed, refs are never
/// ino-major); not a slot-tree kind ⇒ Unknown; wrong length ⇒ Malformed;
/// routing slot above the namespace ⇒ SlotOutOfNamespace.
fn classifyKey(key: []const u8) KeyShape {
    if (key.len == 0) return .{ .defect = .{ .truncated = .{ .got = 0 } } };
    const first = key[0];
    var kind: u8 = undefined;
    if (first == TREE_BLOCK_REFS) {
        kind = TREE_BLOCK_REFS;
    } else if (first > 0x01) {
        if (legacyKeyLen(first)) |want| {
            return .{ .defect = .{ .malformed_known_kind = .{ .kind = first, .want = want + 1, .got = key.len } } };
        }
        return .{ .defect = .{ .unknown_kind = .{ .kind = first } } };
    } else {
        if (key.len <= 8) return .{ .defect = .{ .truncated = .{ .got = key.len } } };
        kind = key[8];
        if (kind == TREE_BLOCK_REFS) {
            return .{ .defect = .{ .malformed_known_kind = .{ .kind = TREE_BLOCK_REFS, .want = FOREST_BLOCK_REF_KEY_LEN, .got = key.len } } };
        }
    }
    const want = legacyKeyLen(kind) orelse return .{ .defect = .{ .unknown_kind = .{ .kind = kind } } };
    if (key.len != want + 1) {
        return .{ .defect = .{ .malformed_known_kind = .{ .kind = kind, .want = want + 1, .got = key.len } } };
    }
    const slot = forestSlotOfIno(readBe64(key, forestRouteOff(kind)));
    if (slot > FOREST_SLOT_MAX) return .{ .defect = .{ .slot_out_of_namespace = .{ .kind = kind, .slot = slot } } };
    return .{ .kind = kind };
}

/// The kind byte of a forest key WITHOUT materializing its legacy form.
pub fn forestKeyKind(key: []const u8) ForestKeyError!u8 {
    return switch (classifyKey(key)) {
        .kind => |k| k,
        .defect => |d| d.toError(),
    };
}

/// Classify a key the forest codec refuses; null iff it decodes.
pub fn classifyRefusedKey(key: []const u8) ?RawKeyDefect {
    return switch (classifyKey(key)) {
        .kind => null,
        .defect => |d| d,
    };
}

pub const SplitForestKey = struct { kind: u8, legacy: KeyBuf };

/// The exact inverse of `forestKey`.
pub fn splitForestKey(key: []const u8) ForestKeyError!SplitForestKey {
    const kind = try forestKeyKind(key);
    var legacy: KeyBuf = .{};
    if (kind == TREE_BLOCK_REFS) {
        legacy.appendSlice(key[1..]);
    } else {
        legacy.appendSlice(key[0..8]);
        legacy.appendSlice(key[9..]);
    }
    return .{ .kind = kind, .legacy = legacy };
}

/// The slot a forest key routes to: `key_ino >> 40` of the routing ino.
/// Interior separator keys are real keys of the tree and route the same way.
pub fn forestKeySlot(key: []const u8) ForestKeyError!ForestSlot {
    const kind = try forestKeyKind(key);
    // The ladder already refused an ino above the namespace.
    return forestSlotOfIno(readBe64(key, forestRouteOff(kind)));
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
    try testing.expectEqual(@as(u8, 5), TREE_BACKPTR_RESERVED);
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

test "inode key is big-endian and memcmp-ordered; wrong lengths refuse" {
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 1 }, &inodeKey(1));
    try testing.expectEqual(@as(u64, 0xDEAD_BEEF_F00D), try decodeInodeKey(&inodeKey(0xDEAD_BEEF_F00D)));
    var prng = std.Random.DefaultPrng.init(1);
    const r = prng.random();
    for (0..1000) |_| {
        const a = r.int(u64);
        const b = r.int(u64);
        try testing.expectEqual(std.math.order(a, b), order(&inodeKey(a), &inodeKey(b)));
    }
    try testing.expectError(error.Corrupt, decodeInodeKey(&@as([7]u8, @splat(0))));
    try testing.expectError(error.Corrupt, decodeInodeKey(&@as([9]u8, @splat(0))));
}

test "dentry key orders parent, then hash54, then coll_seq; the suffix's top two bits are structurally clear" {
    const key = dentryKey(2, 0x2A5, 7);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 2 }, key[0..8]);
    var suffix_be: [8]u8 = undefined;
    std.mem.writeInt(u64, &suffix_be, (0x2A5 << 8) | 7, .big);
    try testing.expectEqualSlices(u8, &suffix_be, key[8..16]);
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
    try testing.expectError(error.Corrupt, DentryKey.decode(key[0..15]));
    var bad = key;
    bad[8] |= 0x80;
    try testing.expectError(error.Corrupt, DentryKey.decode(&bad));
}

test "xattr key orders ino, then hash56, then coll_seq; len 12 refuses" {
    const key = xattrKey(9, 0xBEEF, 3);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 9 }, key[0..8]);
    var suffix_be: [8]u8 = undefined;
    std.mem.writeInt(u64, &suffix_be, (0xBEEF << 8) | 3, .big);
    try testing.expectEqualSlices(u8, &suffix_be, key[8..16]);
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
    try testing.expectError(error.Corrupt, XattrKey.decode(key[0..12]));
    // 56 + 8 = 64: a top bit set is a legal hash, not a structural refusal.
    var top = key;
    top[8] |= 0x80;
    _ = try XattrKey.decode(&top);
}

test "block-map key is owner BE ‖ index BE; the reserved index refuses at encode and decode but bounds a range" {
    const key = try blockMapKey(0x0102, 0x0A0B0C0D);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 1, 2, 0x0A, 0x0B, 0x0C, 0x0D }, &key);
    const k = try BlockMapKey.decode(&key);
    try testing.expectEqual(@as(u64, 0x0102), k.ownerIno());
    try testing.expectEqual(@as(u32, 0x0A0B0C0D), k.blockIndex());
    try testing.expectError(error.Corrupt, blockMapKey(1, BLOCK_INDEX_MAP_BLOB));
    try testing.expectError(error.Corrupt, BlockMapKey.decode(&[_]u8{ 0, 0, 0, 0, 0, 0, 0, 1, 0xFF, 0xFF, 0xFF, 0xFF }));
    try testing.expectError(error.Corrupt, BlockMapKey.decode(key[0..11]));
    const range = indexRangeFrom(5, 100);
    try testing.expectEqualSlices(u8, &(try blockMapKey(5, 100)), &range.lo);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 5, 0xFF, 0xFF, 0xFF, 0xFF }, &range.hi);
    // Every legal index of ino 5 lies inside; nothing of ino 6 does.
    try testing.expect(order(&range.lo, &(try blockMapKey(5, 0xFFFF_FFFE))) == .lt);
    try testing.expect(order(&(try blockMapKey(5, 0xFFFF_FFFE)), &range.hi) == .lt);
    try testing.expect(order(&range.hi, &(try blockMapKey(6, 0))) == .lt);
}

test "block-ref key is vol_tag ‖ block_idx ‖ owner ‖ index, all BE; the owner sits at offset 16" {
    const key = blockRefKey(0x11, 0x22, 0x33, 0x44);
    try testing.expectEqual(@as(usize, 28), key.len);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 0x11 }, key[0..8]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 0x22 }, key[8..16]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0, 0, 0, 0, 0x33 }, key[BLOCK_REF_OWNER_OFF..24]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0x44 }, key[24..28]);
    const r = try BlockRefKey.decode(&key);
    try testing.expectEqual(@as(u64, 0x11), r.volTag());
    try testing.expectEqual(@as(u64, 0x22), r.blockIdx());
    try testing.expectEqual(@as(u64, 0x33), r.ownerIno());
    try testing.expectEqual(@as(u32, 0x44), r.blockIndex());
    try testing.expect(!r.isMapBlob());
    try testing.expect((try BlockRefKey.decode(&blockRefKey(1, 2, 3, BLOCK_INDEX_MAP_BLOB))).isMapBlob());
    try testing.expectError(error.Corrupt, BlockRefKey.decode(key[0..27]));
}

test "volume and block ranges are half-open and cover exactly their prefix" {
    const vr = volumeRange(7);
    try testing.expectEqualSlices(u8, &blockRefKey(7, 0, 0, 0), vr.lo.slice());
    try testing.expectEqualSlices(u8, &blockRefKey(8, 0, 0, 0), vr.hi.slice());
    try testing.expect(order(&blockRefKey(7, std.math.maxInt(u64), std.math.maxInt(u64), std.math.maxInt(u32)), vr.hi.slice()) == .lt);
    // The last tag has no successor: 29 × 0xFF sorts after every 28-byte key.
    const last = volumeRange(std.math.maxInt(u64));
    try testing.expectEqual(@as(usize, 29), last.hi.slice().len);
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
    // Rust `from_str_radix` acceptance, exactly: either case; a leading `+`;
    // never `_` separators or whitespace.
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

test "forest keys: the kind byte is inserted at offset 8 for ino-major kinds and prepended for refs" {
    const ino = guestLocalIno(1, 0x42);
    const fi = try forestKey(TREE_INODES, &inodeKey(ino));
    try testing.expectEqual(@as(usize, FOREST_INODE_KEY_LEN), fi.slice().len);
    try testing.expectEqualSlices(u8, &inodeKey(ino), fi.slice()[0..8]);
    try testing.expectEqual(TREE_INODES, fi.slice()[8]);

    const dk = dentryKey(ino, 0x1234, 9);
    const fd = try forestKey(TREE_DENTRIES, &dk);
    try testing.expectEqual(@as(usize, FOREST_DENTRY_KEY_LEN), fd.slice().len);
    try testing.expectEqualSlices(u8, dk[0..8], fd.slice()[0..8]);
    try testing.expectEqual(TREE_DENTRIES, fd.slice()[8]);
    try testing.expectEqualSlices(u8, dk[8..16], fd.slice()[9..17]);

    const xk = xattrKey(ino, 0x55, 1);
    const fx = try forestKey(TREE_XATTRS, &xk);
    try testing.expectEqual(@as(usize, FOREST_XATTR_KEY_LEN), fx.slice().len);
    try testing.expectEqual(TREE_XATTRS, fx.slice()[8]);

    const mk = try blockMapKey(ino, 3);
    const fm = try forestKey(TREE_BLOCK_MAP, &mk);
    try testing.expectEqual(@as(usize, FOREST_BLOCK_MAP_KEY_LEN), fm.slice().len);
    try testing.expectEqual(TREE_BLOCK_MAP, fm.slice()[8]);
    try testing.expectEqualSlices(u8, mk[8..12], fm.slice()[9..13]);

    const rk = blockRefKey(0xAAAA, 0xBBBB, ino, 7);
    const fr = try forestKey(TREE_BLOCK_REFS, &rk);
    try testing.expectEqual(@as(usize, FOREST_BLOCK_REF_KEY_LEN), fr.slice().len);
    try testing.expectEqual(TREE_BLOCK_REFS, fr.slice()[0]);
    try testing.expectEqualSlices(u8, &rk, fr.slice()[1..29]);
    try testing.expectEqualSlices(u8, &inodeKey(ino), fr.slice()[FOREST_BLOCK_REF_OWNER_OFF..][0..8]);

    // Every forest key splits back to its legacy form and names its slot.
    inline for (.{ .{ TREE_INODES, fi }, .{ TREE_DENTRIES, fd }, .{ TREE_XATTRS, fx }, .{ TREE_BLOCK_MAP, fm }, .{ TREE_BLOCK_REFS, fr } }) |pair| {
        const split = try splitForestKey(pair[1].slice());
        try testing.expectEqual(@as(u8, pair[0]), split.kind);
        const re = try forestKey(pair[0], split.legacy.slice());
        try testing.expectEqualSlices(u8, pair[1].slice(), re.slice());
        try testing.expectEqual(@as(ForestSlot, 2), try forestKeySlot(pair[1].slice()));
        try testing.expectEqual(@as(ForestSlot, 2), try legacyKeySlot(pair[0], split.legacy.slice()));
        try testing.expectEqual(@as(u8, pair[0]), try forestKeyKind(pair[1].slice()));
        try testing.expectEqual(@as(?RawKeyDefect, null), classifyRefusedKey(pair[1].slice()));
    }

    // Both transforms preserve memcmp order among keys of one kind.
    var prng = std.Random.DefaultPrng.init(0x5EED);
    const r = prng.random();
    for (0..1000) |_| {
        const a = dentryKey(r.intRangeAtMost(u64, 0, @as(u64, FOREST_SLOT_MAX + 1) << 40 - 1), r.intRangeAtMost(u64, 0, HASH54_MAX), r.int(u8));
        const b = dentryKey(r.intRangeAtMost(u64, 0, @as(u64, FOREST_SLOT_MAX + 1) << 40 - 1), r.intRangeAtMost(u64, 0, HASH54_MAX), r.int(u8));
        const fa = try forestKey(TREE_DENTRIES, &a);
        const fb = try forestKey(TREE_DENTRIES, &b);
        try testing.expectEqual(order(&a, &b), order(fa.slice(), fb.slice()));
    }
    // Refs sort after every ino-major key.
    try testing.expect(order(fx.slice(), fr.slice()) == .lt);
}

test "forest key encoder refusals: kind outside the slot set, wrong legacy length, slot above the namespace" {
    try testing.expectError(error.UnknownKind, forestKey(TREE_CONTROL, &inodeKey(1)));
    try testing.expectError(error.UnknownKind, forestKey(KIND_INTERIOR, &inodeKey(1)));
    try testing.expectError(error.UnknownKind, forestKey(TREE_SHARED_INDEX, &inodeKey(1)));
    try testing.expectError(error.MalformedKnownKind, forestKey(TREE_INODES, &dentryKey(1, 2, 3)));
    try testing.expectError(error.MalformedKnownKind, forestKey(TREE_BLOCK_REFS, &inodeKey(1)));
    const too_high = @as(u64, FOREST_SLOT_MAX + 1) << 40;
    try testing.expectError(error.SlotOutOfNamespace, forestKey(TREE_INODES, &inodeKey(too_high)));
    try testing.expectError(error.SlotOutOfNamespace, forestKey(TREE_BLOCK_REFS, &blockRefKey(1, 2, too_high, 3)));
    try testing.expectError(error.SlotOutOfNamespace, legacyKeySlot(TREE_INODES, &inodeKey(too_high)));
    // The highest slot still frames.
    _ = try forestKey(TREE_INODES, &inodeKey(@as(u64, FOREST_SLOT_MAX) << 40));
}

test "forest_key_kind's refusal ladder yields the defect classes in the reference's order" {
    // Empty ⇒ Truncated{0}.
    try testing.expectError(error.Truncated, forestKeyKind(&.{}));
    try testing.expectEqual(RawKeyDefect{ .truncated = .{ .got = 0 } }, classifyRefusedKey(&.{}).?);
    // First byte > 0x01 that is a known ino-major kind: a shape it never takes.
    const bad_prefix = [_]u8{0x07} ++ @as([12]u8, @splat(0));
    try testing.expectError(error.MalformedKnownKind, forestKeyKind(&bad_prefix));
    try testing.expectEqual(RawKeyDefect{ .malformed_known_kind = .{ .kind = 7, .want = 13, .got = 13 } }, classifyRefusedKey(&bad_prefix).?);
    // First byte > 0x01 that no kind claims.
    const unknown_prefix = [_]u8{0x09} ++ @as([8]u8, @splat(0));
    try testing.expectError(error.UnknownKind, forestKeyKind(&unknown_prefix));
    try testing.expectEqual(RawKeyDefect{ .unknown_kind = .{ .kind = 9 } }, classifyRefusedKey(&unknown_prefix).?);
    // Ino-major with no kind byte.
    try testing.expectError(error.Truncated, forestKeyKind(&inodeKey(5)));
    try testing.expectEqual(RawKeyDefect{ .truncated = .{ .got = 8 } }, classifyRefusedKey(&inodeKey(5)).?);
    // 0x06 at offset 8: refs are by-block, never ino-major.
    const refs_at_8 = inodeKey(5) ++ [_]u8{0x06};
    try testing.expectError(error.MalformedKnownKind, forestKeyKind(&refs_at_8));
    try testing.expectEqual(RawKeyDefect{ .malformed_known_kind = .{ .kind = 6, .want = 29, .got = 9 } }, classifyRefusedKey(&refs_at_8).?);
    // Kind byte that is another tree's id.
    const control = inodeKey(5) ++ [_]u8{TREE_CONTROL};
    try testing.expectError(error.UnknownKind, forestKeyKind(&control));
    try testing.expectEqual(RawKeyDefect{ .unknown_kind = .{ .kind = 8 } }, classifyRefusedKey(&control).?);
    // Known kind, wrong length.
    const short_dentry = inodeKey(5) ++ [_]u8{TREE_DENTRIES} ++ @as([3]u8, @splat(0));
    try testing.expectError(error.MalformedKnownKind, forestKeyKind(&short_dentry));
    try testing.expectEqual(RawKeyDefect{ .malformed_known_kind = .{ .kind = 2, .want = 17, .got = 12 } }, classifyRefusedKey(&short_dentry).?);
    const short_refs = [_]u8{0x06} ++ @as([20]u8, @splat(0));
    try testing.expectError(error.MalformedKnownKind, forestKeyKind(&short_refs));
    try testing.expectEqual(RawKeyDefect{ .malformed_known_kind = .{ .kind = 6, .want = 29, .got = 21 } }, classifyRefusedKey(&short_refs).?);
    // Right kind, right length, routing ino above the namespace.
    const too_high = @as(u64, FOREST_SLOT_MAX + 1) << 40;
    const high_inode = inodeKey(too_high) ++ [_]u8{TREE_INODES};
    try testing.expectError(error.SlotOutOfNamespace, forestKeyKind(&high_inode));
    try testing.expectEqual(RawKeyDefect{ .slot_out_of_namespace = .{ .kind = 1, .slot = FOREST_SLOT_MAX + 1 } }, classifyRefusedKey(&high_inode).?);
    const high_refs = [_]u8{0x06} ++ blockRefKey(1, 2, too_high, 3);
    try testing.expectError(error.SlotOutOfNamespace, forestKeyKind(&high_refs));
    try testing.expectError(error.SlotOutOfNamespace, forestKeySlot(&high_refs));
    // The repair line: only the two "known kind, impossible shape" classes.
    try testing.expect(classifyRefusedKey(&high_inode).?.repairable());
    try testing.expect(classifyRefusedKey(&short_dentry).?.repairable());
    try testing.expect(!classifyRefusedKey(&control).?.repairable());
    try testing.expect(!classifyRefusedKey(&inodeKey(5)).?.repairable());
}

test "forest key classification is total: every byte string yields a kind or a defect, and the two views agree" {
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
        if (forestKeyKind(key)) |kind| {
            try testing.expect(isSlotTreeKind(kind));
            try testing.expectEqual(@as(?RawKeyDefect, null), classifyRefusedKey(key));
            const split = try splitForestKey(key);
            try testing.expectEqualSlices(u8, key, (try forestKey(split.kind, split.legacy.slice())).slice());
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
        // The cookie payload IS the dentry key's [8..16) word.
        try testing.expectEqual(dentryKeySuffix(h, c), cookie - READDIR_COOKIE_BIAS);
        const key = dentryKey(9, h, c);
        try testing.expectEqual(cookie - READDIR_COOKIE_BIAS, std.mem.readInt(u64, key[8..16], .big));
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
