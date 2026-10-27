//! MetaLV records: value codecs, the record frame, and THE ONE fold algebra
//! (charter §5.1 "Record kinds and ONE fold algebra";
//! `docs/porting/kv_record_bset.md` §2–§3).
//!
//! Three record kinds — `Put(value)`, `Delta(payload)`, `Delete` — and one
//! fold shared byte-identically by point lookup, the bset n-way merge,
//! compaction and journal replay: `foldNewestFirst` is the algebra;
//! `compactFold` adds the tombstone-elision and lineage rules on top of it;
//! `foldForward` is the incremental step, pinned equal to the from-scratch
//! fold by property test. Every caller routes through these three.
//!
//! Two delta CLASSES share the `Delta` kind, discriminated by payload bytes:
//! the Δinode record (`mask u16 LE ‖ masked fields`, `InodeDelta`) and the
//! layout delta (`0x4C31` magic, `LayoutDelta`, whose base is a bincode-
//! serialized layout inside an xattr envelope). A chain mixing classes is
//! representable only by corruption and refuses loud.
//!
//! Decoders are TOTAL: every length and count in the wire is a claim
//! validated against its container before any byte is dereferenced and
//! never an allocation authority (design §9).

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const counters = @import("../core/counters.zig");
const key = @import("key.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;

/// Structural corruption: bytes no encoder of this layer produces.
pub const DecodeError = error{Corrupt};
/// A name longer than the u8 `name_len` can frame.
pub const EncodeError = error{NameTooLong};

pub const stats = struct {
    /// `meta_kv_delta_orphans`: Δ-without-base folds — delta records
    /// discarded because the scan exhausted every source without a `Put`
    /// or a shadowing `Delete`; a counted no-op reachable only through
    /// replay orderings. One count per orphaned record.
    pub var meta_kv_delta_orphans: counters.ShardedCounter = .{};
    /// Records decoded by a fold (the base Put plus every collected delta).
    /// Not on the stats JSON: the D7 zero-decode pin for head/memo serves,
    /// and the profiler's rate for the 16 %-of-daemon-CPU re-decode tax the
    /// reference measured before D7.
    pub var meta_kv_fold_record_decodes: counters.ShardedCounter = .{};
    /// `meta_kv_layout_delta_folds`: layout delta records applied onto a
    /// base at read/compaction/replay time, one per delta.
    pub var meta_kv_layout_delta_folds: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// Bounds-checked little-endian reader (design §9): every length is checked
// against the container before any byte is dereferenced.
// ---------------------------------------------------------------------------

const Reader = struct {
    buf: []const u8,
    pos: usize = 0,

    fn take(self: *Reader, n: usize) DecodeError![]const u8 {
        const end = std.math.add(usize, self.pos, n) catch return error.Corrupt;
        if (end > self.buf.len) return error.Corrupt;
        const out = self.buf[self.pos..end];
        self.pos = end;
        return out;
    }

    fn readU8(self: *Reader) DecodeError!u8 {
        return (try self.take(1))[0];
    }

    fn readU16(self: *Reader) DecodeError!u16 {
        return std.mem.readInt(u16, (try self.take(2))[0..2], .little);
    }

    fn readU32(self: *Reader) DecodeError!u32 {
        return std.mem.readInt(u32, (try self.take(4))[0..4], .little);
    }

    fn readU64(self: *Reader) DecodeError!u64 {
        return std.mem.readInt(u64, (try self.take(8))[0..8], .little);
    }

    fn remaining(self: *const Reader) []const u8 {
        return self.buf[self.pos..];
    }

    /// Encodings are exact: trailing bytes refuse.
    fn finish(self: *const Reader) DecodeError!void {
        if (self.pos != self.buf.len) return error.Corrupt;
    }
};

// ---------------------------------------------------------------------------
// §2.1 Inode value — 68 B, fixed width (format v4; decision 0002 row 4).
// ---------------------------------------------------------------------------

/// The ONE field table of the inode value: the wire struct's fields in
/// order, every one little-endian. The `Put` encoder lays them in this
/// order and the Δinode record's mask bit `i` names field `i` — one source
/// for both (decision 0002 row 3's one-Delta-kind preparation). Times are
/// the i64 ns the charter names (the RAM struct carries their u64 bit
/// patterns: same bytes). `rdev` is the full u64 `dev_t`. `reserved` must
/// read 0 (§12: exactly the encoder's form decodes).
pub const InodeValueWire = extern struct {
    mode_le: [4]u8,
    uid_le: [4]u8,
    gid_le: [4]u8,
    nlink_le: [4]u8,
    flags_le: [4]u8,
    rdev_le: [8]u8,
    size_le: [8]u8,
    atime_le: [8]u8,
    mtime_le: [8]u8,
    ctime_le: [8]u8,
    flags2_le: [4]u8,
    reserved: [4]u8,
};

pub const INODE_VALUE_LEN: usize = @sizeOf(InodeValueWire);

comptime {
    assert(INODE_VALUE_LEN == 68);
    assert(@offsetOf(InodeValueWire, "rdev_le") == 20);
    assert(@offsetOf(InodeValueWire, "ctime_le") == 52);
    assert(@offsetOf(InodeValueWire, "flags2_le") == 60);
    assert(@offsetOf(InodeValueWire, "reserved") == 64);
}

/// One row of the inode field table: the RAM field, its mask bit and its
/// wire width — DERIVED from `InodeValueWire` at comptime (every `*_le`
/// field in declaration order), never written twice.
const InodeFieldSpec = struct { bit: u16, name: []const u8, wire: []const u8, width: usize };

const inode_field_table = blk: {
    const info = @typeInfo(InodeValueWire).@"struct";
    var rows: [info.field_names.len]InodeFieldSpec = undefined;
    var n: usize = 0;
    for (info.field_names, info.field_types) |name, T| {
        if (!std.mem.endsWith(u8, name, "_le")) continue;
        rows[n] = .{ .bit = 1 << n, .name = name[0 .. name.len - "_le".len], .wire = name, .width = @sizeOf(T) };
        n += 1;
    }
    break :blk rows[0..n].*;
};

fn inodeFieldBit(comptime name: []const u8) u16 {
    for (inode_field_table) |f| if (std.mem.eql(u8, f.name, name)) return f.bit;
    unreachable;
}

/// `InodeValue.flags` bit 0: the directory is a STRIPE of another
/// (charter §5.5 "Directory striping"): nameless, minted by the flip or a
/// `SupplyStripeInos`, it never carries a map, feeds no census and is
/// never flipped itself — the record says so wherever it is read (the
/// reference's reverse scan for `is_stripe` needed the parent; decision
/// 0003 §Q).
pub const INODE_FLAG_STRIPE: u32 = 1 << 0;

/// `InodeValue.flags` bit 1: the inode is IMMUTABLE (Linux `FS_IMMUTABLE_FL`,
/// `chattr +i`): no write, no size/mode/owner/time change, no xattr change,
/// no unlink/rename/link of it, no entry made or removed inside it when a
/// directory. Bit 2: APPEND-only (`FS_APPEND_FL`, `chattr +a`): writes
/// only at EOF through `O_APPEND`, no truncate, no mode/owner/explicit-time
/// change, no xattr change, no unlink/rename/link; a directory's entries
/// may be added, never removed. The FUSE kernel never learns either (no
/// `S_IMMUTABLE`/`S_APPEND` on a FUSE inode), so the daemon is the ONE
/// enforcer (`fuse/handler/inode_flags.zig`, `kv_fs`). Both are the
/// record's: durable, remount-stable, served through both ioctl pairs.
pub const INODE_FLAG_IMMUTABLE: u32 = 1 << 1;
pub const INODE_FLAG_APPEND: u32 = 1 << 2;
/// The attribute flags a user sets (the `chattr` bits); `INODE_FLAG_STRIPE`
/// is the format's own and never theirs to touch.
pub const INODE_FLAGS_ATTR: u32 = INODE_FLAG_IMMUTABLE | INODE_FLAG_APPEND;

/// `InodeValue.flags2` bits 0..7: a DIRECTORY's depth below the root (the
/// root's children 1, each level one deeper, saturating at 255 = deeper
/// than any spread depth; `routing/dir_place.zig`, charter §5.4, decision
/// 0003 §BZ). 0 on every file, on a stripe, and on a directory made
/// before the field existed (whose children then record 255). Set at
/// mkdir; informational after birth (a rename keeps it). Bits 8..31 are
/// still reserved (must read 0).
pub const INODE_DEPTH_MASK: u32 = 0xFF;

pub const InodeValue = struct {
    mode: u32 = 0,
    uid: u32 = 0,
    gid: u32 = 0,
    nlink: u32 = 0,
    flags: u32 = 0,
    rdev: u64 = 0,
    size: u64 = 0,
    atime: u64 = 0,
    mtime: u64 = 0,
    ctime: u64 = 0,
    flags2: u32 = 0,

    pub fn isImmutable(self: InodeValue) bool {
        return self.flags & INODE_FLAG_IMMUTABLE != 0;
    }

    pub fn isAppendOnly(self: InodeValue) bool {
        return self.flags & INODE_FLAG_APPEND != 0;
    }

    /// Immutable or append-only: the inode refuses the mutations both
    /// refuse (unlink, rename, link, xattr changes, mode/owner changes).
    pub fn isRestricted(self: InodeValue) bool {
        return self.flags & INODE_FLAGS_ATTR != 0;
    }

    /// The directory depth byte (`INODE_DEPTH_MASK`); 0 = none recorded.
    pub fn dirDepth(self: InodeValue) u8 {
        return @intCast(self.flags2 & INODE_DEPTH_MASK);
    }

    /// The `flags2` word a directory at `depth` is born with.
    pub fn flags2ForDepth(depth: u8) u32 {
        return depth;
    }

    pub fn encode(self: InodeValue) [INODE_VALUE_LEN]u8 {
        var w: InodeValueWire = undefined;
        inline for (inode_field_table) |f| {
            std.mem.writeInt(@FieldType(InodeValue, f.name), &@field(w, f.wire), @field(self, f.name), .little);
        }
        w.reserved = @splat(0);
        return std.mem.toBytes(w);
    }

    /// Exactly `INODE_VALUE_LEN` bytes with a zero reserved tail (truncation,
    /// trailing bytes and a non-zero reserved byte all refuse).
    pub fn decode(bytes: []const u8) DecodeError!InodeValue {
        if (bytes.len != INODE_VALUE_LEN) return error.Corrupt;
        const w = std.mem.bytesToValue(InodeValueWire, bytes[0..INODE_VALUE_LEN]);
        if (!std.mem.allEqual(u8, &w.reserved, 0)) return error.Corrupt;
        var v: InodeValue = .{};
        inline for (inode_field_table) |f| {
            @field(v, f.name) = std.mem.readInt(@FieldType(InodeValue, f.name), &@field(w, f.wire), .little);
        }
        return v;
    }
};

comptime {
    // Every RAM field is a wire field and vice versa: the table is total.
    assert(inode_field_table.len == @typeInfo(InodeValue).@"struct".field_names.len);
    for (inode_field_table) |f| assert(@sizeOf(@FieldType(InodeValue, f.name)) == f.width);
}

// ---------------------------------------------------------------------------
// §2.2 Dentry value: `child_ino u64 LE ‖ file_type u8 ‖ name_len u8 ‖ name`.
// ---------------------------------------------------------------------------

pub const DentryValueHeader = extern struct {
    child_ino_le: [8]u8,
    file_type: u8,
    name_len: u8,
};

comptime {
    assert(@sizeOf(DentryValueHeader) == 10);
}

/// The one length a `name_len u8` can frame.
pub const NAME_MAX: usize = std.math.maxInt(u8);

/// A dentry by its parent's key ino and its name — what a change names to
/// the kernels that may cache it (decision 0003 §AB.2: the tx's notes, the
/// recall's names).
pub const NameRef = struct { parent: u64, name: []const u8 };

pub const DentryValue = struct {
    child_ino: u64,
    /// Directory-entry type byte (`DT_*`).
    file_type: u8,
    /// The full name — collision chains resolve by comparing it.
    name: []const u8,

    pub fn encodedLen(self: DentryValue) usize {
        return @sizeOf(DentryValueHeader) + self.name.len;
    }

    /// Writes into `out` (which must hold `encodedLen()` bytes) and returns
    /// the written prefix. The single-copy form staging sites use.
    pub fn encodeInto(self: DentryValue, out: []u8) EncodeError![]u8 {
        if (self.name.len > NAME_MAX) return error.NameTooLong;
        const n = self.encodedLen();
        assert(out.len >= n);
        var h: DentryValueHeader = undefined;
        std.mem.writeInt(u64, &h.child_ino_le, self.child_ino, .little);
        h.file_type = self.file_type;
        h.name_len = @intCast(self.name.len);
        out[0..@sizeOf(DentryValueHeader)].* = std.mem.toBytes(h);
        @memcpy(out[@sizeOf(DentryValueHeader)..n], self.name);
        return out[0..n];
    }

    pub fn encodeAlloc(self: DentryValue, gpa: Allocator) (EncodeError || Allocator.Error)![]u8 {
        if (self.name.len > NAME_MAX) return error.NameTooLong;
        const out = try gpa.alloc(u8, self.encodedLen());
        return self.encodeInto(out) catch unreachable;
    }

    /// Refuses truncation, a lying `name_len`, and trailing bytes. The name
    /// borrows from `bytes`.
    pub fn decode(bytes: []const u8) DecodeError!DentryValue {
        var r: Reader = .{ .buf = bytes };
        const h = std.mem.bytesToValue(DentryValueHeader, (try r.take(@sizeOf(DentryValueHeader)))[0..@sizeOf(DentryValueHeader)]);
        const name = try r.take(h.name_len);
        try r.finish();
        return .{
            .child_ino = std.mem.readInt(u64, &h.child_ino_le, .little),
            .file_type = h.file_type,
            .name = name,
        };
    }
};

// ---------------------------------------------------------------------------
// §2.3 Xattr value: `name_len u8 ‖ name ‖ value (the remainder)`.
// ---------------------------------------------------------------------------

/// The user-facing record-value ceiling: Linux `XATTR_SIZE_MAX`.
pub const RECORD_VALUE_CAP_CEILING: usize = 65_536;
/// Worst-case xattr envelope over the user value: 1-byte `name_len` + a
/// 255-byte name — so a full 64 KiB value with a maximal name still fits
/// (fstests generic/020).
pub const XATTR_RECORD_ENVELOPE_MAX: usize = 1 + NAME_MAX;

/// The user-facing xattr VALUE cap `min(65 536, node_size / 4)`: keeps the
/// largest record plus headers inside one node at every knob; exactly
/// `XATTR_SIZE_MAX` at the 256 KiB default.
pub fn xattrValueCap(node_size: usize) usize {
    return @min(RECORD_VALUE_CAP_CEILING, node_size / 4);
}

/// The node layer's cap on ONE ENCODED record value: the value cap plus the
/// envelope allowance (`ValueTooLarge` at the frame encoder).
pub fn recordValueCap(node_size: usize) usize {
    return xattrValueCap(node_size) + XATTR_RECORD_ENVELOPE_MAX;
}

pub const XattrValue = struct {
    name: []const u8,
    value: []const u8,

    pub fn encodedLen(self: XattrValue) usize {
        return 1 + self.name.len + self.value.len;
    }

    pub fn encodeInto(self: XattrValue, out: []u8) EncodeError![]u8 {
        if (self.name.len > NAME_MAX) return error.NameTooLong;
        const n = self.encodedLen();
        assert(out.len >= n);
        out[0] = @intCast(self.name.len);
        @memcpy(out[1..][0..self.name.len], self.name);
        @memcpy(out[1 + self.name.len ..][0..self.value.len], self.value);
        return out[0..n];
    }

    pub fn encodeAlloc(self: XattrValue, gpa: Allocator) (EncodeError || Allocator.Error)![]u8 {
        if (self.name.len > NAME_MAX) return error.NameTooLong;
        const out = try gpa.alloc(u8, self.encodedLen());
        return self.encodeInto(out) catch unreachable;
    }

    /// Refuses truncation and a lying `name_len`; the value is the
    /// remainder (the record framing carries the total length), so there is
    /// no trailing-byte check.
    pub fn decode(bytes: []const u8) DecodeError!XattrValue {
        var r: Reader = .{ .buf = bytes };
        const name_len = try r.readU8();
        const name = try r.take(name_len);
        return .{ .name = name, .value = r.remaining() };
    }
};

// ---------------------------------------------------------------------------
// §2.4 Block-ref value — 4 B.
// ---------------------------------------------------------------------------

pub const BLOCK_REF_VALUE_LEN: usize = 4;
/// Forward-only: an unknown version is a newer binary's accounting, and
/// guessing would silently mis-count shared ownership.
pub const BLOCK_REF_VALUE_VERSION: u8 = 1;
/// bit0: this reference is the ino's indirect block-map blob (paired with
/// `BLOCK_INDEX_MAP_BLOB` in the key).
pub const BLOCK_REF_FLAG_MAP_BLOB: u8 = 1 << 0;
/// bit1: the block is SHARED across slot trees through the clone protocol.
pub const BLOCK_REF_FLAG_SHARED: u8 = 1 << 1;
const BLOCK_REF_FLAGS_KNOWN: u8 = BLOCK_REF_FLAG_MAP_BLOB | BLOCK_REF_FLAG_SHARED;

pub const BlockRefValueWire = extern struct {
    version: u8,
    flags: u8,
    reserved_le: [2]u8,
};

comptime {
    assert(@sizeOf(BlockRefValueWire) == BLOCK_REF_VALUE_LEN);
}

pub const BlockRefValue = struct {
    map_blob: bool,
    shared: bool,

    pub fn encode(self: BlockRefValue) [BLOCK_REF_VALUE_LEN]u8 {
        var flags: u8 = 0;
        if (self.map_blob) flags |= BLOCK_REF_FLAG_MAP_BLOB;
        if (self.shared) flags |= BLOCK_REF_FLAG_SHARED;
        return std.mem.toBytes(BlockRefValueWire{ .version = BLOCK_REF_VALUE_VERSION, .flags = flags, .reserved_le = .{ 0, 0 } });
    }

    /// Length-, version-, flag- and reserved-checked: silently accepting an
    /// unreadable accounting record would under-count shared ownership.
    pub fn decode(bytes: []const u8) DecodeError!BlockRefValue {
        if (bytes.len != BLOCK_REF_VALUE_LEN) return error.Corrupt;
        const w = std.mem.bytesToValue(BlockRefValueWire, bytes[0..BLOCK_REF_VALUE_LEN]);
        if (w.version != BLOCK_REF_VALUE_VERSION) return error.Corrupt;
        if (w.flags & ~BLOCK_REF_FLAGS_KNOWN != 0) return error.Corrupt;
        if (std.mem.readInt(u16, &w.reserved_le, .little) != 0) return error.Corrupt;
        return .{
            .map_blob = w.flags & BLOCK_REF_FLAG_MAP_BLOB != 0,
            .shared = w.flags & BLOCK_REF_FLAG_SHARED != 0,
        };
    }
};

// ---------------------------------------------------------------------------
// §2.5 Block-map value: `version u8 ‖ kind u8 ‖ payload`, values LE.
// ---------------------------------------------------------------------------

/// Forward-only: an unknown version means mappings this binary cannot
/// interpret, and guessing would resolve blocks to wrong device bytes.
pub const BLOCK_MAP_VALUE_VERSION: u8 = 1;
pub const BLOCK_MAP_KIND_POINT: u8 = 1;
pub const BLOCK_MAP_KIND_STRING: u8 = 2;
pub const BLOCK_MAP_KIND_RUN: u8 = 3;
pub const BLOCK_MAP_KIND_POINT_STAMPED: u8 = 4;
pub const BLOCK_MAP_KIND_RUN_STAMPED: u8 = 5;
/// version ‖ kind ‖ vol_tag u64 ‖ offset u64.
pub const BLOCK_MAP_POINT_VALUE_LEN: usize = 1 + 1 + 8 + 8;
/// POINT ‖ len u32.
pub const BLOCK_MAP_RUN_VALUE_LEN: usize = BLOCK_MAP_POINT_VALUE_LEN + 4;
/// POINT ‖ incarnation u64.
pub const BLOCK_MAP_POINT_STAMPED_VALUE_LEN: usize = BLOCK_MAP_POINT_VALUE_LEN + 8;
/// RUN ‖ start_incarnation u64.
pub const BLOCK_MAP_RUN_STAMPED_VALUE_LEN: usize = BLOCK_MAP_RUN_VALUE_LEN + 8;
/// The longest span one run may cover (design A6): the exact-miss read law
/// is ONE bounded forward range scan of this width — a format LAW, never a
/// knob; a wider run would be invisible to every reader's floor probe.
pub const RUN_LEN_MAX: u32 = 4096;
/// A 1-run is a point; the encoder emits `2..=RUN_LEN_MAX` only.
pub const RUN_LEN_MIN: u32 = 2;

/// One block-map record as a token carries it (a foreign reader's map
/// window): its index, the indices it resolves, and the encoded VALUE
/// (`MapEntry`), borrowed. The reader's data plane decodes the binding
/// with its own stride.
pub const MapRecordView = struct {
    index: u32,
    len: u32,
    value: []const u8,

    /// Covers `block` (`[index, index + len)`).
    pub fn covers(self: MapRecordView, block: u32) bool {
        return self.index <= block and @as(u64, block) < @as(u64, self.index) + self.len;
    }
};

pub const MapEntry = union(enum) {
    point: struct { vol_tag: u64, offset: u64 },
    /// A decorated block key stored verbatim (`damaged:` markers etc.).
    string: []const u8,
    /// Covers `[start_index, start_index + len)` at one-block stride; the
    /// stride is the volume's block size, resolved by the router, never
    /// stored.
    run: struct { vol_tag: u64, start_offset: u64, len: u32 },
    /// POINT carrying the block key's incarnation stamp VERBATIM.
    point_stamped: struct { vol_tag: u64, offset: u64, incarnation: u64 },
    /// RUN whose covered stamps are consecutive: `stamp(i) = start + i`.
    run_stamped: struct { vol_tag: u64, start_offset: u64, len: u32, start_incarnation: u64 },

    pub fn encodedLen(self: MapEntry) usize {
        return switch (self) {
            .point => BLOCK_MAP_POINT_VALUE_LEN,
            .string => |s| 2 + s.len,
            .run => BLOCK_MAP_RUN_VALUE_LEN,
            .point_stamped => BLOCK_MAP_POINT_STAMPED_VALUE_LEN,
            .run_stamped => BLOCK_MAP_RUN_STAMPED_VALUE_LEN,
        };
    }

    /// Indices this entry resolves: `len` for runs, 1 otherwise.
    pub fn runLen(self: MapEntry) u32 {
        return switch (self) {
            .run => |r| r.len,
            .run_stamped => |r| r.len,
            else => 1,
        };
    }

    pub fn encodeInto(self: MapEntry, out: []u8) []u8 {
        const n = self.encodedLen();
        assert(out.len >= n);
        out[0] = BLOCK_MAP_VALUE_VERSION;
        switch (self) {
            .point => |p| {
                out[1] = BLOCK_MAP_KIND_POINT;
                std.mem.writeInt(u64, out[2..10], p.vol_tag, .little);
                std.mem.writeInt(u64, out[10..18], p.offset, .little);
            },
            .string => |s| {
                out[1] = BLOCK_MAP_KIND_STRING;
                @memcpy(out[2..n], s);
            },
            .run => |r| {
                out[1] = BLOCK_MAP_KIND_RUN;
                std.mem.writeInt(u64, out[2..10], r.vol_tag, .little);
                std.mem.writeInt(u64, out[10..18], r.start_offset, .little);
                std.mem.writeInt(u32, out[18..22], r.len, .little);
            },
            .point_stamped => |p| {
                out[1] = BLOCK_MAP_KIND_POINT_STAMPED;
                std.mem.writeInt(u64, out[2..10], p.vol_tag, .little);
                std.mem.writeInt(u64, out[10..18], p.offset, .little);
                std.mem.writeInt(u64, out[18..26], p.incarnation, .little);
            },
            .run_stamped => |r| {
                out[1] = BLOCK_MAP_KIND_RUN_STAMPED;
                std.mem.writeInt(u64, out[2..10], r.vol_tag, .little);
                std.mem.writeInt(u64, out[10..18], r.start_offset, .little);
                std.mem.writeInt(u32, out[18..22], r.len, .little);
                std.mem.writeInt(u64, out[22..30], r.start_incarnation, .little);
            },
        }
        return out[0..n];
    }

    /// Version-, kind- and length-gated; every refusal is loud corruption.
    pub fn decode(bytes: []const u8) DecodeError!MapEntry {
        if (bytes.len < 2) return error.Corrupt;
        if (bytes[0] != BLOCK_MAP_VALUE_VERSION) return error.Corrupt;
        switch (bytes[1]) {
            BLOCK_MAP_KIND_POINT => {
                if (bytes.len != BLOCK_MAP_POINT_VALUE_LEN) return error.Corrupt;
                return .{ .point = .{
                    .vol_tag = std.mem.readInt(u64, bytes[2..10], .little),
                    .offset = std.mem.readInt(u64, bytes[10..18], .little),
                } };
            },
            BLOCK_MAP_KIND_STRING => return .{ .string = bytes[2..] },
            BLOCK_MAP_KIND_RUN => {
                if (bytes.len != BLOCK_MAP_RUN_VALUE_LEN) return error.Corrupt;
                const len = std.mem.readInt(u32, bytes[18..22], .little);
                try validateRunLen(len);
                return .{ .run = .{
                    .vol_tag = std.mem.readInt(u64, bytes[2..10], .little),
                    .start_offset = std.mem.readInt(u64, bytes[10..18], .little),
                    .len = len,
                } };
            },
            BLOCK_MAP_KIND_POINT_STAMPED => {
                if (bytes.len != BLOCK_MAP_POINT_STAMPED_VALUE_LEN) return error.Corrupt;
                const incarnation = std.mem.readInt(u64, bytes[18..26], .little);
                // Stamp 0 is "unstamped", which rides POINT: a zero here is a
                // second spelling of one mapping and no encoder produces it.
                if (incarnation == 0) return error.Corrupt;
                return .{ .point_stamped = .{
                    .vol_tag = std.mem.readInt(u64, bytes[2..10], .little),
                    .offset = std.mem.readInt(u64, bytes[10..18], .little),
                    .incarnation = incarnation,
                } };
            },
            BLOCK_MAP_KIND_RUN_STAMPED => {
                if (bytes.len != BLOCK_MAP_RUN_STAMPED_VALUE_LEN) return error.Corrupt;
                const len = std.mem.readInt(u32, bytes[18..22], .little);
                try validateRunLen(len);
                const start = std.mem.readInt(u64, bytes[22..30], .little);
                // The emitter verified every ACTUAL minted stamp, so a span
                // whose `start + len − 1` wraps cannot have been emitted.
                if (start == 0) return error.Corrupt;
                _ = std.math.add(u64, start, len - 1) catch return error.Corrupt;
                return .{ .run_stamped = .{
                    .vol_tag = std.mem.readInt(u64, bytes[2..10], .little),
                    .start_offset = std.mem.readInt(u64, bytes[10..18], .little),
                    .len = len,
                    .start_incarnation = start,
                } };
            },
            else => return error.Corrupt,
        }
    }
};

fn validateRunLen(len: u32) DecodeError!void {
    if (len < RUN_LEN_MIN or len > RUN_LEN_MAX) return error.Corrupt;
}

// ---------------------------------------------------------------------------
// §2.6 Inode delta payload — the Δtime record.
// ---------------------------------------------------------------------------

/// The mask bits: bit `i` IS field `i` of `InodeValueWire` (the one table).
pub const DELTA_MODE: u16 = inodeFieldBit("mode");
pub const DELTA_UID: u16 = inodeFieldBit("uid");
pub const DELTA_GID: u16 = inodeFieldBit("gid");
pub const DELTA_NLINK: u16 = inodeFieldBit("nlink");
pub const DELTA_FLAGS: u16 = inodeFieldBit("flags");
pub const DELTA_RDEV: u16 = inodeFieldBit("rdev");
pub const DELTA_SIZE: u16 = inodeFieldBit("size");
pub const DELTA_ATIME: u16 = inodeFieldBit("atime");
pub const DELTA_MTIME: u16 = inodeFieldBit("mtime");
pub const DELTA_CTIME: u16 = inodeFieldBit("ctime");
pub const DELTA_FLAGS2: u16 = inodeFieldBit("flags2");
/// The defined bits (one per wire field); anything else is a decode error.
/// The layout-delta magic 0x4C31 lies outside on purpose.
pub const DELTA_MASK_ALL: u16 = (1 << inode_field_table.len) - 1;
/// The Δtime mask: mtime + ctime, never value fields.
pub const DELTA_TIMES: u16 = DELTA_MTIME | DELTA_CTIME;
/// mask + every field: the inode value's fixed bytes minus its reserved tail.
pub const INODE_DELTA_MAX_LEN: usize = 2 + INODE_VALUE_LEN - @sizeOf(@FieldType(InodeValueWire, "reserved"));

comptime {
    assert(DELTA_MASK_ALL == 0x7FF);
    assert(INODE_DELTA_MAX_LEN == 66);
    assert(LAYOUT_DELTA_MAGIC & ~DELTA_MASK_ALL != 0);
}

/// A fixed-capacity encoded inode delta (≤ 58 B), returned without
/// allocating.
/// The mask an encoded inode Δ names (its first word); null for a payload
/// too short to be one. A peek for the hooks that judge a committed Δ by
/// its fields alone (the lease epochs: an access stamp names no byte).
pub fn inodeDeltaMask(payload: []const u8) ?u16 {
    if (payload.len < 2) return null;
    return std.mem.readInt(u16, payload[0..2], .little);
}

pub const InodeDeltaBuf = struct {
    buf: [INODE_DELTA_MAX_LEN]u8 = undefined,
    len: u8 = 0,

    pub fn slice(self: *const InodeDeltaBuf) []const u8 {
        return self.buf[0..self.len];
    }
};

pub const InodeDelta = struct {
    mask: u16,
    /// Unmasked entries are ignored by `apply` and never encoded.
    fields: InodeValue,

    /// The Δtime record: mtime/ctime only, so concurrent shared-parent-lock
    /// creates never clobber value fields.
    pub fn times(mtime: u64, ctime_: u64) InodeDelta {
        return .{ .mask = DELTA_TIMES, .fields = .{ .mtime = mtime, .ctime = ctime_ } };
    }

    /// The Δctime record (the moved-inode stamp; the pending-times drain's
    /// mtime-untouched shape).
    pub fn ctime(ctime_: u64) InodeDelta {
        return .{ .mask = DELTA_CTIME, .fields = .{ .ctime = ctime_ } };
    }

    pub fn encodedLen(self: InodeDelta) usize {
        var n: usize = 2;
        inline for (field_table) |f| {
            if (self.mask & f.bit != 0) n += f.width;
        }
        return n;
    }

    /// The canonical field order IS the `Put` value's: bit 0 first.
    const field_table = inode_field_table;

    /// `mask u16 LE ‖ the masked fields in canonical order`.
    pub fn encode(self: InodeDelta) InodeDeltaBuf {
        assert(self.mask & ~DELTA_MASK_ALL == 0);
        var out: InodeDeltaBuf = .{};
        std.mem.writeInt(u16, out.buf[0..2], self.mask, .little);
        var pos: usize = 2;
        inline for (field_table) |f| {
            if (self.mask & f.bit != 0) {
                const T = @FieldType(InodeValue, f.name);
                std.mem.writeInt(T, out.buf[pos..][0..f.width], @field(self.fields, f.name), .little);
                pos += f.width;
            }
        }
        out.len = @intCast(pos);
        return out;
    }

    /// Refuses unknown mask bits, truncation and trailing bytes.
    pub fn decode(bytes: []const u8) DecodeError!InodeDelta {
        var r: Reader = .{ .buf = bytes };
        const mask = try r.readU16();
        if (mask & ~DELTA_MASK_ALL != 0) return error.Corrupt;
        var fields: InodeValue = .{};
        inline for (field_table) |f| {
            if (mask & f.bit != 0) {
                const T = @FieldType(InodeValue, f.name);
                @field(fields, f.name) = std.mem.readInt(T, (try r.take(f.width))[0..f.width], .little);
            }
        }
        try r.finish();
        return .{ .mask = mask, .fields = fields };
    }

    /// Field OVERWRITE of the masked fields — idempotent, so replaying a
    /// delta over an already-folded base is a no-op.
    pub fn apply(self: InodeDelta, base: *InodeValue) void {
        inline for (field_table) |f| {
            if (self.mask & f.bit != 0) @field(base, f.name) = @field(self.fields, f.name);
        }
    }
};

// ---------------------------------------------------------------------------
// §2.7 Layout delta payload — the second delta class.
// ---------------------------------------------------------------------------

/// Leading u16 LE of a layout delta ("L1"). Outside `InodeDelta`'s mask
/// space, so the fold branches on the payload alone and an inode-delta
/// decoder refuses it loud instead of misfolding.
pub const LAYOUT_DELTA_MAGIC: u16 = 0x4C31;

const F_BLOCK_MAP_ID: u8 = 1 << 0;
const F_BLOCK_PREFIX: u8 = 1 << 1;
const F_FILE_ID: u8 = 1 << 2;
const F_DATA_KEY: u8 = 1 << 3;
/// The record carries `(base_version, version)` at fixed offsets `[3..19)`
/// — the commit gate's peek law.
const F_VERSIONED: u8 = 1 << 4;
const F_KNOWN: u8 = F_BLOCK_MAP_ID | F_BLOCK_PREFIX | F_FILE_ID | F_DATA_KEY | F_VERSIONED;
/// magic u16 ‖ flags u8 ‖ base_version u64 ‖ version u64.
const LAYOUT_VERSIONS_END: usize = 19;

/// The fold's class discriminator: a magic peek. A payload shorter than
/// the magic is nobody's delta and falls through to the inode decoder's
/// loud rejection.
pub fn isLayoutDelta(payload: []const u8) bool {
    return payload.len >= 2 and std.mem.readInt(u16, payload[0..2], .little) == LAYOUT_DELTA_MAGIC;
}

pub const LayoutVersions = struct { base_version: u64, version: u64 };

/// The `(base_version, version)` peek of a versioned layout delta; null
/// for the unversioned wire or anything not a well-formed versioned prefix
/// (0 is the reserved unversioned value, so a wire carrying it under the
/// flag is malformed — the peek just declines; `decode` refuses it).
pub fn layoutDeltaVersions(payload: []const u8) ?LayoutVersions {
    if (payload.len < LAYOUT_VERSIONS_END or !isLayoutDelta(payload) or payload[2] & F_VERSIONED == 0) return null;
    const v: LayoutVersions = .{
        .base_version = std.mem.readInt(u64, payload[3..11], .little),
        .version = std.mem.readInt(u64, payload[11..19], .little),
    };
    return if (v.version != 0) v else null;
}

/// Re-stamp an ENCODED layout delta's version pair (the owner-side
/// chain-onto-head primitive). `version == 0` answers a copy; a versioned
/// wire is patched in place at `[3..19)`; an unversioned wire is decoded,
/// stamped and re-encoded.
pub fn restampDeltaVersions(gpa: Allocator, payload: []const u8, base_version: u64, version: u64) (DecodeError || Allocator.Error)![]u8 {
    if (version == 0) return gpa.dupe(u8, payload);
    if (payload.len >= LAYOUT_VERSIONS_END and isLayoutDelta(payload) and payload[2] & F_VERSIONED != 0) {
        const out = try gpa.dupe(u8, payload);
        std.mem.writeInt(u64, out[3..11], base_version, .little);
        std.mem.writeInt(u64, out[11..19], version, .little);
        return out;
    }
    var decoded = try LayoutDelta.decode(gpa, payload);
    defer decoded.deinit(gpa);
    decoded.setVersions(base_version, version);
    return decoded.encodeAlloc(gpa);
}

/// One `(block_index → block key)` map entry.
pub const LayoutEntry = struct { block: u32, key: []const u8 };

/// A block-publish layout delta: every layout field EXCEPT the block map
/// ABSOLUTELY ("full minus map"), plus `(block → key)` map INSERTS. Inserts
/// only — removals take the full-`Put` path by design.
pub const LayoutDelta = struct {
    file_type: []const u8 = "",
    size: u64 = 0,
    block_map_id: ?[]const u8 = null,
    block_prefix: ?[]const u8 = null,
    file_id: ?[]const u8 = null,
    data_key: ?[]const u8 = null,
    entries: []const LayoutEntry = &.{},
    /// The DURABLE name of the base this delta was computed against: the
    /// previous link's `version`, or 0 for the first link above a `Put`
    /// (which carries no stamp — unverifiable by construction).
    base_version: u64 = 0,
    /// This link's own durable version; 0 = an UNVERSIONED record (the
    /// pre-item-9 wire; `base_version` is not representable without it).
    version: u64 = 0,

    /// Frees an `entries` array produced by `decode`.
    pub fn deinit(self: *LayoutDelta, gpa: Allocator) void {
        gpa.free(self.entries);
        self.* = .{};
    }

    /// A zero `version` zeroes the pair together so the wire invariant
    /// (`base_version` present ⇔ `version != 0`) holds structurally.
    pub fn setVersions(self: *LayoutDelta, base_version: u64, version: u64) void {
        if (version == 0) {
            self.base_version = 0;
            self.version = 0;
        } else {
            self.base_version = base_version;
            self.version = version;
        }
    }

    fn flags(self: LayoutDelta, keep_versions: bool) u8 {
        var f: u8 = 0;
        if (self.block_map_id != null) f |= F_BLOCK_MAP_ID;
        if (self.block_prefix != null) f |= F_BLOCK_PREFIX;
        if (self.file_id != null) f |= F_FILE_ID;
        if (self.data_key != null) f |= F_DATA_KEY;
        if (keep_versions and self.version != 0) f |= F_VERSIONED;
        return f;
    }

    pub fn encodedLen(self: LayoutDelta, keep_versions: bool) usize {
        var n: usize = 2 + 1;
        if (keep_versions and self.version != 0) n += 16;
        n += 1 + self.file_type.len + 8;
        inline for (.{ self.block_map_id, self.block_prefix, self.file_id, self.data_key }) |opt| {
            if (opt) |b| n += 2 + b.len;
        }
        n += 4;
        for (self.entries) |e| n += 4 + 2 + e.key.len;
        return n;
    }

    /// `magic u16 ‖ flags u8 ‖ [base_version u64 ‖ version u64 iff bit 4] ‖
    /// file_type_len u8 ‖ file_type ‖ size u64 ‖ [block_map_id u16+bytes] ‖
    /// [block_prefix] ‖ [file_id] ‖ [data_key] ‖ entry_count u32 ‖ entries ×
    /// {block u32 ‖ key_len u16 ‖ key}`, all LE. The version pair encodes iff
    /// `version != 0` and `keep_versions` (the un-stamped volume's strip seam:
    /// its wire is byte-identical to the pre-item-9 one).
    pub fn encodeInto(self: LayoutDelta, out: []u8, keep_versions: bool) []u8 {
        assert(self.file_type.len <= std.math.maxInt(u8));
        const n = self.encodedLen(keep_versions);
        assert(out.len >= n);
        var pos: usize = 0;
        std.mem.writeInt(u16, out[0..2], LAYOUT_DELTA_MAGIC, .little);
        out[2] = self.flags(keep_versions);
        pos = 3;
        if (keep_versions and self.version != 0) {
            std.mem.writeInt(u64, out[pos..][0..8], self.base_version, .little);
            std.mem.writeInt(u64, out[pos + 8 ..][0..8], self.version, .little);
            pos += 16;
        }
        out[pos] = @intCast(self.file_type.len);
        pos += 1;
        @memcpy(out[pos..][0..self.file_type.len], self.file_type);
        pos += self.file_type.len;
        std.mem.writeInt(u64, out[pos..][0..8], self.size, .little);
        pos += 8;
        inline for (.{ self.block_map_id, self.block_prefix, self.file_id, self.data_key }) |opt| {
            if (opt) |b| {
                assert(b.len <= std.math.maxInt(u16));
                std.mem.writeInt(u16, out[pos..][0..2], @intCast(b.len), .little);
                @memcpy(out[pos + 2 ..][0..b.len], b);
                pos += 2 + b.len;
            }
        }
        std.mem.writeInt(u32, out[pos..][0..4], @intCast(self.entries.len), .little);
        pos += 4;
        for (self.entries) |e| {
            assert(e.key.len <= std.math.maxInt(u16));
            std.mem.writeInt(u32, out[pos..][0..4], e.block, .little);
            std.mem.writeInt(u16, out[pos + 4 ..][0..2], @intCast(e.key.len), .little);
            @memcpy(out[pos + 6 ..][0..e.key.len], e.key);
            pos += 6 + e.key.len;
        }
        assert(pos == n);
        return out[0..n];
    }

    pub fn encodeAlloc(self: LayoutDelta, gpa: Allocator) Allocator.Error![]u8 {
        const out = try gpa.alloc(u8, self.encodedLen(true));
        return self.encodeInto(out, true);
    }

    /// The version pair STRIPPED — what a volume without `KV_LAYOUT_VERSIONS`
    /// stores.
    pub fn encodeUnversionedAlloc(self: LayoutDelta, gpa: Allocator) Allocator.Error![]u8 {
        const out = try gpa.alloc(u8, self.encodedLen(false));
        return self.encodeInto(out, false);
    }

    /// Refuses a wrong magic, unknown flags, a versioned flag carrying
    /// version 0, truncation, lying lengths, non-UTF-8 strings and trailing
    /// bytes. Strings borrow from `bytes`; `entries` is allocated with a
    /// capacity bounded by the container (never by the count claim: the
    /// smallest entry is 6 bytes).
    pub fn decode(gpa: Allocator, bytes: []const u8) (DecodeError || Allocator.Error)!LayoutDelta {
        var r: Reader = .{ .buf = bytes };
        if ((try r.readU16()) != LAYOUT_DELTA_MAGIC) return error.Corrupt;
        const f = try r.readU8();
        if (f & ~F_KNOWN != 0) return error.Corrupt;
        var d: LayoutDelta = .{};
        if (f & F_VERSIONED != 0) {
            d.base_version = try r.readU64();
            d.version = try r.readU64();
            if (d.version == 0) return error.Corrupt;
        }
        const ft_len = try r.readU8();
        d.file_type = try takeUtf8(&r, ft_len);
        d.size = try r.readU64();
        if (f & F_BLOCK_MAP_ID != 0) d.block_map_id = try takeUtf8(&r, try r.readU16());
        if (f & F_BLOCK_PREFIX != 0) d.block_prefix = try takeUtf8(&r, try r.readU16());
        if (f & F_FILE_ID != 0) d.file_id = try takeUtf8(&r, try r.readU16());
        if (f & F_DATA_KEY != 0) d.data_key = try r.take(try r.readU16());
        const n = try r.readU32();
        // Allocation bound: a claim beyond what the remaining bytes can hold
        // is refused before any allocation.
        const min_entry_len = 4 + 2;
        if (n > r.remaining().len / min_entry_len) return error.Corrupt;
        var entries = try std.ArrayList(LayoutEntry).initCapacity(gpa, n);
        errdefer entries.deinit(gpa);
        for (0..n) |_| {
            const block = try r.readU32();
            const klen = try r.readU16();
            entries.appendAssumeCapacity(.{ .block = block, .key = try takeUtf8(&r, klen) });
        }
        try r.finish();
        d.entries = try entries.toOwnedSlice(gpa);
        return d;
    }

    /// The in-place fold: map inserts (creating the map iff there are
    /// entries) then OVERWRITE of every non-map field.
    pub fn applyTo(self: LayoutDelta, gpa: Allocator, layout: *Layout) Allocator.Error!void {
        if (self.entries.len != 0) {
            if (layout.block_map == null) layout.block_map = .empty;
            for (self.entries) |e| try layout.insert(gpa, e.block, e.key);
        }
        layout.file_type = self.file_type;
        layout.size = self.size;
        layout.block_map_id = self.block_map_id;
        layout.block_prefix = self.block_prefix;
        layout.file_id = self.file_id;
        layout.data_key = self.data_key;
    }
};

fn takeUtf8(r: *Reader, n: usize) DecodeError![]const u8 {
    const s = try r.take(n);
    if (!std.unicode.utf8ValidateSlice(s)) return error.Corrupt;
    return s;
}

// ---------------------------------------------------------------------------
// The layout BASE: a bincode 1.3 `LayoutMetadata` (fixint, LE, u64 lengths,
// Option tag u8) with the block map in ASCENDING block-index order on
// encode. Decode accepts what bincode's `deserialize` accepts (any map
// order, later duplicate wins, TRAILING BYTES ALLOWED — bincode 1.3's
// `deserialize` is `allow_trailing_bytes`), so a fold that succeeds on the
// reference succeeds here; the re-encode is canonical either way.
// ---------------------------------------------------------------------------

/// How a layout base refuses a delta (writer rules made structural — a
/// delta staged onto any of these is corruption). Typed, never
/// string-parsed: the owner's compose arms branch on INDIRECT vs KVMAP.
pub const LayoutBaseError = error{
    /// First byte `{`: a legacy JSON layout.
    LayoutBaseLegacyJson,
    /// Not a bincode layout.
    LayoutBaseUndecodable,
    /// `block_map_id` starts `indirect:` — its map lives in a data-plane
    /// blob; inline entries would shadow it.
    LayoutBaseIndirect,
    /// `block_map_id` starts `kvmap:` — its map lives in tree-7 records.
    LayoutBaseKvmap,
};

const BINCODE_SOME: u8 = 1;
const BINCODE_NONE: u8 = 0;
/// u32 block ‖ u64 key length: the smallest encodable map entry.
const BINCODE_MIN_MAP_ENTRY_LEN: usize = 4 + 8;

pub const Layout = struct {
    file_type: []const u8 = "",
    size: u64 = 0,
    block_map_id: ?[]const u8 = null,
    block_prefix: ?[]const u8 = null,
    file_id: ?[]const u8 = null,
    data_key: ?[]const u8 = null,
    /// Sorted ascending by block, unique.
    block_map: ?std.ArrayList(LayoutEntry) = null,

    pub fn deinit(self: *Layout, gpa: Allocator) void {
        if (self.block_map) |*m| m.deinit(gpa);
        self.* = .{};
    }

    /// Insert-or-replace keeping ascending order (the canonical encoder's
    /// order). Appends in O(1) when blocks arrive ascending.
    pub fn insert(self: *Layout, gpa: Allocator, block: u32, k: []const u8) Allocator.Error!void {
        const m = &self.block_map.?;
        const idx = std.sort.lowerBound(LayoutEntry, m.items, block, entryBlockOrder);
        if (idx < m.items.len and m.items[idx].block == block) {
            m.items[idx].key = k;
        } else {
            try m.insert(gpa, idx, .{ .block = block, .key = k });
        }
    }

    fn entryBlockOrder(block: u32, e: LayoutEntry) std.math.Order {
        return std.math.order(block, e.block);
    }

    pub fn get(self: *const Layout, block: u32) ?[]const u8 {
        const m = &(self.block_map orelse return null);
        const idx = std.sort.lowerBound(LayoutEntry, m.items, block, entryBlockOrder);
        if (idx < m.items.len and m.items[idx].block == block) return m.items[idx].key;
        return null;
    }

    /// bincode decode WITHOUT the indirect/kvmap refusals (legacy JSON and
    /// undecodable bases still refuse).
    pub fn decodeAny(gpa: Allocator, base: []const u8) (LayoutBaseError || Allocator.Error)!Layout {
        if (base.len > 0 and base[0] == '{') return error.LayoutBaseLegacyJson;
        return decodeBincode(gpa, base) catch |e| switch (e) {
            error.Corrupt => error.LayoutBaseUndecodable,
            error.OutOfMemory => error.OutOfMemory,
        };
    }

    /// The fold's base decoder with the full refusal ladder.
    pub fn decodeBase(gpa: Allocator, base: []const u8) (LayoutBaseError || Allocator.Error)!Layout {
        var l = try decodeAny(gpa, base);
        errdefer l.deinit(gpa);
        if (l.block_map_id) |id| {
            if (std.mem.startsWith(u8, id, "indirect:")) return error.LayoutBaseIndirect;
            if (std.mem.startsWith(u8, id, "kvmap:")) return error.LayoutBaseKvmap;
        }
        return l;
    }

    fn bincodeBytes(r: *Reader) DecodeError![]const u8 {
        const len = try r.readU64();
        if (len > r.remaining().len) return error.Corrupt;
        return r.take(@intCast(len));
    }

    fn bincodeString(r: *Reader) DecodeError![]const u8 {
        const s = try bincodeBytes(r);
        if (!std.unicode.utf8ValidateSlice(s)) return error.Corrupt;
        return s;
    }

    fn bincodeOptTag(r: *Reader) DecodeError!bool {
        return switch (try r.readU8()) {
            BINCODE_NONE => false,
            BINCODE_SOME => true,
            else => error.Corrupt,
        };
    }

    fn decodeBincode(gpa: Allocator, base: []const u8) (DecodeError || Allocator.Error)!Layout {
        var r: Reader = .{ .buf = base };
        var l: Layout = .{};
        errdefer l.deinit(gpa);
        l.file_type = try bincodeString(&r);
        l.size = try r.readU64();
        if (try bincodeOptTag(&r)) l.block_map_id = try bincodeString(&r);
        if (try bincodeOptTag(&r)) l.block_prefix = try bincodeString(&r);
        if (try bincodeOptTag(&r)) l.file_id = try bincodeString(&r);
        if (try bincodeOptTag(&r)) l.data_key = try bincodeBytes(&r);
        if (try bincodeOptTag(&r)) {
            const n = try r.readU64();
            if (n > r.remaining().len / BINCODE_MIN_MAP_ENTRY_LEN) return error.Corrupt;
            l.block_map = try std.ArrayList(LayoutEntry).initCapacity(gpa, @intCast(n));
            for (0..@intCast(n)) |_| {
                const block = try r.readU32();
                const k = try bincodeString(&r);
                try l.insert(gpa, block, k);
            }
        }
        return l;
    }

    pub fn encodedLen(self: *const Layout) usize {
        var n: usize = 8 + self.file_type.len + 8;
        inline for (.{ self.block_map_id, self.block_prefix, self.file_id, self.data_key }) |opt| {
            n += 1;
            if (opt) |b| n += 8 + b.len;
        }
        n += 1;
        if (self.block_map) |m| {
            n += 8;
            for (m.items) |e| n += 4 + 8 + e.key.len;
        }
        return n;
    }

    /// The canonical encoding (bincode fixint LE; map ascending).
    pub fn encodeInto(self: *const Layout, out: []u8) []u8 {
        const n = self.encodedLen();
        assert(out.len >= n);
        var pos: usize = 0;
        pos += putBincodeBytes(out[pos..], self.file_type);
        std.mem.writeInt(u64, out[pos..][0..8], self.size, .little);
        pos += 8;
        inline for (.{ self.block_map_id, self.block_prefix, self.file_id, self.data_key }) |opt| {
            if (opt) |b| {
                out[pos] = BINCODE_SOME;
                pos += 1;
                pos += putBincodeBytes(out[pos..], b);
            } else {
                out[pos] = BINCODE_NONE;
                pos += 1;
            }
        }
        if (self.block_map) |m| {
            out[pos] = BINCODE_SOME;
            pos += 1;
            std.mem.writeInt(u64, out[pos..][0..8], m.items.len, .little);
            pos += 8;
            for (m.items) |e| {
                std.mem.writeInt(u32, out[pos..][0..4], e.block, .little);
                pos += 4;
                pos += putBincodeBytes(out[pos..], e.key);
            }
        } else {
            out[pos] = BINCODE_NONE;
            pos += 1;
        }
        assert(pos == n);
        return out[0..n];
    }

    pub fn encodeAlloc(self: *const Layout, gpa: Allocator) Allocator.Error![]u8 {
        const out = try gpa.alloc(u8, self.encodedLen());
        return self.encodeInto(out);
    }

    fn putBincodeBytes(out: []u8, b: []const u8) usize {
        std.mem.writeInt(u64, out[0..8], b.len, .little);
        @memcpy(out[8..][0..b.len], b);
        return 8 + b.len;
    }
};

// ---------------------------------------------------------------------------
// §3.1 Record kinds & framing.
// ---------------------------------------------------------------------------

/// Encodings start at 1 so a zeroed byte is never a valid kind.
pub const RecordKind = enum(u8) {
    /// Full value.
    put = 1,
    /// Partial update folding into the newest base `Put`.
    delta = 2,
    /// Per-key tombstone.
    delete = 3,

    fn fromByte(b: u8) DecodeError!RecordKind {
        return switch (b) {
            1 => .put,
            2 => .delta,
            3 => .delete,
            else => error.Corrupt,
        };
    }
};

/// `key_len u16 ‖ kind u8 ‖ flags u8 ‖ val_len u32 ‖ seq u64`, LE — 16 B,
/// naturally aligned (format v4, decision 0003 Q1). `flags` is the one
/// reserved must-be-zero byte a forward-only format keeps as cheap
/// insurance; a non-zero value refuses (§12: exactly the encoder's form
/// decodes).
pub const RecordHeader = extern struct {
    key_len_le: [2]u8,
    kind: u8,
    flags: u8,
    val_len_le: [4]u8,
    seq_le: [8]u8,
};

pub const RECORD_HEADER_LEN: usize = 16;

comptime {
    assert(@sizeOf(RecordHeader) == RECORD_HEADER_LEN);
    assert(@offsetOf(RecordHeader, "val_len_le") == 4);
    assert(@offsetOf(RecordHeader, "seq_le") == 8);
}

/// The smallest encodable record: a header and a one-byte key (keys are
/// never empty). The bset parser's allocation bound.
pub const RECORD_MIN_LEN: usize = RECORD_HEADER_LEN + 1;

/// A borrowed, zero-copy record view — what the fold and bset layers
/// consume.
pub const RecordRef = struct {
    key: []const u8,
    seq: u64,
    kind: RecordKind,
    /// Empty for `Delete`; the delta payload for `Delta`.
    value: []const u8,

    pub fn encodedLen(self: RecordRef) usize {
        return RECORD_HEADER_LEN + self.key.len + self.value.len;
    }

    pub fn encodeInto(self: RecordRef, out: []u8) []u8 {
        assert(self.key.len != 0 and self.key.len <= std.math.maxInt(u16));
        assert(self.value.len <= std.math.maxInt(u32));
        assert(self.kind != .delete or self.value.len == 0);
        const n = self.encodedLen();
        assert(out.len >= n);
        var h: RecordHeader = undefined;
        std.mem.writeInt(u16, &h.key_len_le, @intCast(self.key.len), .little);
        h.kind = @backingInt(self.kind);
        h.flags = 0;
        std.mem.writeInt(u32, &h.val_len_le, @intCast(self.value.len), .little);
        std.mem.writeInt(u64, &h.seq_le, self.seq, .little);
        out[0..RECORD_HEADER_LEN].* = std.mem.toBytes(h);
        @memcpy(out[RECORD_HEADER_LEN..][0..self.key.len], self.key);
        @memcpy(out[RECORD_HEADER_LEN + self.key.len ..][0..self.value.len], self.value);
        return out[0..n];
    }

    pub fn appendTo(self: RecordRef, gpa: Allocator, out: *std.ArrayList(u8)) Allocator.Error!void {
        const dst = try out.addManyAsSlice(gpa, self.encodedLen());
        _ = self.encodeInto(dst);
    }

    pub const Decoded = struct { rec: RecordRef, used: usize };

    /// Decode one record from the front of `buf`. Every length is checked
    /// against the container before use; tombstones carry no value; unknown
    /// kinds, a non-zero flags byte and empty keys refuse.
    pub fn decode(buf: []const u8) DecodeError!Decoded {
        if (buf.len < RECORD_HEADER_LEN) return error.Corrupt;
        const h = std.mem.bytesToValue(RecordHeader, buf[0..RECORD_HEADER_LEN]);
        const key_len: usize = std.mem.readInt(u16, &h.key_len_le, .little);
        const kind = try RecordKind.fromByte(h.kind);
        if (h.flags != 0) return error.Corrupt;
        const seq = std.mem.readInt(u64, &h.seq_le, .little);
        const val_len: usize = std.mem.readInt(u32, &h.val_len_le, .little);
        if (key_len == 0) return error.Corrupt;
        if (kind == .delete and val_len != 0) return error.Corrupt;
        const total_u64: u64 = @as(u64, RECORD_HEADER_LEN) + key_len + val_len;
        if (total_u64 > buf.len) return error.Corrupt;
        const total: usize = @intCast(total_u64);
        return .{
            .rec = .{
                .key = buf[RECORD_HEADER_LEN..][0..key_len],
                .seq = seq,
                .kind = kind,
                .value = buf[RECORD_HEADER_LEN + key_len .. total],
            },
            .used = total,
        };
    }

    pub fn toRecord(self: RecordRef, gpa: Allocator) Allocator.Error!Record {
        const k = try gpa.dupe(u8, self.key);
        errdefer gpa.free(k);
        const v = try gpa.dupe(u8, self.value);
        return .{ .key = k, .seq = self.seq, .kind = self.kind, .value = v };
    }

    pub fn eql(a: RecordRef, b: RecordRef) bool {
        return a.seq == b.seq and a.kind == b.kind and std.mem.eql(u8, a.key, b.key) and std.mem.eql(u8, a.value, b.value);
    }
};

/// An owned record — the staging/build-side twin of `RecordRef`.
pub const Record = struct {
    key: []u8,
    seq: u64,
    kind: RecordKind,
    value: []u8,

    pub fn put(gpa: Allocator, k: []const u8, seq: u64, value: []const u8) Allocator.Error!Record {
        return (RecordRef{ .key = k, .seq = seq, .kind = .put, .value = value }).toRecord(gpa);
    }

    pub fn delta(gpa: Allocator, k: []const u8, seq: u64, d: InodeDelta) Allocator.Error!Record {
        const enc = d.encode();
        return (RecordRef{ .key = k, .seq = seq, .kind = .delta, .value = enc.slice() }).toRecord(gpa);
    }

    pub fn delete(gpa: Allocator, k: []const u8, seq: u64) Allocator.Error!Record {
        return (RecordRef{ .key = k, .seq = seq, .kind = .delete, .value = &.{} }).toRecord(gpa);
    }

    pub fn deinit(self: *Record, gpa: Allocator) void {
        gpa.free(self.key);
        gpa.free(self.value);
        self.* = undefined;
    }

    pub fn ref(self: *const Record) RecordRef {
        return .{ .key = self.key, .seq = self.seq, .kind = self.kind, .value = self.value };
    }
};

pub fn deinitRecords(gpa: Allocator, records: []Record) void {
    for (records) |*r| r.deinit(gpa);
}

// ---------------------------------------------------------------------------
// §3.2–§3.5 THE fold algebra — shared byte-identically by point lookup,
// the bset n-way merge, compaction and journal replay. "Replay reproduces
// RAM" is a single theorem about `foldNewestFirst`.
// ---------------------------------------------------------------------------

/// Everything a fold can refuse. Structural decode failures are `Corrupt`;
/// the layout chain's laws and base refusals are their own classes.
pub const FoldError = DecodeError || LayoutBaseError || Allocator.Error || error{
    /// A chain mixing inode and layout deltas on one key — representable
    /// only by corruption (inode and xattr keys never alias).
    MixedDeltaClasses,
    /// A versioned link above unversioned ones or vice versa: within one
    /// base segment links are HOMOGENEOUS (the commit gate re-bases before
    /// a versioned link can join a pre-stamp chain).
    LayoutChainHeterogeneous,
    /// A link's `base_version` is not the previous link's `version`: a
    /// fork two writers staged against one base. Folding it would produce
    /// a layout NEITHER computed ("divergent chains fold to divergent
    /// layouts"), so it refuses loud.
    LayoutChainDivergent,
};

/// Outcome of folding one key's records.
pub const Folded = union(enum) {
    /// A live value. `seq` is the newest CONTRIBUTING record's seq (a
    /// delta-folded Put carries the newest delta's seq, so the folded Put
    /// shadows everything it folded away). `owned` is false when no delta
    /// had to be applied: the value borrows the record bytes (zero-copy).
    put: struct { value: []const u8, seq: u64, owned: bool },
    /// Absent because of a tombstone with this seq — compaction needs it
    /// for the elision rule.
    tombstone: struct { seq: u64 },
    /// No records, or only orphaned deltas (counted no-op).
    absent,

    /// The user-visible projection: live bytes, or null for tombstones and
    /// absent keys alike.
    pub fn liveValue(self: Folded) ?[]const u8 {
        return switch (self) {
            .put => |p| p.value,
            .tombstone, .absent => null,
        };
    }

    pub fn deinit(self: *Folded, gpa: Allocator) void {
        switch (self.*) {
            .put => |p| if (p.owned) gpa.free(p.value),
            else => {},
        }
        self.* = .absent;
    }
};

/// Fold one key's records presented NEWEST-seq-first. Exact `seq` ties
/// across sources keep input order and are legal only for identical-effect
/// records (a node bset overlapping the journal replay window).
///
/// Scan collecting `Delta`s until the first `Put` or `Delete`: `Delete` ⇒
/// tombstone (collected deltas DISCARDED, not counted — the tombstone is
/// the underlying truth); `Put` ⇒ apply the deltas ascending onto it;
/// exhausted with deltas pending ⇒ absent, one orphan count per record.
/// Values stay opaque unless a delta forces a base decode.
pub fn foldNewestFirst(gpa: Allocator, records: []const RecordRef) FoldError!Folded {
    var prev_seq: u64 = std.math.maxInt(u64);
    for (records, 0..) |r, i| {
        // Input contract: newest-seq-first.
        assert(prev_seq >= r.seq);
        prev_seq = r.seq;
        switch (r.kind) {
            .delta => {},
            .delete => return .{ .tombstone = .{ .seq = r.seq } },
            .put => {
                // Every record before `i` was a delta: the chain is the
                // prefix, newest-first.
                const deltas = records[0..i];
                if (deltas.len == 0) return .{ .put = .{ .value = r.value, .seq = r.seq, .owned = false } };
                return .{ .put = .{
                    .value = try foldDeltasOntoPut(gpa, r.value, deltas),
                    .seq = deltas[0].seq,
                    .owned = true,
                } };
            },
        }
    }
    if (records.len != 0) stats.meta_kv_delta_orphans.add(records.len);
    return .absent;
}

/// Apply a collected delta chain (NEWEST-first) onto its base `Put` value —
/// the algebra's one value-aware step, branched by delta CLASS on the
/// newest delta's payload. Returns the folded value, owned by the caller.
pub fn foldDeltasOntoPut(gpa: Allocator, base_value: []const u8, deltas: []const RecordRef) FoldError![]u8 {
    assert(deltas.len != 0);
    // One count per record decoded: the base plus every delta.
    stats.meta_kv_fold_record_decodes.add(1 + deltas.len);
    if (isLayoutDelta(deltas[0].value)) {
        const x = try XattrValue.decode(base_value);
        var layout = try Layout.decodeBase(gpa, x.value);
        defer layout.deinit(gpa);
        var prev_version: ?u64 = null;
        var saw_unversioned = false;
        var last_seq: ?u64 = null;
        // Ascending seq: oldest first, newest applied last wins.
        var i = deltas.len;
        while (i > 0) {
            i -= 1;
            const d = deltas[i];
            if (!isLayoutDelta(d.value)) return error.MixedDeltaClasses;
            var dec = try LayoutDelta.decode(gpa, d.value);
            defer dec.deinit(gpa);
            // Exact-seq duplicates (the same record presented from two
            // sources) re-apply idempotently and skip the link law.
            const duplicate = last_seq == d.seq;
            last_seq = d.seq;
            if (!duplicate) {
                if (dec.version != 0) {
                    if (saw_unversioned) return error.LayoutChainHeterogeneous;
                    // The first versioned link claims base 0 (the bare Put
                    // carries no stamp); every later one claims the
                    // previous link's version.
                    const want = prev_version orelse 0;
                    if (dec.base_version != want) return error.LayoutChainDivergent;
                    prev_version = dec.version;
                } else {
                    if (prev_version != null) return error.LayoutChainHeterogeneous;
                    saw_unversioned = true;
                }
            }
            try dec.applyTo(gpa, &layout);
        }
        stats.meta_kv_layout_delta_folds.add(deltas.len);
        return encodeLayoutXattr(gpa, x.name, &layout);
    }
    var base = try InodeValue.decode(base_value);
    var i = deltas.len;
    while (i > 0) {
        i -= 1;
        if (isLayoutDelta(deltas[i].value)) return error.MixedDeltaClasses;
        (try InodeDelta.decode(deltas[i].value)).apply(&base);
    }
    return gpa.dupe(u8, &base.encode());
}

/// `XattrValue{name, canonical layout}` encoded in one allocation.
fn encodeLayoutXattr(gpa: Allocator, name: []const u8, layout: *const Layout) Allocator.Error![]u8 {
    // The name came off a u8 `name_len`, so the envelope always frames it.
    assert(name.len <= NAME_MAX);
    const out = try gpa.alloc(u8, 1 + name.len + layout.encodedLen());
    out[0] = @intCast(name.len);
    @memcpy(out[1..][0..name.len], name);
    _ = layout.encodeInto(out[1 + name.len ..]);
    return out;
}

/// Compaction fold: the same algebra; at most one folded `Put` — plus, for
/// a versioned layout chain, the retained head link — per key, appended to
/// `out` `(key, seq)`-ascending and directly buildable into a bset.
///
/// Tombstone elision: a `Delete` may be dropped only if `seq < durable_tail`
/// (`seq == tail` KEEPS — still inside the replay window, where replay
/// could otherwise resurrect the key from an older journal `Put`). The
/// comparison crosses record-seq vs ring-position domains on purpose and is
/// conservative under the seq-space law (`seq = position + offset ≥
/// position`): a nonzero offset only delays an elision.
///
/// Lineage rule (the width-N fork latch): when the NEWEST record is a
/// VERSIONED layout link, folding the whole chain into a bare `Put` would
/// erase the head version a LATER link may already claim (the commit gate
/// stamps claims against the durable head, and nothing orders that stamp
/// against this task). So fold everything BELOW the link and RETAIN the
/// link, claim restamped to 0 (it is now its segment's first link). The
/// folded value is byte-equal either way. A prefix folding to a tombstone
/// or to nothing keeps the plain algebra.
pub fn compactFold(gpa: Allocator, group_newest_first: []const RecordRef, durable_tail: u64, out: *std.ArrayList(Record)) FoldError!void {
    if (group_newest_first.len == 0) return;
    const first = group_newest_first[0];
    const k = first.key;
    for (group_newest_first) |r| assert(std.mem.eql(u8, r.key, k));
    if (first.kind == .delta and isLayoutDelta(first.value)) {
        if (layoutDeltaVersions(first.value)) |v| {
            var below = try foldNewestFirst(gpa, group_newest_first[1..]);
            defer below.deinit(gpa);
            if (below == .put) {
                const p = below.put;
                const restamped = try restampDeltaVersions(gpa, first.value, 0, v.version);
                errdefer gpa.free(restamped);
                try out.ensureUnusedCapacity(gpa, 2);
                var folded_put = try Record.put(gpa, k, p.seq, p.value);
                errdefer folded_put.deinit(gpa);
                const link_key = try gpa.dupe(u8, k);
                out.appendAssumeCapacity(folded_put);
                out.appendAssumeCapacity(.{ .key = link_key, .seq = first.seq, .kind = .delta, .value = restamped });
                return;
            }
        }
    }
    var folded = try foldNewestFirst(gpa, group_newest_first);
    defer folded.deinit(gpa);
    switch (folded) {
        .put => |p| try out.append(gpa, try Record.put(gpa, k, p.seq, p.value)),
        .tombstone => |t| if (t.seq >= durable_tail) try out.append(gpa, try Record.delete(gpa, k, t.seq)),
        .absent => {},
    }
}

/// The owned, materialized outcome of folding one key — the overlay head
/// riding the newest record and the writer-side carry between applies.
pub const FoldedHead = union(enum) {
    /// `decoded` carries the already-decoded INODE value across inode-delta
    /// applies (layout heads re-decode from `value`; their class is
    /// self-describing). `materialized` is true exactly when `value` is a
    /// fresh owned buffer produced by delta application (a plain-`Put` head
    /// borrows the record's bytes).
    live: struct { value: []const u8, decoded: ?InodeValue, materialized: bool },
    tombstone,
    absent,

    /// Heap bytes this head OWNS beyond its footprint: the §5.7 budget
    /// charges exactly this plus a fixed per-head overhead.
    pub fn ownedBytes(self: FoldedHead) usize {
        return switch (self) {
            .live => |l| if (l.materialized) l.value.len else 0,
            else => 0,
        };
    }

    pub fn deinit(self: *FoldedHead, gpa: Allocator) void {
        switch (self.*) {
            .live => |l| if (l.materialized) gpa.free(l.value),
            else => {},
        }
        self.* = .absent;
    }
};

/// Fold ONE newer record onto the folded outcome of everything older — the
/// algebra run incrementally. Theorem (pinned by property test): `prev ==
/// fold(history) ∧ rec.seq > every seq in history ⇒ foldForward(prev, rec)
/// == fold(rec ∪ history)`. Deliberately version-BLIND (no chain-link law:
/// a materialized head carries no memory of the last link's version); the
/// commit gate keeps divergent links off the durable chain and every
/// from-scratch fold runs the strict law.
pub fn foldForward(gpa: Allocator, prev: *const FoldedHead, kind: RecordKind, value: []const u8) FoldError!FoldedHead {
    switch (kind) {
        .put => return .{ .live = .{ .value = value, .decoded = null, .materialized = false } },
        .delete => return .tombstone,
        .delta => switch (prev.*) {
            .live => |l| {
                if (isLayoutDelta(value)) {
                    stats.meta_kv_fold_record_decodes.add(2);
                    const x = try XattrValue.decode(l.value);
                    var layout = try Layout.decodeBase(gpa, x.value);
                    defer layout.deinit(gpa);
                    var dec = try LayoutDelta.decode(gpa, value);
                    defer dec.deinit(gpa);
                    try dec.applyTo(gpa, &layout);
                    stats.meta_kv_layout_delta_folds.add(1);
                    return .{ .live = .{ .value = try encodeLayoutXattr(gpa, x.name, &layout), .decoded = null, .materialized = true } };
                }
                var base = l.decoded orelse blk: {
                    // One base decode; the decoded value then rides the head
                    // for every later delta.
                    stats.meta_kv_fold_record_decodes.add(1);
                    break :blk try InodeValue.decode(l.value);
                };
                stats.meta_kv_fold_record_decodes.add(1);
                (try InodeDelta.decode(value)).apply(&base);
                return .{ .live = .{ .value = try gpa.dupe(u8, &base.encode()), .decoded = base, .materialized = true } };
            },
            .tombstone => return .tombstone,
            .absent => {
                // Δ-without-base, counted at materialization instead of at
                // every read.
                stats.meta_kv_delta_orphans.inc();
                return .absent;
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Tests — these pin the on-disk value forms and the algebra.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// Deterministic, field-distinct inode value for a seed (the reference's
/// `iv`).
fn iv(seed: u64) InodeValue {
    const s32: u32 = @truncate(seed);
    return .{
        .mode = 0o100644 ^ s32,
        .uid = 1000 +% s32,
        .gid = 2000 +% @as(u32, @truncate(seed >> 8)),
        .nlink = 1 + (s32 & 3),
        .flags = std.math.rotl(u32, s32, 7),
        .rdev = 0,
        .size = seed *% 4096,
        .atime = seed +% 1,
        .mtime = seed +% 2,
        .ctime = seed +% 3,
    };
}

/// Fold-test key: one inode-tree key shared by a record group.
const fold_key = key.inodeKey(77);

/// Test-only owned-record builders over gpa; freed with `deinitRecords`.
fn tPut(seq: u64, v: InodeValue) !Record {
    return Record.put(testing.allocator, &fold_key, seq, &v.encode());
}

fn tTimes(seq: u64, mtime: u64, ctime: u64) !Record {
    return Record.delta(testing.allocator, &fold_key, seq, InodeDelta.times(mtime, ctime));
}

fn tDel(seq: u64) !Record {
    return Record.delete(testing.allocator, &fold_key, seq);
}

fn refs(records: []const Record, out: []RecordRef) []RecordRef {
    for (records, 0..) |*r, i| out[i] = r.ref();
    return out[0..records.len];
}

fn hexBytes(comptime hex: []const u8) [hex.len / 2]u8 {
    var out: [hex.len / 2]u8 = undefined;
    _ = std.fmt.hexToBytes(&out, hex) catch unreachable;
    return out;
}

test "inode value is 68 B fixed width: 5×u32 ‖ rdev u64 ‖ size u64 ‖ 3×i64 ns ‖ flags2 u32 ‖ reserved[4] = 0; exactly that form decodes" {
    const v: InodeValue = .{ .mode = 1, .uid = 2, .gid = 3, .nlink = 4, .flags = 5, .rdev = 6, .size = 7, .atime = 8, .mtime = 9, .ctime = 10, .flags2 = 11 };
    const bytes = v.encode();
    try testing.expectEqual(@as(usize, 68), bytes.len);
    try testing.expectEqual(@as(usize, 68), INODE_VALUE_LEN);
    try testing.expectEqualSlices(u8, &.{ 1, 0, 0, 0 }, bytes[0..4]);
    try testing.expectEqualSlices(u8, &.{ 2, 0, 0, 0 }, bytes[4..8]);
    try testing.expectEqualSlices(u8, &.{ 3, 0, 0, 0 }, bytes[8..12]);
    try testing.expectEqualSlices(u8, &.{ 4, 0, 0, 0 }, bytes[12..16]);
    try testing.expectEqualSlices(u8, &.{ 5, 0, 0, 0 }, bytes[16..20]);
    try testing.expectEqualSlices(u8, &.{ 6, 0, 0, 0, 0, 0, 0, 0 }, bytes[20..28]);
    try testing.expectEqualSlices(u8, &.{ 7, 0, 0, 0, 0, 0, 0, 0 }, bytes[28..36]);
    try testing.expectEqualSlices(u8, &.{ 8, 0, 0, 0, 0, 0, 0, 0 }, bytes[36..44]);
    try testing.expectEqualSlices(u8, &.{ 9, 0, 0, 0, 0, 0, 0, 0 }, bytes[44..52]);
    try testing.expectEqualSlices(u8, &.{ 10, 0, 0, 0, 0, 0, 0, 0 }, bytes[52..60]);
    try testing.expectEqualSlices(u8, &.{ 11, 0, 0, 0 }, bytes[60..64]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0 }, bytes[64..68]);
    try testing.expectEqual(v, try InodeValue.decode(&bytes));
    var with_rdev = iv(7);
    with_rdev.rdev = 0x0001_0000_0000_0501;
    try testing.expectEqual(with_rdev, try InodeValue.decode(&with_rdev.encode()));
    // The one form: a non-zero reserved byte, truncation and trailing bytes
    // all refuse.
    var reserved = bytes;
    reserved[66] = 1;
    try testing.expectError(error.Corrupt, InodeValue.decode(&reserved));
    try testing.expectError(error.Corrupt, InodeValue.decode(bytes[0 .. bytes.len - 1]));
    try testing.expectError(error.Corrupt, InodeValue.decode(&(bytes ++ [_]u8{0})));
    try testing.expectError(error.Corrupt, InodeValue.decode(&.{}));
    // The pre-v4 57 B varint form (`01 ‖ 56 fixed bytes`) and its
    // non-canonical twin (`81 00 ‖ …`, which the LEB128 decoder accepted —
    // format audit row 4) are both refused: no version tag exists to be
    // non-canonical about.
    const legacy = [_]u8{0x01} ++ @as([56]u8, @splat(0));
    try testing.expectError(error.Corrupt, InodeValue.decode(&legacy));
    const legacy_noncanonical = [_]u8{ 0x81, 0x00 } ++ @as([56]u8, @splat(0));
    try testing.expectError(error.Corrupt, InodeValue.decode(&legacy_noncanonical));
}

test "the directory depth rides flags2's low byte: born at a depth, read back after the codec; a record without one (a file, a pre-field directory) reads 0; bits 8..31 are untouched by it" {
    var d: InodeValue = .{ .mode = 0o40755, .nlink = 2, .flags2 = InodeValue.flags2ForDepth(3) };
    try testing.expectEqual(@as(u8, 3), d.dirDepth());
    try testing.expectEqual(@as(u8, 3), (try InodeValue.decode(&d.encode())).dirDepth());
    d.flags2 = InodeValue.flags2ForDepth(255);
    try testing.expectEqual(@as(u8, 255), (try InodeValue.decode(&d.encode())).dirDepth());
    try testing.expectEqual(@as(u8, 0), (InodeValue{ .mode = 0o100644, .nlink = 1 }).dirDepth());
    try testing.expectEqual(@as(u8, 0), (InodeValue{ .mode = 0o40755, .nlink = 2 }).dirDepth());
    try testing.expectEqual(@as(u32, 0x100 | 9), InodeValue.flags2ForDepth(9) | 0x100);
    try testing.expectEqual(@as(u8, 9), (InodeValue{ .flags2 = 0x100 | 9 }).dirDepth());
}

test "the inode field table is derived from the wire struct: 11 fields, bit i = field i, widths = wire widths; the delta mask covers exactly them" {
    try testing.expectEqual(@as(usize, 11), inode_field_table.len);
    try testing.expectEqualStrings("mode", inode_field_table[0].name);
    try testing.expectEqualStrings("flags2", inode_field_table[10].name);
    try testing.expectEqual(@as(u16, 1 << 5), DELTA_RDEV);
    try testing.expectEqual(@as(usize, 8), inode_field_table[5].width);
    try testing.expectEqual(@as(u16, 1 << 10), DELTA_FLAGS2);
    try testing.expectEqual(@as(u16, 0x7FF), DELTA_MASK_ALL);
    try testing.expectEqual(@as(u16, 0x300), DELTA_TIMES);
    var acc: u16 = 0;
    var width: usize = 0;
    for (inode_field_table, 0..) |f, i| {
        try testing.expectEqual(@as(u16, 1) << @intCast(i), f.bit);
        acc |= f.bit;
        width += f.width;
    }
    try testing.expectEqual(DELTA_MASK_ALL, acc);
    // The Put value's bytes are the fields in table order, then reserved.
    try testing.expectEqual(INODE_VALUE_LEN - 4, width);
    try testing.expectEqual(@as(usize, 66), INODE_DELTA_MAX_LEN);
}

test "dentry value roundtrips and caps the name at 255" {
    const d: DentryValue = .{ .child_ino = 42, .file_type = 8, .name = "hello" };
    var buf: [512]u8 = undefined;
    const bytes = try d.encodeInto(&buf);
    try testing.expectEqual(@as(usize, 8 + 1 + 1 + 5), bytes.len);
    try testing.expectEqualSlices(u8, &.{ 42, 0, 0, 0, 0, 0, 0, 0, 8, 5, 'h', 'e', 'l', 'l', 'o' }, bytes);
    const back = try DentryValue.decode(bytes);
    try testing.expectEqual(d.child_ino, back.child_ino);
    try testing.expectEqual(d.file_type, back.file_type);
    try testing.expectEqualStrings(d.name, back.name);
    const max_name: [255]u8 = @splat('x');
    const max: DentryValue = .{ .child_ino = 1, .file_type = 4, .name = &max_name };
    const max_bytes = try max.encodeInto(&buf);
    try testing.expectEqualStrings(&max_name, (try DentryValue.decode(max_bytes)).name);
    const over_name: [256]u8 = @splat('x');
    try testing.expectError(error.NameTooLong, (DentryValue{ .child_ino = 1, .file_type = 8, .name = &over_name }).encodeInto(&buf));
    try testing.expectError(error.NameTooLong, (DentryValue{ .child_ino = 1, .file_type = 8, .name = &over_name }).encodeAlloc(testing.allocator));
    // Lying name_len: longer and shorter than the remainder.
    var lying: [15]u8 = bytes[0..15].*;
    lying[9] = 200;
    try testing.expectError(error.Corrupt, DentryValue.decode(&lying));
    lying[9] = 2;
    try testing.expectError(error.Corrupt, DentryValue.decode(&lying));
    try testing.expectError(error.Corrupt, DentryValue.decode(&@as([9]u8, @splat(0))));
    const owned = try d.encodeAlloc(testing.allocator);
    defer testing.allocator.free(owned);
    var fresh: [16]u8 = undefined;
    try testing.expectEqualSlices(u8, try d.encodeInto(&fresh), owned);
}

test "xattr value roundtrips, caps the name at 255, and takes the remainder as the value" {
    const value: [300]u8 = @splat(0xAB);
    const x: XattrValue = .{ .name = "user.layout", .value = &value };
    const bytes = try x.encodeAlloc(testing.allocator);
    defer testing.allocator.free(bytes);
    try testing.expectEqual(@as(usize, 1 + 11 + 300), bytes.len);
    try testing.expectEqual(@as(u8, 11), bytes[0]);
    const back = try XattrValue.decode(bytes);
    try testing.expectEqualStrings("user.layout", back.name);
    try testing.expectEqualSlices(u8, &value, back.value);
    const empty: XattrValue = .{ .name = "user.empty", .value = "" };
    var buf: [64]u8 = undefined;
    const eb = try empty.encodeInto(&buf);
    try testing.expectEqual(@as(usize, 0), (try XattrValue.decode(eb)).value.len);
    const over_name: [256]u8 = @splat('n');
    try testing.expectError(error.NameTooLong, (XattrValue{ .name = &over_name, .value = "" }).encodeInto(&buf));
    // Lying name_len beyond the buffer; empty input.
    var lying = [_]u8{ 255, 'a', 'b' };
    try testing.expectError(error.Corrupt, XattrValue.decode(&lying));
    try testing.expectError(error.Corrupt, XattrValue.decode(&.{}));
    lying[0] = 0;
    // A zero-length name with a 2-byte value is well-formed.
    try testing.expectEqualSlices(u8, "ab", (try XattrValue.decode(&lying)).value);
}

test "xattr caps: value cap min(64 KiB, node_size/4); record cap adds the 256 B envelope" {
    try testing.expectEqual(@as(usize, 65_536), RECORD_VALUE_CAP_CEILING);
    try testing.expectEqual(@as(usize, 256), XATTR_RECORD_ENVELOPE_MAX);
    try testing.expectEqual(@as(usize, 65_536), xattrValueCap(256 * 1024));
    try testing.expectEqual(@as(usize, 16_384), xattrValueCap(64 * 1024));
    try testing.expectEqual(@as(usize, 32_768), xattrValueCap(128 * 1024));
    try testing.expectEqual(@as(usize, 65_536), xattrValueCap(1024 * 1024));
    try testing.expectEqual(@as(usize, 65_792), recordValueCap(256 * 1024));
    try testing.expectEqual(@as(usize, 16_640), recordValueCap(64 * 1024));
}

test "block-ref value is version ‖ flags ‖ reserved u16, 4 B; every refusal" {
    try testing.expectEqualSlices(u8, &.{ 1, 0, 0, 0 }, &(BlockRefValue{ .map_blob = false, .shared = false }).encode());
    try testing.expectEqualSlices(u8, &.{ 1, 1, 0, 0 }, &(BlockRefValue{ .map_blob = true, .shared = false }).encode());
    try testing.expectEqualSlices(u8, &.{ 1, 3, 0, 0 }, &(BlockRefValue{ .map_blob = true, .shared = true }).encode());
    try testing.expectEqual(BlockRefValue{ .map_blob = false, .shared = true }, try BlockRefValue.decode(&.{ 1, 2, 0, 0 }));
    try testing.expectError(error.Corrupt, BlockRefValue.decode(&.{ 2, 0, 0, 0 }));
    try testing.expectError(error.Corrupt, BlockRefValue.decode(&.{ 1, 4, 0, 0 }));
    try testing.expectError(error.Corrupt, BlockRefValue.decode(&.{ 1, 0, 1, 0 }));
    try testing.expectError(error.Corrupt, BlockRefValue.decode(&.{ 1, 0, 0 }));
    try testing.expectError(error.Corrupt, BlockRefValue.decode(&.{ 1, 0, 0, 0, 0 }));
}

test "block-map values: POINT/STRING/RUN/POINT2/RUN2 byte layouts and every refusal" {
    var buf: [64]u8 = undefined;
    const point: MapEntry = .{ .point = .{ .vol_tag = 0x11, .offset = 0x22 } };
    const pb = point.encodeInto(&buf);
    try testing.expectEqualSlices(u8, &.{ 1, 1, 0x11, 0, 0, 0, 0, 0, 0, 0, 0x22, 0, 0, 0, 0, 0, 0, 0 }, pb);
    try testing.expectEqual(point, try MapEntry.decode(pb));
    try testing.expectEqual(@as(u32, 1), point.runLen());

    const str: MapEntry = .{ .string = "damaged:x" };
    const sb = str.encodeInto(&buf);
    try testing.expectEqualSlices(u8, "\x01\x02damaged:x", sb);
    try testing.expectEqualStrings("damaged:x", (try MapEntry.decode(sb)).string);
    try testing.expectEqual(@as(usize, 0), (try MapEntry.decode(&.{ 1, 2 })).string.len);

    const run: MapEntry = .{ .run = .{ .vol_tag = 0x11, .start_offset = 0x22, .len = 4096 } };
    const rb = run.encodeInto(&buf);
    try testing.expectEqual(@as(usize, 22), rb.len);
    try testing.expectEqualSlices(u8, &.{ 0, 0x10, 0, 0 }, rb[18..22]);
    try testing.expectEqual(run, try MapEntry.decode(rb));
    try testing.expectEqual(@as(u32, 4096), run.runLen());
    // Run length outside 2..=4096: a 1-run is a point; a wider run is
    // invisible to the floor probe.
    var bad_run = rb[0..22].*;
    std.mem.writeInt(u32, bad_run[18..22], 1, .little);
    try testing.expectError(error.Corrupt, MapEntry.decode(&bad_run));
    std.mem.writeInt(u32, bad_run[18..22], 4097, .little);
    try testing.expectError(error.Corrupt, MapEntry.decode(&bad_run));
    std.mem.writeInt(u32, bad_run[18..22], 2, .little);
    _ = try MapEntry.decode(&bad_run);

    const p2: MapEntry = .{ .point_stamped = .{ .vol_tag = 1, .offset = 2, .incarnation = 3 } };
    const p2b = p2.encodeInto(&buf);
    try testing.expectEqual(@as(usize, 26), p2b.len);
    try testing.expectEqual(@as(u8, 4), p2b[1]);
    try testing.expectEqual(p2, try MapEntry.decode(p2b));
    var p2_zero = p2b[0..26].*;
    std.mem.writeInt(u64, p2_zero[18..26], 0, .little);
    try testing.expectError(error.Corrupt, MapEntry.decode(&p2_zero));

    const r2: MapEntry = .{ .run_stamped = .{ .vol_tag = 1, .start_offset = 2, .len = 3, .start_incarnation = 100 } };
    const r2b = r2.encodeInto(&buf);
    try testing.expectEqual(@as(usize, 30), r2b.len);
    try testing.expectEqual(@as(u8, 5), r2b[1]);
    try testing.expectEqual(r2, try MapEntry.decode(r2b));
    try testing.expectEqual(@as(u32, 3), r2.runLen());
    var r2_bad = r2b[0..30].*;
    std.mem.writeInt(u64, r2_bad[22..30], 0, .little);
    try testing.expectError(error.Corrupt, MapEntry.decode(&r2_bad));
    // start + len − 1 overflows: no emitter produces it.
    std.mem.writeInt(u64, r2_bad[22..30], std.math.maxInt(u64) - 1, .little);
    try testing.expectError(error.Corrupt, MapEntry.decode(&r2_bad));
    std.mem.writeInt(u64, r2_bad[22..30], std.math.maxInt(u64) - 2, .little);
    _ = try MapEntry.decode(&r2_bad);

    // Shared gates: length, version, unknown kind, wrong per-kind length.
    try testing.expectError(error.Corrupt, MapEntry.decode(&.{1}));
    try testing.expectError(error.Corrupt, MapEntry.decode(&.{ 2, 1 }));
    try testing.expectError(error.Corrupt, MapEntry.decode(&.{ 1, 6 }));
    try testing.expectError(error.Corrupt, MapEntry.decode(pb[0..17]));
    try testing.expectError(error.Corrupt, MapEntry.decode(rb[0..21]));
    try testing.expectError(error.Corrupt, MapEntry.decode(&(p2b[0..26].* ++ [_]u8{0})));
    try testing.expectError(error.Corrupt, MapEntry.decode(r2b[0..29]));
}

test "inode delta: times is 18 B with mask 0x300 and touches only the time fields" {
    const d = InodeDelta.times(111, 222);
    try testing.expectEqual(DELTA_TIMES, d.mask);
    const enc = d.encode();
    try testing.expectEqual(@as(usize, 18), enc.slice().len);
    try testing.expectEqualSlices(u8, &.{ 0x00, 0x03 }, enc.slice()[0..2]);
    try testing.expectEqualSlices(u8, &.{ 111, 0, 0, 0, 0, 0, 0, 0 }, enc.slice()[2..10]);
    try testing.expectEqualSlices(u8, &.{ 222, 0, 0, 0, 0, 0, 0, 0 }, enc.slice()[10..18]);
    try testing.expectEqual(d, try InodeDelta.decode(enc.slice()));
    const c = InodeDelta.ctime(9).encode();
    try testing.expectEqualSlices(u8, &.{ 0x00, 0x02, 9, 0, 0, 0, 0, 0, 0, 0 }, c.slice());
    var base = iv(3);
    const orig = base;
    d.apply(&base);
    try testing.expectEqual(@as(u64, 111), base.mtime);
    try testing.expectEqual(@as(u64, 222), base.ctime);
    base.mtime = orig.mtime;
    base.ctime = orig.ctime;
    try testing.expectEqual(orig, base);
}

test "inode delta: full mask is 66 B in the Put value's field order; unknown bits, truncation, trailing bytes refuse" {
    const full: InodeDelta = .{ .mask = DELTA_MASK_ALL, .fields = iv(9) };
    const enc = full.encode();
    try testing.expectEqual(@as(usize, 66), enc.slice().len);
    try testing.expectEqual(full, try InodeDelta.decode(enc.slice()));
    var base: InodeValue = .{};
    full.apply(&base);
    try testing.expectEqual(iv(9), base);
    // Canonical order = the Put value's: a full-mask delta's fields ARE the
    // Put value's bytes minus the reserved tail (one table for both).
    const f = iv(9);
    try testing.expectEqualSlices(u8, f.encode()[0 .. INODE_VALUE_LEN - 4], enc.slice()[2..]);
    try testing.expectEqual(f.mode, std.mem.readInt(u32, enc.slice()[2..6], .little));
    try testing.expectEqual(f.rdev, std.mem.readInt(u64, enc.slice()[22..30], .little));
    try testing.expectEqual(f.size, std.mem.readInt(u64, enc.slice()[30..38], .little));
    try testing.expectEqual(f.ctime, std.mem.readInt(u64, enc.slice()[54..62], .little));
    try testing.expectEqual(f.flags2, std.mem.readInt(u32, enc.slice()[62..66], .little));
    // A sparse mask skips unmasked fields: {uid, size} = 2 + 4 + 8.
    const sparse: InodeDelta = .{ .mask = DELTA_UID | DELTA_SIZE, .fields = .{ .uid = 7, .size = 8 } };
    const sb = sparse.encode();
    try testing.expectEqualSlices(u8, &.{ 0x42, 0x00, 7, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0 }, sb.slice());
    try testing.expectEqual(sparse, try InodeDelta.decode(sb.slice()));
    var unknown = InodeDelta.times(1, 2).encode();
    std.mem.writeInt(u16, unknown.buf[0..2], DELTA_MASK_ALL + 1, .little);
    try testing.expectError(error.Corrupt, InodeDelta.decode(unknown.slice()));
    const ok = InodeDelta.times(1, 2).encode();
    try testing.expectError(error.Corrupt, InodeDelta.decode(ok.slice()[0..17]));
    try testing.expectError(error.Corrupt, InodeDelta.decode(&(ok.buf[0..18].* ++ [_]u8{0})));
    try testing.expectError(error.Corrupt, InodeDelta.decode(&.{0}));
    // The layout magic is refused by the inode-delta decoder.
    try testing.expectError(error.Corrupt, InodeDelta.decode(&.{ 0x31, 0x4C, 0 }));
}

// Fixture bytes produced by bincode 1.3.3 serializing the reference's
// `LayoutMetadata` (serde derive, `serialize_block_map_sorted`) — the base
// is bincode, so its canonical form is pinned by bytes from the Rust side,
// not re-derived.
//
// A: `apply_merges_entries_and_overwrites_non_map_fields`'s base
//    {file_type "staged", size 100, prefix "keepme-not", file_id "stage-old", map {1→"old1"}}.
const fixture_layout_a = hexBytes("0600000000000000737461676564640000000000000000010a000000000000006b6565706d652d6e6f7401090000000000000073746167652d6f6c64000101000000000000000100000004000000000000006f6c6431");
// B: A with `sample()` applied — {striped, 12 MiB, block_map_id "block_map_42",
//    prefix None, file_id "stage-abc", data_key [9,8,7], map {0→"b0://0", 1→"old1", 2→"b0://8388608"}}.
const fixture_layout_b = hexBytes("0700000000000000737472697065640000c00000000000010c00000000000000626c6f636b5f6d61705f34320001090000000000000073746167652d61626301030000000000000009080701030000000000000000000000060000000000000062303a2f2f300100000004000000000000006f6c6431020000000c0000000000000062303a2f2f38333838363038");
// C: `LayoutMetadata::default()`.
const fixture_layout_c = hexBytes("000000000000000000000000000000000000000000");
// D: bset.rs `layout_put(_, _, [(0, "be://data:a")])` — {striped, 4096, prefix "be://data", map {0→"be://data:a"}}.
const fixture_layout_d = hexBytes("07000000000000007374726970656400100000000000000001090000000000000062653a2f2f646174610000010100000000000000000000000b0000000000000062653a2f2f646174613a61");
// E: the indirect base {striped, 4, block_map_id "indirect:b0://123"}.
const fixture_layout_e = hexBytes("0700000000000000737472697065640400000000000000011100000000000000696e6469726563743a62303a2f2f31323300000000");
// F: layout_wire.rs `sample().encode()` (unversioned); G: the same stamped (0x11, 0x12).
const fixture_delta_f = hexBytes("314c0d07737472697065640000c000000000000c00626c6f636b5f6d61705f3432090073746167652d61626303000908070200000000000000060062303a2f2f30020000000c0062303a2f2f38333838363038");
const fixture_delta_g = hexBytes("314c1d1100000000000000120000000000000007737472697065640000c000000000000c00626c6f636b5f6d61705f3432090073746167652d61626303000908070200000000000000060062303a2f2f30020000000c0062303a2f2f38333838363038");

const sample_entries = [_]LayoutEntry{ .{ .block = 0, .key = "b0://0" }, .{ .block = 2, .key = "b0://8388608" } };
const sample_delta: LayoutDelta = .{
    .file_type = "striped",
    .size = 12 * 1024 * 1024,
    .block_map_id = "block_map_42",
    .block_prefix = null,
    .file_id = "stage-abc",
    .data_key = &.{ 9, 8, 7 },
    .entries = &sample_entries,
};

fn expectLayoutDeltaEql(want: LayoutDelta, got: LayoutDelta) !void {
    try testing.expectEqualStrings(want.file_type, got.file_type);
    try testing.expectEqual(want.size, got.size);
    try testing.expectEqualDeep(want.block_map_id, got.block_map_id);
    try testing.expectEqualDeep(want.block_prefix, got.block_prefix);
    try testing.expectEqualDeep(want.file_id, got.file_id);
    try testing.expectEqualDeep(want.data_key, got.data_key);
    try testing.expectEqualDeep(want.entries, got.entries);
    try testing.expectEqual(want.base_version, got.base_version);
    try testing.expectEqual(want.version, got.version);
}

test "layout base: bincode fixtures decode and re-encode byte-identically; the default is 21 zero bytes" {
    const gpa = testing.allocator;
    var a = try Layout.decodeBase(gpa, &fixture_layout_a);
    defer a.deinit(gpa);
    try testing.expectEqualStrings("staged", a.file_type);
    try testing.expectEqual(@as(u64, 100), a.size);
    try testing.expectEqual(@as(?[]const u8, null), a.block_map_id);
    try testing.expectEqualStrings("keepme-not", a.block_prefix.?);
    try testing.expectEqualStrings("stage-old", a.file_id.?);
    try testing.expectEqual(@as(?[]const u8, null), a.data_key);
    try testing.expectEqualStrings("old1", a.get(1).?);
    try testing.expectEqual(@as(?[]const u8, null), a.get(0));
    const a_enc = try a.encodeAlloc(gpa);
    defer gpa.free(a_enc);
    try testing.expectEqualSlices(u8, &fixture_layout_a, a_enc);

    var c = try Layout.decodeBase(gpa, &fixture_layout_c);
    defer c.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), c.file_type.len);
    try testing.expect(c.block_map == null);
    const c_enc = try c.encodeAlloc(gpa);
    defer gpa.free(c_enc);
    try testing.expectEqualSlices(u8, &fixture_layout_c, c_enc);

    var d = try Layout.decodeBase(gpa, &fixture_layout_d);
    defer d.deinit(gpa);
    try testing.expectEqualStrings("be://data:a", d.get(0).?);
    const d_enc = try d.encodeAlloc(gpa);
    defer gpa.free(d_enc);
    try testing.expectEqualSlices(u8, &fixture_layout_d, d_enc);

    // The Zig encoder of the same logical value reproduces bincode's bytes.
    var built: Layout = .{ .file_type = "striped", .size = 4096, .block_prefix = "be://data", .block_map = .empty };
    defer built.deinit(gpa);
    try built.insert(gpa, 0, "be://data:a");
    const built_enc = try built.encodeAlloc(gpa);
    defer gpa.free(built_enc);
    try testing.expectEqualSlices(u8, &fixture_layout_d, built_enc);
}

test "layout base decode accepts what bincode 1.3 accepts: any map order (later duplicate wins), trailing bytes; refuses bad tags and non-UTF-8" {
    const gpa = testing.allocator;
    // Hand-built: map with (5,"a"),(5,"b"),(1,"z") in that order.
    var raw: std.ArrayList(u8) = .empty;
    defer raw.deinit(gpa);
    try raw.appendSlice(gpa, &.{ 1, 0, 0, 0, 0, 0, 0, 0, 'x' });
    try raw.appendSlice(gpa, &.{ 1, 0, 0, 0, 0, 0, 0, 0 });
    try raw.appendSlice(gpa, &.{ 0, 0, 0, 0 });
    try raw.appendSlice(gpa, &.{ 1, 3, 0, 0, 0, 0, 0, 0, 0 });
    for ([_]struct { u32, []const u8 }{ .{ 5, "a" }, .{ 5, "b" }, .{ 1, "z" } }) |e| {
        var b: [4]u8 = undefined;
        std.mem.writeInt(u32, &b, e[0], .little);
        try raw.appendSlice(gpa, &b);
        try raw.appendSlice(gpa, &.{ @intCast(e[1].len), 0, 0, 0, 0, 0, 0, 0 });
        try raw.appendSlice(gpa, e[1]);
    }
    var l = try Layout.decodeBase(gpa, raw.items);
    defer l.deinit(gpa);
    try testing.expectEqual(@as(usize, 2), l.block_map.?.items.len);
    try testing.expectEqualStrings("z", l.get(1).?);
    try testing.expectEqualStrings("b", l.get(5).?);
    try testing.expectEqual(@as(u32, 1), l.block_map.?.items[0].block);
    // Trailing bytes decode (bincode 1.3 `deserialize` allows them) and the
    // canonical re-encode drops them.
    try raw.append(gpa, 0xAA);
    var t = try Layout.decodeBase(gpa, raw.items);
    defer t.deinit(gpa);
    const t_enc = try t.encodeAlloc(gpa);
    defer gpa.free(t_enc);
    // 9 (file_type) + 8 (size) + 4 (None tags) + 9 (Some ‖ count) + 2 × 13 (entries).
    try testing.expectEqual(@as(usize, 9 + 8 + 4 + 9 + 2 * 13), t_enc.len);
    try testing.expectEqualSlices(u8, raw.items[0..22], t_enc[0..22]);
    // Option tag 2 refuses; non-UTF-8 file_type refuses; a length claim
    // beyond the container refuses.
    var tag2 = fixture_layout_c;
    tag2[16] = 2;
    try testing.expectError(error.LayoutBaseUndecodable, Layout.decodeBase(gpa, &tag2));
    try testing.expectError(error.LayoutBaseUndecodable, Layout.decodeBase(gpa, &([_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0xFF } ++ fixture_layout_c[8..].*)));
    try testing.expectError(error.LayoutBaseUndecodable, Layout.decodeBase(gpa, &[_]u8{ 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0 }));
    try testing.expectError(error.LayoutBaseUndecodable, Layout.decodeBase(gpa, &@as([3]u8, @splat(0xFF))));
    try testing.expectError(error.LayoutBaseUndecodable, Layout.decodeBase(gpa, ""));
}

test "layout base refusals are typed: legacy JSON, indirect, kvmap; decodeAny keeps indirect" {
    const gpa = testing.allocator;
    try testing.expectError(error.LayoutBaseLegacyJson, Layout.decodeBase(gpa, "{\"file_type\":\"striped\"}"));
    try testing.expectError(error.LayoutBaseIndirect, Layout.decodeBase(gpa, &fixture_layout_e));
    var any = try Layout.decodeAny(gpa, &fixture_layout_e);
    defer any.deinit(gpa);
    try testing.expectEqualStrings("indirect:b0://123", any.block_map_id.?);
    var kv: Layout = .{ .file_type = "striped", .size = 4, .block_map_id = "kvmap:1" };
    const kv_enc = try kv.encodeAlloc(gpa);
    defer gpa.free(kv_enc);
    try testing.expectError(error.LayoutBaseKvmap, Layout.decodeBase(gpa, kv_enc));
}

test "layout delta: sample() encodes to the reference bytes (unversioned and versioned) and decodes back; strictness" {
    const gpa = testing.allocator;
    const f = try sample_delta.encodeAlloc(gpa);
    defer gpa.free(f);
    try testing.expectEqualSlices(u8, &fixture_delta_f, f);
    try testing.expect(isLayoutDelta(f));
    try testing.expectEqual(@as(?LayoutVersions, null), layoutDeltaVersions(f));
    var dec = try LayoutDelta.decode(gpa, f);
    defer dec.deinit(gpa);
    try expectLayoutDeltaEql(sample_delta, dec);

    var versioned = sample_delta;
    versioned.setVersions(0x11, 0x12);
    const g = try versioned.encodeAlloc(gpa);
    defer gpa.free(g);
    try testing.expectEqualSlices(u8, &fixture_delta_g, g);
    try testing.expectEqual(LayoutVersions{ .base_version = 0x11, .version = 0x12 }, layoutDeltaVersions(g).?);
    // The strip seam: the unversioned encode of a versioned delta is the
    // pre-item-9 wire.
    const stripped = try versioned.encodeUnversionedAlloc(gpa);
    defer gpa.free(stripped);
    try testing.expectEqualSlices(u8, &fixture_delta_f, stripped);
    // setVersions(_, 0) zeroes the pair together.
    versioned.setVersions(5, 0);
    try testing.expectEqual(@as(u64, 0), versioned.base_version);

    // Truncation and trailing bytes refuse; wrong magic is not a layout delta.
    try testing.expectError(error.Corrupt, LayoutDelta.decode(gpa, f[0 .. f.len - 1]));
    const trailing = try std.mem.concat(gpa, u8, &.{ f, &.{0} });
    defer gpa.free(trailing);
    try testing.expectError(error.Corrupt, LayoutDelta.decode(gpa, trailing));
    var wrong = try gpa.dupe(u8, f);
    defer gpa.free(wrong);
    wrong[0] ^= 0xFF;
    try testing.expect(!isLayoutDelta(wrong));
    try testing.expectError(error.Corrupt, LayoutDelta.decode(gpa, wrong));
    // Unknown flag bit; versioned flag with version 0.
    var flag = try gpa.dupe(u8, f);
    defer gpa.free(flag);
    flag[2] |= 0x20;
    try testing.expectError(error.Corrupt, LayoutDelta.decode(gpa, flag));
    var zero_v = try gpa.dupe(u8, g);
    defer gpa.free(zero_v);
    std.mem.writeInt(u64, zero_v[11..19], 0, .little);
    try testing.expectError(error.Corrupt, LayoutDelta.decode(gpa, zero_v));
    try testing.expectEqual(@as(?LayoutVersions, null), layoutDeltaVersions(zero_v));
    // An entry-count claim beyond the container refuses before allocating.
    var lying = try gpa.dupe(u8, f);
    defer gpa.free(lying);
    const count_off = f.len - (sample_entries[0].key.len + 6) - (sample_entries[1].key.len + 6) - 4;
    std.mem.writeInt(u32, lying[count_off..][0..4], std.math.maxInt(u32), .little);
    try testing.expectError(error.Corrupt, LayoutDelta.decode(gpa, lying));
    // Non-UTF-8 in an entry key refuses.
    var bad_utf8 = try gpa.dupe(u8, f);
    defer gpa.free(bad_utf8);
    bad_utf8[bad_utf8.len - 1] = 0xFF;
    try testing.expectError(error.Corrupt, LayoutDelta.decode(gpa, bad_utf8));
}

test "layout delta apply merges entries and overwrites every non-map field: A + sample == B, byte-deterministic" {
    const gpa = testing.allocator;
    var base = try Layout.decodeBase(gpa, &fixture_layout_a);
    defer base.deinit(gpa);
    try sample_delta.applyTo(gpa, &base);
    const folded = try base.encodeAlloc(gpa);
    defer gpa.free(folded);
    try testing.expectEqualSlices(u8, &fixture_layout_b, folded);
    try testing.expectEqualStrings("striped", base.file_type);
    try testing.expectEqual(@as(?[]const u8, null), base.block_prefix);
    try testing.expectEqual(@as(usize, 3), base.block_map.?.items.len);
    try testing.expectEqualStrings("old1", base.get(1).?);
    // Idempotent: applying again yields the same bytes.
    try sample_delta.applyTo(gpa, &base);
    const again = try base.encodeAlloc(gpa);
    defer gpa.free(again);
    try testing.expectEqualSlices(u8, &fixture_layout_b, again);
    // Empty entries never create a map.
    var none: Layout = .{};
    defer none.deinit(gpa);
    const d: LayoutDelta = .{ .file_type = "inline", .size = 3 };
    try d.applyTo(gpa, &none);
    try testing.expect(none.block_map == null);
}

test "restamp: zero version copies; a versioned wire is patched at [3..19); an unversioned wire re-encodes versioned" {
    const gpa = testing.allocator;
    const copy = try restampDeltaVersions(gpa, &fixture_delta_f, 7, 0);
    defer gpa.free(copy);
    try testing.expectEqualSlices(u8, &fixture_delta_f, copy);
    const patched = try restampDeltaVersions(gpa, &fixture_delta_g, 0, 0x12);
    defer gpa.free(patched);
    try testing.expectEqual(LayoutVersions{ .base_version = 0, .version = 0x12 }, layoutDeltaVersions(patched).?);
    try testing.expectEqualSlices(u8, fixture_delta_g[19..], patched[19..]);
    const upgraded = try restampDeltaVersions(gpa, &fixture_delta_f, 0x11, 0x12);
    defer gpa.free(upgraded);
    try testing.expectEqualSlices(u8, &fixture_delta_g, upgraded);
    try testing.expectError(error.Corrupt, restampDeltaVersions(gpa, "garbage", 1, 2));
}

test "record framing: 16 B header {key_len u16, kind u8, flags u8 = 0, val_len u32, seq u64}; all three kinds roundtrip sequentially" {
    const gpa = testing.allocator;
    var records = [_]Record{
        try Record.put(gpa, &key.inodeKey(5), 9, &iv(1).encode()),
        try Record.delta(gpa, &key.inodeKey(5), 10, InodeDelta.times(4, 5)),
        try Record.delete(gpa, &key.inodeKey(5), 11),
    };
    defer deinitRecords(gpa, &records);
    try testing.expectEqual(RecordKind.delta, records[1].kind);
    try testing.expectEqual(@as(usize, 0), records[2].value.len);
    var buf: std.ArrayList(u8) = .empty;
    defer buf.deinit(gpa);
    for (&records) |*rec| {
        const before = buf.items.len;
        try rec.ref().appendTo(gpa, &buf);
        try testing.expectEqual(rec.ref().encodedLen(), buf.items.len - before);
    }
    // Header bytes of the first record: key_len 9, kind 1, flags 0, val_len 68, seq 9.
    try testing.expectEqual(@as(usize, 16), RECORD_HEADER_LEN);
    try testing.expectEqualSlices(u8, &.{ 9, 0, 1, 0, 68, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0 }, buf.items[0..16]);
    var cursor: usize = 0;
    for (&records) |*rec| {
        const d = try RecordRef.decode(buf.items[cursor..]);
        try testing.expect(d.rec.eql(rec.ref()));
        cursor += d.used;
    }
    try testing.expectEqual(buf.items.len, cursor);
}

test "record decode rejects malformed framing: truncation, lying lengths, bad kinds, a non-zero flags byte, empty key, tombstone with value" {
    const gpa = testing.allocator;
    var rec = try Record.put(gpa, &key.inodeKey(5), 9, &iv(1).encode());
    defer rec.deinit(gpa);
    var buf: std.ArrayList(u8) = .empty;
    defer buf.deinit(gpa);
    try rec.ref().appendTo(gpa, &buf);
    const b = buf.items;
    try testing.expectError(error.Corrupt, RecordRef.decode(b[0 .. RECORD_HEADER_LEN - 1]));
    try testing.expectError(error.Corrupt, RecordRef.decode(b[0 .. RECORD_HEADER_LEN + 3]));
    try testing.expectError(error.Corrupt, RecordRef.decode(b[0 .. b.len - 1]));
    var lying = try gpa.dupe(u8, b);
    defer gpa.free(lying);
    std.mem.writeInt(u16, lying[0..2], std.math.maxInt(u16), .little);
    try testing.expectError(error.Corrupt, RecordRef.decode(lying));
    lying[0..2].* = b[0..2].*;
    std.mem.writeInt(u32, lying[4..8], std.math.maxInt(u32), .little);
    try testing.expectError(error.Corrupt, RecordRef.decode(lying));
    lying[4..8].* = b[4..8].*;
    for ([_]u8{ 0, 4, 0xFF }) |bad_kind| {
        lying[2] = bad_kind;
        try testing.expectError(error.Corrupt, RecordRef.decode(lying));
    }
    lying[2] = 1;
    // The reserved flags byte: every non-zero value refuses.
    for ([_]u8{ 1, 0x80, 0xFF }) |flags| {
        lying[3] = flags;
        try testing.expectError(error.Corrupt, RecordRef.decode(lying));
    }
    lying[3] = 0;
    _ = try RecordRef.decode(lying);
    std.mem.writeInt(u16, lying[0..2], 0, .little);
    try testing.expectError(error.Corrupt, RecordRef.decode(lying));
    var del = try Record.delete(gpa, &key.inodeKey(5), 1);
    defer del.deinit(gpa);
    var dbuf: std.ArrayList(u8) = .empty;
    defer dbuf.deinit(gpa);
    try del.ref().appendTo(gpa, &dbuf);
    std.mem.writeInt(u32, dbuf.items[4..8], 1, .little);
    try dbuf.append(gpa, 0xFF);
    try testing.expectError(error.Corrupt, RecordRef.decode(dbuf.items));
}

test "fold: LWW newest Put wins and borrows; empty folds to absent" {
    const gpa = testing.allocator;
    var recs = [_]Record{ try tPut(5, iv(2)), try tPut(1, iv(1)) };
    defer deinitRecords(gpa, &recs);
    var rbuf: [8]RecordRef = undefined;
    var folded = try foldNewestFirst(gpa, refs(&recs, &rbuf));
    defer folded.deinit(gpa);
    try testing.expectEqual(@as(u64, 5), folded.put.seq);
    try testing.expect(!folded.put.owned);
    try testing.expectEqualSlices(u8, &iv(2).encode(), folded.put.value);
    try testing.expectEqual(folded.put.value.ptr, recs[0].value.ptr);
    var empty = try foldNewestFirst(gpa, &.{});
    defer empty.deinit(gpa);
    try testing.expectEqual(Folded.absent, empty);
}

test "fold: a tombstone shadows older records in both scan shapes; deltas above it are discarded, not counted" {
    const gpa = testing.allocator;
    const orphans_before = stats.meta_kv_delta_orphans.load();
    var recs = [_]Record{ try tDel(7), try tPut(3, iv(1)) };
    defer deinitRecords(gpa, &recs);
    var rbuf: [8]RecordRef = undefined;
    var f1 = try foldNewestFirst(gpa, refs(&recs, &rbuf));
    defer f1.deinit(gpa);
    try testing.expectEqual(Folded{ .tombstone = .{ .seq = 7 } }, f1);
    var recs2 = [_]Record{ try tTimes(9, 90, 99), try tDel(7), try tPut(3, iv(1)) };
    defer deinitRecords(gpa, &recs2);
    var f2 = try foldNewestFirst(gpa, refs(&recs2, &rbuf));
    defer f2.deinit(gpa);
    try testing.expectEqual(Folded{ .tombstone = .{ .seq = 7 } }, f2);
    try testing.expectEqual(orphans_before, stats.meta_kv_delta_orphans.load());
    try testing.expectEqual(@as(?[]const u8, null), f2.liveValue());
}

test "fold: a newer Put revives the key after a tombstone" {
    const gpa = testing.allocator;
    var recs = [_]Record{ try tPut(9, iv(4)), try tDel(7), try tPut(3, iv(1)) };
    defer deinitRecords(gpa, &recs);
    var rbuf: [8]RecordRef = undefined;
    var f = try foldNewestFirst(gpa, refs(&recs, &rbuf));
    defer f.deinit(gpa);
    try testing.expectEqual(@as(u64, 9), f.put.seq);
    try testing.expectEqualSlices(u8, &iv(4).encode(), f.liveValue().?);
}

test "fold: deltas apply ascending onto the newest Put; the folded seq is the newest delta's; the value is owned" {
    const gpa = testing.allocator;
    const base = iv(1);
    var recs = [_]Record{ try tTimes(9, 90, 99), try tTimes(5, 50, 55), try tPut(2, base), try tPut(1, iv(0)) };
    defer deinitRecords(gpa, &recs);
    var rbuf: [8]RecordRef = undefined;
    const decodes_before = stats.meta_kv_fold_record_decodes.load();
    var f = try foldNewestFirst(gpa, refs(&recs, &rbuf));
    defer f.deinit(gpa);
    try testing.expectEqual(@as(u64, 9), f.put.seq);
    try testing.expect(f.put.owned);
    const out = try InodeValue.decode(f.put.value);
    try testing.expectEqual(@as(u64, 90), out.mtime);
    try testing.expectEqual(@as(u64, 99), out.ctime);
    var want = base;
    want.mtime = 90;
    want.ctime = 99;
    try testing.expectEqual(want, out);
    // One decode count per record decoded: the base plus two deltas.
    try testing.expectEqual(decodes_before + 3, stats.meta_kv_fold_record_decodes.load());
}

test "fold: a delta without a base is a counted absent, one count per orphan" {
    const gpa = testing.allocator;
    const before = stats.meta_kv_delta_orphans.load();
    var recs = [_]Record{ try tTimes(9, 90, 99), try tTimes(5, 50, 55) };
    defer deinitRecords(gpa, &recs);
    var rbuf: [8]RecordRef = undefined;
    var f = try foldNewestFirst(gpa, refs(&recs, &rbuf));
    defer f.deinit(gpa);
    try testing.expectEqual(Folded.absent, f);
    try testing.expectEqual(before + 2, stats.meta_kv_delta_orphans.load());
}

test "fold: values stay opaque unless a delta forces a decode; a garbage base under a delta is Corrupt, never a panic" {
    const gpa = testing.allocator;
    var garbage = try Record.put(gpa, &fold_key, 4, "not-an-inode-value");
    defer garbage.deinit(gpa);
    var f = try foldNewestFirst(gpa, &.{garbage.ref()});
    defer f.deinit(gpa);
    try testing.expect(!f.put.owned);
    try testing.expectEqualStrings("not-an-inode-value", f.put.value);
    var d = try tTimes(9, 1, 2);
    defer d.deinit(gpa);
    try testing.expectError(error.Corrupt, foldNewestFirst(gpa, &.{ d.ref(), garbage.ref() }));
}

test "fold: mixed delta classes on one key refuse in both orders" {
    const gpa = testing.allocator;
    const inode_payload = InodeDelta.times(1, 2).encode();
    const base_v = iv(1).encode();
    const xbase_v = try (XattrValue{ .name = "layout", .value = &fixture_layout_a }).encodeAlloc(gpa);
    defer gpa.free(xbase_v);
    // Newest is an inode delta: the inode branch meets the older layout delta.
    try testing.expectError(error.MixedDeltaClasses, foldNewestFirst(gpa, &.{
        .{ .key = &fold_key, .seq = 9, .kind = .delta, .value = inode_payload.slice() },
        .{ .key = &fold_key, .seq = 8, .kind = .delta, .value = &fixture_delta_f },
        .{ .key = &fold_key, .seq = 1, .kind = .put, .value = &base_v },
    }));
    // Newest is a layout delta: the layout branch meets the older inode delta.
    try testing.expectError(error.MixedDeltaClasses, foldNewestFirst(gpa, &.{
        .{ .key = &fold_key, .seq = 9, .kind = .delta, .value = &fixture_delta_f },
        .{ .key = &fold_key, .seq = 8, .kind = .delta, .value = inode_payload.slice() },
        .{ .key = &fold_key, .seq = 1, .kind = .put, .value = xbase_v },
    }));
}

test "compact fold: tombstone elision respects the durable-tail boundary (seq < tail elides; seq == tail keeps)" {
    const gpa = testing.allocator;
    var d = try tDel(7);
    defer d.deinit(gpa);
    var out: std.ArrayList(Record) = .empty;
    defer {
        deinitRecords(gpa, out.items);
        out.deinit(gpa);
    }
    try compactFold(gpa, &.{d.ref()}, 8, &out);
    try testing.expectEqual(@as(usize, 0), out.items.len);
    try compactFold(gpa, &.{d.ref()}, 7, &out);
    try testing.expectEqual(@as(usize, 1), out.items.len);
    try testing.expectEqual(RecordKind.delete, out.items[0].kind);
    try testing.expectEqual(@as(u64, 7), out.items[0].seq);
    try testing.expectEqualSlices(u8, &fold_key, out.items[0].key);
    try testing.expectEqual(@as(usize, 0), out.items[0].value.len);
    try compactFold(gpa, &.{d.ref()}, 0, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
}

test "compact fold emits one folded Put or nothing per key" {
    const gpa = testing.allocator;
    var out: std.ArrayList(Record) = .empty;
    defer {
        deinitRecords(gpa, out.items);
        out.deinit(gpa);
    }
    var rbuf: [8]RecordRef = undefined;
    // Delta + base ⇒ one folded Put carrying the newest seq.
    var recs = [_]Record{ try tTimes(5, 50, 55), try tPut(2, iv(1)) };
    defer deinitRecords(gpa, &recs);
    try compactFold(gpa, refs(&recs, &rbuf), 0, &out);
    try testing.expectEqual(@as(usize, 1), out.items.len);
    try testing.expectEqual(RecordKind.put, out.items[0].kind);
    try testing.expectEqual(@as(u64, 5), out.items[0].seq);
    const folded = try InodeValue.decode(out.items[0].value);
    try testing.expectEqual(@as(u64, 50), folded.mtime);
    try testing.expectEqual(@as(u64, 55), folded.ctime);
    // Shadowed Puts collapse to the newest.
    var recs2 = [_]Record{ try tPut(5, iv(2)), try tPut(2, iv(1)) };
    defer deinitRecords(gpa, &recs2);
    try compactFold(gpa, refs(&recs2, &rbuf), 0, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    try testing.expectEqual(@as(u64, 5), out.items[1].seq);
    try testing.expectEqualSlices(u8, &iv(2).encode(), out.items[1].value);
    // Orphan deltas compact to nothing (counted).
    const before = stats.meta_kv_delta_orphans.load();
    var recs3 = [_]Record{try tTimes(5, 50, 55)};
    defer deinitRecords(gpa, &recs3);
    try compactFold(gpa, refs(&recs3, &rbuf), 0, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    try testing.expectEqual(before + 1, stats.meta_kv_delta_orphans.load());
    // Tombstone below the tail shadows the Put AND elides: the key vanishes.
    var recs4 = [_]Record{ try tDel(7), try tPut(3, iv(1)) };
    defer deinitRecords(gpa, &recs4);
    try compactFold(gpa, refs(&recs4, &rbuf), 8, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    // Empty group is a no-op.
    try compactFold(gpa, &.{}, 0, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
}

fn layoutPutBytes(gpa: Allocator, size: u64, map: []const LayoutEntry) ![]u8 {
    var l: Layout = .{ .file_type = "striped", .size = size, .block_prefix = "be://data", .block_map = .empty };
    defer l.deinit(gpa);
    for (map) |e| try l.insert(gpa, e.block, e.key);
    const value = try l.encodeAlloc(gpa);
    defer gpa.free(value);
    return (XattrValue{ .name = "layout", .value = value }).encodeAlloc(gpa);
}

fn layoutLinkBytes(gpa: Allocator, entries: []const LayoutEntry, base_version: u64, version: u64) ![]u8 {
    var d: LayoutDelta = .{ .file_type = "striped", .size = 4096, .block_prefix = "be://data", .entries = entries };
    d.setVersions(base_version, version);
    return d.encodeAlloc(gpa);
}

test "fold: a layout delta chain folds onto its xattr base canonically; the chain-link law and duplicate-seq skip" {
    const gpa = testing.allocator;
    const base_v = try layoutPutBytes(gpa, 4096, &.{.{ .block = 0, .key = "be://data:a" }});
    defer gpa.free(base_v);
    const d1_v = try layoutLinkBytes(gpa, &.{.{ .block = 1, .key = "be://data:b" }}, 0, 0x11);
    defer gpa.free(d1_v);
    const d2_v = try layoutLinkBytes(gpa, &.{.{ .block = 2, .key = "be://data:c" }}, 0x11, 0x12);
    defer gpa.free(d2_v);
    const base: RecordRef = .{ .key = &fold_key, .seq = 1, .kind = .put, .value = base_v };
    const d1: RecordRef = .{ .key = &fold_key, .seq = 2, .kind = .delta, .value = d1_v };
    const d2: RecordRef = .{ .key = &fold_key, .seq = 3, .kind = .delta, .value = d2_v };
    const folds_before = stats.meta_kv_layout_delta_folds.load();
    var f = try foldNewestFirst(gpa, &.{ d2, d1, base });
    defer f.deinit(gpa);
    try testing.expect(f.put.owned);
    try testing.expectEqual(@as(u64, 3), f.put.seq);
    try testing.expectEqual(folds_before + 2, stats.meta_kv_layout_delta_folds.load());
    const x = try XattrValue.decode(f.put.value);
    try testing.expectEqualStrings("layout", x.name);
    var l = try Layout.decodeBase(gpa, x.value);
    defer l.deinit(gpa);
    try testing.expectEqualStrings("be://data:a", l.get(0).?);
    try testing.expectEqualStrings("be://data:b", l.get(1).?);
    try testing.expectEqualStrings("be://data:c", l.get(2).?);
    // The folded value equals the canonical encode of the full map.
    const want = try layoutPutBytes(gpa, 4096, &.{ .{ .block = 0, .key = "be://data:a" }, .{ .block = 1, .key = "be://data:b" }, .{ .block = 2, .key = "be://data:c" } });
    defer gpa.free(want);
    try testing.expectEqualSlices(u8, want, f.put.value);
    // Divergent: d2 claims 0x11 but folds onto a base whose head is 0x10.
    const d1_other = try layoutLinkBytes(gpa, &.{.{ .block = 1, .key = "be://data:b" }}, 0, 0x10);
    defer gpa.free(d1_other);
    try testing.expectError(error.LayoutChainDivergent, foldNewestFirst(gpa, &.{ d2, .{ .key = &fold_key, .seq = 2, .kind = .delta, .value = d1_other }, base }));
    // A first link claiming a nonzero base above a bare Put diverges.
    try testing.expectError(error.LayoutChainDivergent, foldNewestFirst(gpa, &.{ d2, base }));
    // Heterogeneous: unversioned above versioned and versioned above unversioned.
    const unv = try layoutLinkBytes(gpa, &.{.{ .block = 3, .key = "be://data:d" }}, 0, 0);
    defer gpa.free(unv);
    try testing.expectError(error.LayoutChainHeterogeneous, foldNewestFirst(gpa, &.{ .{ .key = &fold_key, .seq = 3, .kind = .delta, .value = unv }, d1, base }));
    try testing.expectError(error.LayoutChainHeterogeneous, foldNewestFirst(gpa, &.{ d1, .{ .key = &fold_key, .seq = 1, .kind = .delta, .value = unv }, .{ .key = &fold_key, .seq = 0, .kind = .put, .value = base_v } }));
    // An exact-seq duplicate (the same link from two sources) skips the link law.
    var dup = try foldNewestFirst(gpa, &.{ d2, d2, d1, base });
    defer dup.deinit(gpa);
    try testing.expectEqualSlices(u8, want, dup.put.value);
    // Base refusals surface typed: an indirect base under a layout delta.
    const indirect_x = try (XattrValue{ .name = "layout", .value = &fixture_layout_e }).encodeAlloc(gpa);
    defer gpa.free(indirect_x);
    try testing.expectError(error.LayoutBaseIndirect, foldNewestFirst(gpa, &.{ d1, .{ .key = &fold_key, .seq = 1, .kind = .put, .value = indirect_x } }));
    // A tombstone shadows a layout chain.
    var t = try foldNewestFirst(gpa, &.{ d2, d1, .{ .key = &fold_key, .seq = 1, .kind = .delete, .value = &.{} }, base });
    defer t.deinit(gpa);
    try testing.expectEqual(Folded{ .tombstone = .{ .seq = 1 } }, t);
}

test "compact fold lineage rule: a versioned layout head compacts to Put + the link restamped to base 0; a later link still folds" {
    const gpa = testing.allocator;
    const base_v = try layoutPutBytes(gpa, 4096, &.{.{ .block = 0, .key = "be://data:a" }});
    defer gpa.free(base_v);
    const d1_v = try layoutLinkBytes(gpa, &.{.{ .block = 1, .key = "be://data:b" }}, 0, 0x11);
    defer gpa.free(d1_v);
    const d2_v = try layoutLinkBytes(gpa, &.{.{ .block = 2, .key = "be://data:c" }}, 0x11, 0x12);
    defer gpa.free(d2_v);
    const group = [_]RecordRef{
        .{ .key = &fold_key, .seq = 3, .kind = .delta, .value = d2_v },
        .{ .key = &fold_key, .seq = 2, .kind = .delta, .value = d1_v },
        .{ .key = &fold_key, .seq = 1, .kind = .put, .value = base_v },
    };
    var out: std.ArrayList(Record) = .empty;
    defer {
        deinitRecords(gpa, out.items);
        out.deinit(gpa);
    }
    try compactFold(gpa, &group, 0, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    try testing.expectEqual(RecordKind.put, out.items[0].kind);
    try testing.expectEqual(@as(u64, 2), out.items[0].seq);
    try testing.expectEqual(RecordKind.delta, out.items[1].kind);
    try testing.expectEqual(@as(u64, 3), out.items[1].seq);
    try testing.expectEqual(LayoutVersions{ .base_version = 0, .version = 0x12 }, layoutDeltaVersions(out.items[1].value).?);
    // A LIVE later link committed against the pre-compaction head (0x12).
    const d3_v = try layoutLinkBytes(gpa, &.{.{ .block = 3, .key = "be://data:d" }}, 0x12, 0x13);
    defer gpa.free(d3_v);
    var f = try foldNewestFirst(gpa, &.{ .{ .key = &fold_key, .seq = 4, .kind = .delta, .value = d3_v }, out.items[1].ref(), out.items[0].ref() });
    defer f.deinit(gpa);
    const x = try XattrValue.decode(f.put.value);
    var l = try Layout.decodeBase(gpa, x.value);
    defer l.deinit(gpa);
    for ([_]struct { u32, []const u8 }{ .{ 0, "be://data:a" }, .{ 1, "be://data:b" }, .{ 2, "be://data:c" }, .{ 3, "be://data:d" } }) |e| {
        try testing.expectEqualStrings(e[1], l.get(e[0]).?);
    }
    // Put + retained link fold to the same bytes as the whole chain at once;
    // the Put alone is the fold BELOW the link.
    var whole = try foldNewestFirst(gpa, &group);
    defer whole.deinit(gpa);
    var two = try foldNewestFirst(gpa, &.{ out.items[1].ref(), out.items[0].ref() });
    defer two.deinit(gpa);
    try testing.expectEqualSlices(u8, whole.put.value, two.put.value);
    var below = try foldNewestFirst(gpa, group[1..]);
    defer below.deinit(gpa);
    try testing.expectEqualSlices(u8, below.put.value, out.items[0].value);
    // A versioned head over a tombstone or over nothing keeps the plain algebra.
    var out2: std.ArrayList(Record) = .empty;
    defer {
        deinitRecords(gpa, out2.items);
        out2.deinit(gpa);
    }
    try compactFold(gpa, &.{ group[0], .{ .key = &fold_key, .seq = 2, .kind = .delete, .value = &.{} } }, 0, &out2);
    try testing.expectEqual(@as(usize, 1), out2.items.len);
    try testing.expectEqual(RecordKind.delete, out2.items[0].kind);
    const orphans_before = stats.meta_kv_delta_orphans.load();
    try compactFold(gpa, &.{group[0]}, 0, &out2);
    try testing.expectEqual(@as(usize, 1), out2.items.len);
    try testing.expectEqual(orphans_before + 1, stats.meta_kv_delta_orphans.load());
    // An unversioned chain stays a single record.
    const unv = try layoutLinkBytes(gpa, &.{.{ .block = 1, .key = "be://data:b" }}, 0, 0);
    defer gpa.free(unv);
    var out3: std.ArrayList(Record) = .empty;
    defer {
        deinitRecords(gpa, out3.items);
        out3.deinit(gpa);
    }
    try compactFold(gpa, &.{ .{ .key = &fold_key, .seq = 2, .kind = .delta, .value = unv }, group[2] }, 0, &out3);
    try testing.expectEqual(@as(usize, 1), out3.items.len);
    try testing.expectEqual(RecordKind.put, out3.items[0].kind);
}

/// One history step for the fold-forward equivalence property.
const Step = struct { kind: RecordKind, value: []const u8 };

fn stepsToRecords(gpa: Allocator, steps: []const Step, out: *std.ArrayList(Record)) !void {
    for (steps, 0..) |s, i| {
        try out.append(gpa, try (RecordRef{ .key = &fold_key, .seq = i + 1, .kind = s.kind, .value = s.value }).toRecord(gpa));
    }
}

/// Fold-forward oldest → newest, then compare with the from-scratch fold.
fn expectForwardEqualsScratch(gpa: Allocator, records: []const Record) !void {
    var head: FoldedHead = .absent;
    defer head.deinit(gpa);
    for (records) |*r| {
        var next = try foldForward(gpa, &head, r.kind, r.value);
        errdefer next.deinit(gpa);
        head.deinit(gpa);
        head = next;
    }
    var newest_first = try gpa.alloc(RecordRef, records.len);
    defer gpa.free(newest_first);
    for (records, 0..) |*r, i| newest_first[records.len - 1 - i] = r.ref();
    var scratch = try foldNewestFirst(gpa, newest_first);
    defer scratch.deinit(gpa);
    switch (head) {
        .live => |l| {
            try testing.expect(scratch == .put);
            try testing.expectEqualSlices(u8, scratch.put.value, l.value);
            if (l.decoded) |d| try testing.expectEqualSlices(u8, &d.encode(), l.value);
            try testing.expectEqual(l.materialized, head.ownedBytes() != 0);
        },
        .tombstone => try testing.expect(scratch == .tombstone),
        .absent => try testing.expect(scratch == .absent),
    }
}

test "property: fold_forward ≡ fold_newest_first over random inode histories (Put / Δtimes / Δctime / Delete)" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0xD7A);
    const r = prng.random();
    var bufs: [24][INODE_VALUE_LEN]u8 = undefined;
    for (0..512) |_| {
        const n = r.intRangeAtMost(usize, 1, 24);
        var steps: [24]Step = undefined;
        for (0..n) |i| {
            steps[i] = switch (r.intRangeLessThan(u8, 0, 4)) {
                0 => blk: {
                    bufs[i] = iv(r.int(u64)).encode();
                    break :blk .{ .kind = .put, .value = &bufs[i] };
                },
                1 => blk: {
                    const d = InodeDelta.times(r.int(u64), r.int(u64)).encode();
                    @memcpy(bufs[i][0..d.len], d.slice());
                    break :blk .{ .kind = .delta, .value = bufs[i][0..d.len] };
                },
                2 => blk: {
                    const d = InodeDelta.ctime(r.int(u64)).encode();
                    @memcpy(bufs[i][0..d.len], d.slice());
                    break :blk .{ .kind = .delta, .value = bufs[i][0..d.len] };
                },
                else => .{ .kind = .delete, .value = &.{} },
            };
        }
        var records: std.ArrayList(Record) = .empty;
        defer {
            deinitRecords(gpa, records.items);
            records.deinit(gpa);
        }
        try stepsToRecords(gpa, steps[0..n], &records);
        try expectForwardEqualsScratch(gpa, records.items);
    }
}

test "property: fold_forward ≡ fold_newest_first over random layout histories (layout Put / unversioned layout delta / Delete)" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0x1A70);
    const r = prng.random();
    var keybuf: [24][24]u8 = undefined;
    for (0..256) |_| {
        const n = r.intRangeAtMost(usize, 1, 24);
        var values: std.ArrayList([]u8) = .empty;
        defer {
            for (values.items) |v| gpa.free(v);
            values.deinit(gpa);
        }
        var steps: [24]Step = undefined;
        for (0..n) |i| {
            const kk = try std.fmt.bufPrint(&keybuf[i], "oss0://{d}", .{r.intRangeAtMost(u32, 0, 7) * 4096});
            steps[i] = switch (r.intRangeLessThan(u8, 0, 3)) {
                0 => blk: {
                    const nb = r.intRangeAtMost(u32, 0, 4);
                    var entries: [4]LayoutEntry = undefined;
                    for (0..nb) |b| entries[b] = .{ .block = @intCast(b), .key = kk };
                    const v = try layoutPutBytes(gpa, r.int(u64), entries[0..nb]);
                    try values.append(gpa, v);
                    break :blk .{ .kind = .put, .value = v };
                },
                1 => blk: {
                    const b = r.intRangeAtMost(u32, 0, 7);
                    const v = try layoutLinkBytes(gpa, &.{.{ .block = b, .key = kk }}, 0, 0);
                    try values.append(gpa, v);
                    break :blk .{ .kind = .delta, .value = v };
                },
                else => .{ .kind = .delete, .value = &.{} },
            };
        }
        var records: std.ArrayList(Record) = .empty;
        defer {
            deinitRecords(gpa, records.items);
            records.deinit(gpa);
        }
        try stepsToRecords(gpa, steps[0..n], &records);
        try expectForwardEqualsScratch(gpa, records.items);
    }
}

/// Post-fold user-visible state over the 6-key space: key → live value.
const LiveState = struct {
    values: [6]?[]u8 = @splat(null),

    fn deinit(self: *LiveState, gpa: Allocator) void {
        for (&self.values) |*v| if (v.*) |b| gpa.free(b);
        self.* = .{};
    }

    fn eql(a: *const LiveState, b: *const LiveState) bool {
        for (a.values, b.values) |x, y| {
            if ((x == null) != (y == null)) return false;
            if (x) |xb| if (!std.mem.eql(u8, xb, y.?)) return false;
        }
        return true;
    }
};

fn keyIndex(k: []const u8) usize {
    return @intCast(key.decodeInodeKey(k) catch unreachable);
}

fn seqDesc(_: void, a: RecordRef, b: RecordRef) bool {
    return a.seq > b.seq;
}

/// Group by key, newest-first (stable: equal seqs keep input order), fold.
fn groupNewestFirst(gpa: Allocator, records: []const Record, idx: usize, out: *std.ArrayList(RecordRef)) !void {
    out.clearRetainingCapacity();
    for (records) |*r| if (keyIndex(r.key) == idx) try out.append(gpa, r.ref());
    std.sort.block(RecordRef, out.items, {}, seqDesc);
}

fn modelLiveState(gpa: Allocator, records: []const Record) !LiveState {
    var state: LiveState = .{};
    errdefer state.deinit(gpa);
    var group: std.ArrayList(RecordRef) = .empty;
    defer group.deinit(gpa);
    for (1..7) |idx| {
        try groupNewestFirst(gpa, records, idx, &group);
        var f = try foldNewestFirst(gpa, group.items);
        defer f.deinit(gpa);
        if (f.liveValue()) |v| state.values[idx - 1] = try gpa.dupe(u8, v);
    }
    return state;
}

fn compactHistory(gpa: Allocator, records: []const Record, durable_tail: u64, out: *std.ArrayList(Record)) !void {
    var group: std.ArrayList(RecordRef) = .empty;
    defer group.deinit(gpa);
    for (1..7) |idx| {
        try groupNewestFirst(gpa, records, idx, &group);
        try compactFold(gpa, group.items, durable_tail, out);
    }
}

fn recordsEql(a: []const Record, b: []const Record) bool {
    if (a.len != b.len) return false;
    for (a, b) |x, y| if (!x.ref().eql(y.ref())) return false;
    return true;
}

test "property: replay reproduces RAM is ONE fold theorem — live(compact(history, tail) ∪ window) == live(history); compaction is a fixed point" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0x5EED);
    const r = prng.random();
    for (0..50) |_| {
        const n_seq: u64 = 120;
        var history: std.ArrayList(Record) = .empty;
        defer {
            deinitRecords(gpa, history.items);
            history.deinit(gpa);
        }
        for (1..n_seq + 1) |seq| {
            const k = key.inodeKey(r.intRangeAtMost(u64, 1, 6));
            const rec = switch (r.intRangeLessThan(u8, 0, 10)) {
                0...4 => try Record.put(gpa, &k, seq, &iv(seq).encode()),
                5...7 => try Record.delta(gpa, &k, seq, InodeDelta.times(seq * 10, seq * 10 + 1)),
                else => try Record.delete(gpa, &k, seq),
            };
            try history.append(gpa, rec);
        }
        var ram = try modelLiveState(gpa, history.items);
        defer ram.deinit(gpa);
        for ([_]u64{ 0, r.intRangeAtMost(u64, 1, n_seq), n_seq + 1 }) |tail| {
            var compacted: std.ArrayList(Record) = .empty;
            defer {
                deinitRecords(gpa, compacted.items);
                compacted.deinit(gpa);
            }
            try compactHistory(gpa, history.items, tail, &compacted);
            var replay_input: std.ArrayList(Record) = .empty;
            defer {
                deinitRecords(gpa, replay_input.items);
                replay_input.deinit(gpa);
            }
            for (compacted.items) |*c| try replay_input.append(gpa, try c.ref().toRecord(gpa));
            for (history.items) |*h| if (h.seq >= tail) try replay_input.append(gpa, try h.ref().toRecord(gpa));
            var replayed = try modelLiveState(gpa, replay_input.items);
            defer replayed.deinit(gpa);
            try testing.expect(ram.eql(&replayed));
            var twice: std.ArrayList(Record) = .empty;
            defer {
                deinitRecords(gpa, twice.items);
                twice.deinit(gpa);
            }
            try compactHistory(gpa, compacted.items, tail, &twice);
            try testing.expect(recordsEql(compacted.items, twice.items));
        }
    }
}

test "property: encode∘decode = id for every value codec under a deterministic PRNG" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0xC0DEC);
    const r = prng.random();
    var name_buf: [255]u8 = undefined;
    var val_buf: [600]u8 = undefined;
    var scratch: [1024]u8 = undefined;
    var scratch2: [1024]u8 = undefined;
    for (0..2000) |_| {
        // Inode value.
        const v: InodeValue = .{ .mode = r.int(u32), .uid = r.int(u32), .gid = r.int(u32), .nlink = r.int(u32), .flags = r.int(u32), .rdev = r.int(u64), .size = r.int(u64), .atime = r.int(u64), .mtime = r.int(u64), .ctime = r.int(u64), .flags2 = r.int(u32) };
        try testing.expectEqual(v, try InodeValue.decode(&v.encode()));
        // Inode delta with a random mask.
        const d: InodeDelta = .{ .mask = r.intRangeAtMost(u16, 0, DELTA_MASK_ALL), .fields = v };
        const de = d.encode();
        const dd = try InodeDelta.decode(de.slice());
        try testing.expectEqual(d.mask, dd.mask);
        var applied: InodeValue = .{};
        var applied2: InodeValue = .{};
        d.apply(&applied);
        dd.apply(&applied2);
        try testing.expectEqual(applied, applied2);
        // Dentry and xattr values.
        const nl = r.intRangeAtMost(usize, 0, 255);
        r.bytes(name_buf[0..nl]);
        const vl = r.intRangeAtMost(usize, 0, val_buf.len);
        r.bytes(val_buf[0..vl]);
        const dent: DentryValue = .{ .child_ino = r.int(u64), .file_type = r.int(u8), .name = name_buf[0..nl] };
        const db = try dent.encodeInto(&scratch);
        const dback = try DentryValue.decode(db);
        try testing.expectEqual(dent.child_ino, dback.child_ino);
        try testing.expectEqual(dent.file_type, dback.file_type);
        try testing.expectEqualSlices(u8, dent.name, dback.name);
        const x: XattrValue = .{ .name = name_buf[0..nl], .value = val_buf[0..vl] };
        const xb = try x.encodeInto(&scratch);
        const xback = try XattrValue.decode(xb);
        try testing.expectEqualSlices(u8, x.name, xback.name);
        try testing.expectEqualSlices(u8, x.value, xback.value);
        // Block-ref value.
        const brv: BlockRefValue = .{ .map_blob = r.boolean(), .shared = r.boolean() };
        try testing.expectEqual(brv, try BlockRefValue.decode(&brv.encode()));
        // Block-map entries of every kind.
        const run_len = r.intRangeAtMost(u32, RUN_LEN_MIN, RUN_LEN_MAX);
        const entries = [_]MapEntry{
            .{ .point = .{ .vol_tag = r.int(u64), .offset = r.int(u64) } },
            .{ .string = val_buf[0..vl] },
            .{ .run = .{ .vol_tag = r.int(u64), .start_offset = r.int(u64), .len = run_len } },
            .{ .point_stamped = .{ .vol_tag = r.int(u64), .offset = r.int(u64), .incarnation = r.intRangeAtMost(u64, 1, std.math.maxInt(u64)) } },
            .{ .run_stamped = .{ .vol_tag = r.int(u64), .start_offset = r.int(u64), .len = run_len, .start_incarnation = r.intRangeAtMost(u64, 1, std.math.maxInt(u64) - @as(u64, run_len)) } },
        };
        for (entries) |e| {
            const eb = e.encodeInto(&scratch);
            try testing.expectEqual(e.encodedLen(), eb.len);
            const back = try MapEntry.decode(eb);
            try testing.expectEqual(std.meta.activeTag(e), std.meta.activeTag(back));
            try testing.expectEqualSlices(u8, eb, back.encodeInto(&scratch2));
        }
        // Record frame.
        const rec: RecordRef = .{ .key = name_buf[0..@max(nl, 1)], .seq = r.int(u64), .kind = if (r.boolean()) .put else .delta, .value = val_buf[0..vl] };
        const rb = rec.encodeInto(&scratch);
        const rd = try RecordRef.decode(rb);
        try testing.expect(rd.rec.eql(rec));
        try testing.expectEqual(rb.len, rd.used);
        // Layout delta with random entries, versioned or not.
        var lentries: [3]LayoutEntry = undefined;
        const ne = r.intRangeAtMost(usize, 0, 3);
        for (0..ne) |i| lentries[i] = .{ .block = r.int(u32), .key = "k" };
        var ld: LayoutDelta = .{ .file_type = "striped", .size = r.int(u64), .block_map_id = if (r.boolean()) "id" else null, .block_prefix = if (r.boolean()) "" else null, .file_id = null, .data_key = if (r.boolean()) val_buf[0..@min(vl, 7)] else null, .entries = lentries[0..ne] };
        ld.setVersions(r.int(u64), if (r.boolean()) r.intRangeAtMost(u64, 1, std.math.maxInt(u64)) else 0);
        const lb = try ld.encodeAlloc(gpa);
        defer gpa.free(lb);
        var lback = try LayoutDelta.decode(gpa, lb);
        defer lback.deinit(gpa);
        try expectLayoutDeltaEql(ld, lback);
        // Layout base: canonical encode decodes to an equal re-encode.
        var layout: Layout = .{ .file_type = "t", .size = r.int(u64), .data_key = ld.data_key, .block_map = .empty };
        defer layout.deinit(gpa);
        for (lentries[0..ne]) |e| try layout.insert(gpa, e.block, e.key);
        const lyb = try layout.encodeAlloc(gpa);
        defer gpa.free(lyb);
        var lyback = try Layout.decodeBase(gpa, lyb);
        defer lyback.deinit(gpa);
        const lyb2 = try lyback.encodeAlloc(gpa);
        defer gpa.free(lyb2);
        try testing.expectEqualSlices(u8, lyb, lyb2);
    }
}

/// Every decoder returns or errors — never panics, never allocates from a
/// claim — and a successful record decode makes progress.
fn checkDecodersTotal(gpa: Allocator, bytes: []const u8) !void {
    _ = InodeValue.decode(bytes) catch {};
    _ = DentryValue.decode(bytes) catch {};
    _ = XattrValue.decode(bytes) catch {};
    _ = BlockRefValue.decode(bytes) catch {};
    _ = MapEntry.decode(bytes) catch {};
    _ = InodeDelta.decode(bytes) catch {};
    _ = isLayoutDelta(bytes);
    _ = layoutDeltaVersions(bytes);
    if (LayoutDelta.decode(gpa, bytes)) |*d| {
        var dd = d.*;
        dd.deinit(gpa);
    } else |_| {}
    if (Layout.decodeBase(gpa, bytes)) |*l| {
        var ll = l.*;
        ll.deinit(gpa);
    } else |_| {}
    if (RecordRef.decode(bytes)) |d| {
        try testing.expect(d.used >= RECORD_MIN_LEN and d.used <= bytes.len);
        try testing.expect(d.rec.key.len != 0);
    } else |_| {}
}

fn decodersTotalFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [1024]u8 = undefined;
    const len = smith.slice(&buf);
    try checkDecodersTotal(testing.allocator, buf[0..len]);
}

/// A `Smith.slice` input: `u32 LE len ‖ bytes`, built at comptime so the
/// corpus points at static memory.
fn fuzzSlice(comptime bytes: []const u8) []const u8 {
    const framed = comptime blk: {
        var len: [4]u8 = undefined;
        std.mem.writeInt(u32, &len, bytes.len, .little);
        break :blk len ++ bytes[0..bytes.len].*;
    };
    return &framed;
}

test "fuzz: every record value decoder is total on arbitrary bytes" {
    try fuzzkit.fuzz({}, decodersTotalFuzz, .{ .corpus = &.{
        fuzzSlice(&fixture_layout_a),
        fuzzSlice(&fixture_delta_g),
        fuzzSlice(&.{ 9, 0, 1, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 1 }),
        fuzzSlice(&.{ 1, 5, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0 }),
        fuzzSlice(&.{ 0xFF, 0x03, 0, 0 }),
    } });
}

test "property: every decoder is total under a deterministic PRNG, including mutations of valid encodings" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0x70741);
    const r = prng.random();
    var buf: [512]u8 = undefined;
    for (0..20_000) |_| {
        const len = r.intRangeAtMost(usize, 0, buf.len);
        r.bytes(buf[0..len]);
        try checkDecodersTotal(gpa, buf[0..len]);
    }
    // Mutations of valid encodings reach the structural gates.
    const valid = [_][]const u8{ &fixture_layout_a, &fixture_layout_b, &fixture_delta_f, &fixture_delta_g, &iv(3).encode(), &.{ 1, 3, 0, 0 } };
    for (0..5000) |_| {
        const src = valid[r.intRangeLessThan(usize, 0, valid.len)];
        @memcpy(buf[0..src.len], src);
        const flips = r.intRangeAtMost(usize, 1, 3);
        for (0..flips) |_| buf[r.intRangeLessThan(usize, 0, src.len)] ^= @as(u8, 1) << r.int(u3);
        const cut = r.intRangeAtMost(usize, 0, src.len);
        try checkDecodersTotal(gpa, buf[0..src.len]);
        try checkDecodersTotal(gpa, buf[0..cut]);
    }
}
