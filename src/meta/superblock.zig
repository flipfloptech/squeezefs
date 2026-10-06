//! Superblock v4 — sector 0 (charter §5.1, §1.7; `docs/porting/kv_alloc_superblock.md`
//! §1). Pure: images in, images out. The loader's primary/backup choice and
//! the write order are expressed over images and as an ordered plan the io
//! task executes.
//!
//! ONE `extern struct` with explicit little-endian fields and the checksum
//! LAST (format v4; decision 0002 rows 40–42): `{magic[8], version u16,
//! reserved u16, node_size u32, volume_len u64, features_incompat u64,
//! features_ro u64, root_ledger, journal, alloc_bitmap, heap, appender_dir
//! (5 × {start u64, len u64}), uuid[16], set_uuid[16], hash_seed u64,
//! sb_generation u64, reserved[16], xxh3 u64}` = 192 B; `[192, 4096)` is
//! zero padding under the checksum (the whole sector with the field zeroed).
//!
//! Forward-only (charter §1.7): the version IS every always-on mechanism of
//! the format; the four `features_incompat` bits are CAPABILITIES that can
//! legitimately differ per volume or flip later, one complete thing each.
//! An unknown incompat bit refuses loud naming the bit; unknown ro bits
//! classify fine and the mount is read-only. The version gate precedes the
//! checksum so a future format is reported as such, never as a tear.
//!
//! `volume_len` makes the DUR-5 backup copy SELF-LOCATED: it sits in the
//! last aligned sector of the volume the superblock itself names, so a
//! grown device never hides it (the loader checks the copy it read came
//! from the slot its own geometry names).

const std = @import("std");
const checksum = @import("../core/checksum.zig");
const alloc_core = @import("alloc_core.zig");
const alloc_ext = @import("alloc_ext.zig");

/// One O_DIRECT sector: the metadata grain and the superblock's durable unit.
pub const sector_len: usize = 4096;
/// Eight ASCII bytes, no version digits (the version field is the version).
pub const magic_value = "SQZMETLV";
/// The pre-v4 magic (the Rust reference's v2/v3 images): recognised only to
/// refuse with "reformat required" instead of "foreign volume".
pub const legacy_magic_value = "METALV01";
pub const version_v4: u16 = 4;

/// The fixed-field layout: the codec reads and writes exactly these bytes.
pub const SuperblockWire = extern struct {
    magic: [8]u8,
    version_le: [2]u8,
    /// Must be 0.
    reserved0_le: [2]u8,
    node_size_le: [4]u8,
    volume_len_le: [8]u8,
    features_incompat_le: [8]u8,
    features_ro_le: [8]u8,
    root_ledger: ExtentWire,
    journal: ExtentWire,
    alloc_bitmap: ExtentWire,
    heap: ExtentWire,
    appender_dir: ExtentWire,
    uuid: [16]u8,
    set_uuid: [16]u8,
    hash_seed_le: [8]u8,
    sb_generation_le: [8]u8,
    /// Must be all 0.
    reserved: [16]u8,
    checksum_le: [8]u8,
};

pub const ExtentWire = extern struct {
    start_le: [8]u8,
    len_le: [8]u8,
};

pub const fixed_len: usize = @sizeOf(SuperblockWire);
pub const off_version: usize = @offsetOf(SuperblockWire, "version_le");
pub const off_sb_generation: usize = @offsetOf(SuperblockWire, "sb_generation_le");
pub const off_checksum: usize = @offsetOf(SuperblockWire, "checksum_le");

comptime {
    std.debug.assert(fixed_len == 192);
    std.debug.assert(off_version == 8);
    std.debug.assert(@offsetOf(SuperblockWire, "node_size_le") == 12);
    std.debug.assert(@offsetOf(SuperblockWire, "volume_len_le") == 16);
    std.debug.assert(@offsetOf(SuperblockWire, "features_incompat_le") == 24);
    std.debug.assert(@offsetOf(SuperblockWire, "root_ledger") == 40);
    std.debug.assert(@offsetOf(SuperblockWire, "appender_dir") == 104);
    std.debug.assert(@offsetOf(SuperblockWire, "uuid") == 120);
    std.debug.assert(@offsetOf(SuperblockWire, "set_uuid") == 136);
    std.debug.assert(@offsetOf(SuperblockWire, "hash_seed_le") == 152);
    std.debug.assert(off_sb_generation == 160);
    std.debug.assert(off_checksum == 184);
    std.debug.assert(off_checksum + 8 == fixed_len);
}

/// Node sizes: 4 KiB multiples in [64 KiB, 1 MiB] (the format knob's floor
/// and ceiling; default 256 KiB = bcachefs's shipped `btree_node_size`).
pub const min_node_size: u64 = 65_536;
pub const max_node_size: u64 = 1_048_576;
pub const default_node_size: u64 = 262_144;
pub const node_page: u64 = 4096;

pub fn nodeSizeValid(node_size: u64) bool {
    return node_size >= min_node_size and node_size <= max_node_size and node_size % node_page == 0;
}

/// 32 round-robin 4 KiB slots at offset 4096.
pub const root_ledger_len: u64 = 32 * 4096;
pub const journal_page_len: u64 = 4096;

/// One on-disk extent `[start, start + len)`; encoded `start u64 LE ‖ len u64 LE`.
pub const ExtentRef = struct {
    start: u64,
    len: u64,

    pub const none: ExtentRef = .{ .start = 0, .len = 0 };

    pub fn end(self: ExtentRef) u64 {
        return self.start + self.len;
    }

    pub fn isNone(self: ExtentRef) bool {
        return self.start == 0 and self.len == 0;
    }
};

// ---------------------------------------------------------------------------
// features_incompat: four CAPABILITY bits, one complete thing each. Every
// always-on mechanism of the format (the v3 node layer, the node-seq
// watermark, the membership stamp, the derived routing width, durable
// terms, block refcounts, writer-scoped staging, ino lanes, block-key
// incarnations, the claim set, layout versions, the forest layout) IS the
// version.
// ---------------------------------------------------------------------------

pub const Presence = enum {
    /// Set by the builder of a format class that includes the mechanism;
    /// never a mount gate on its own.
    format_time,
    /// Stamped bit-before-record by the backend; never at format.
    stamped_later,
};

pub const IncompatBit = struct { bit: u6, name: []const u8, presence: Presence };

pub const incompat_bits = [_]IncompatBit{
    .{ .bit = 0, .name = "SYMMETRIC_PLANE", .presence = .format_time },
    .{ .bit = 1, .name = "LAYOUT_DELTAS", .presence = .stamped_later },
    .{ .bit = 2, .name = "BLOCK_MAP_TREE", .presence = .stamped_later },
    .{ .bit = 3, .name = "VOLUME_LIFECYCLE", .presence = .stamped_later },
};

/// The appender region + manager plane (charter §5.5): the appender
/// directory and appender-0 page exist iff this bit is set. Absent on the
/// one-member forest `--single-writer` builds (rung 6 lands the plane).
pub const symmetric_plane: u64 = 1 << 0;
/// The volume may carry layout `Delta` records; stamped before the first.
pub const layout_deltas: u64 = 1 << 1;
/// The kind-7 block-map tree for PB-class files; stamped at first crossing.
pub const block_map_tree: u64 = 1 << 2;
/// The set has lifecycle history; stamped at the first lifecycle commit.
pub const volume_lifecycle: u64 = 1 << 3;

/// Bits 0..=3; anything above refuses.
pub const features_incompat_known: u64 = (1 << 4) - 1;
/// No ro bits yet: any set ro bit is unknown ⇒ the mount is read-only.
pub const features_ro_known: u64 = 0;

// ---------------------------------------------------------------------------
// The struct and its codec.
// ---------------------------------------------------------------------------

pub const Superblock = struct {
    node_size: u32,
    /// The device length at format: the DUR-5 copy's self-location.
    volume_len: u64,
    features_incompat: u64,
    features_ro: u64,
    root_ledger: ExtentRef,
    journal: ExtentRef,
    alloc_bitmap: ExtentRef,
    heap: ExtentRef,
    /// Non-zero iff `SYMMETRIC_PLANE`.
    appender_dir: ExtentRef,
    uuid: [16]u8,
    set_uuid: [16]u8,
    /// An INDEPENDENT random seed minted at set creation (never derived from
    /// an identifier anyone can read off a URI): the same on every member.
    hash_seed: u64,

    pub fn totalExtents(self: Superblock) u64 {
        return self.heap.len / self.node_size;
    }

    pub fn journalPages(self: Superblock) u64 {
        return self.journal.len / journal_page_len;
    }

    pub fn unknownIncompat(self: Superblock) u64 {
        return self.features_incompat & ~features_incompat_known;
    }

    pub fn unknownRo(self: Superblock) u64 {
        return self.features_ro & ~features_ro_known;
    }

    pub fn blockMapTreeStamped(self: Superblock) bool {
        return self.features_incompat & block_map_tree != 0;
    }

    pub fn symmetricPlaneStamped(self: Superblock) bool {
        return self.features_incompat & symmetric_plane != 0;
    }

    /// The DUR-5 copy's offset: the last aligned sector of THIS volume.
    pub fn backupOffset(self: Superblock) u64 {
        // The validator guaranteed the slot exists and lies past the heap.
        return backupOffsetFor(self.volume_len).?;
    }

    /// Validates the geometry, then lays every field and stamps the
    /// checksum over the whole sector.
    pub fn encodeSector(self: Superblock, generation: u64, out: *[sector_len]u8, diag: ?*Refusal) DecodeError!void {
        try self.validateGeometry(diag);
        @memset(out, 0);
        var w: SuperblockWire = undefined;
        w.magic = magic_value.*;
        std.mem.writeInt(u16, &w.version_le, version_v4, .little);
        std.mem.writeInt(u16, &w.reserved0_le, 0, .little);
        std.mem.writeInt(u32, &w.node_size_le, self.node_size, .little);
        std.mem.writeInt(u64, &w.volume_len_le, self.volume_len, .little);
        std.mem.writeInt(u64, &w.features_incompat_le, self.features_incompat, .little);
        std.mem.writeInt(u64, &w.features_ro_le, self.features_ro, .little);
        w.root_ledger = extentWire(self.root_ledger);
        w.journal = extentWire(self.journal);
        w.alloc_bitmap = extentWire(self.alloc_bitmap);
        w.heap = extentWire(self.heap);
        w.appender_dir = extentWire(self.appender_dir);
        w.uuid = self.uuid;
        w.set_uuid = self.set_uuid;
        std.mem.writeInt(u64, &w.hash_seed_le, self.hash_seed, .little);
        std.mem.writeInt(u64, &w.sb_generation_le, generation, .little);
        w.reserved = @splat(0);
        w.checksum_le = @splat(0);
        out[0..fixed_len].* = std.mem.toBytes(w);
        checksum.stamp(out, off_checksum);
    }

    pub fn decodeSector(buf: []const u8, diag: ?*Refusal) DecodeError!Superblock {
        return decodeSectorWithKnown(buf, features_incompat_known, diag);
    }

    /// The decoder in its exact check order: length → magic → version →
    /// checksum → fields → reserved bytes → unknown bits → appender dir
    /// without `SYMMETRIC_PLANE` → geometry. `known` models the deciding
    /// binary (tests pass a narrower mask to prove the refusal an OLDER
    /// binary would issue for a later capability).
    pub fn decodeSectorWithKnown(buf: []const u8, known: u64, diag: ?*Refusal) DecodeError!Superblock {
        if (buf.len != sector_len) return refuse(diag, .{ .bad_length = buf.len });
        if (!std.mem.eql(u8, buf[0..8], magic_value)) return refuse(diag, .bad_magic);
        const version = std.mem.readInt(u16, buf[off_version..][0..2], .little);
        if (version != version_v4) return refuse(diag, .{ .wrong_version = version });
        const stored = std.mem.readInt(u64, buf[off_checksum..][0..8], .little);
        const computed = checksum.xxh3FieldZeroed(buf, off_checksum);
        if (stored != computed) return refuse(diag, .{ .checksum_mismatch = .{ .stored = stored, .computed = computed } });
        const w = std.mem.bytesToValue(SuperblockWire, buf[0..fixed_len]);
        const sb = Superblock{
            .node_size = std.mem.readInt(u32, &w.node_size_le, .little),
            .volume_len = std.mem.readInt(u64, &w.volume_len_le, .little),
            .features_incompat = std.mem.readInt(u64, &w.features_incompat_le, .little),
            .features_ro = std.mem.readInt(u64, &w.features_ro_le, .little),
            .root_ledger = extentRef(w.root_ledger),
            .journal = extentRef(w.journal),
            .alloc_bitmap = extentRef(w.alloc_bitmap),
            .heap = extentRef(w.heap),
            .appender_dir = extentRef(w.appender_dir),
            .uuid = w.uuid,
            .set_uuid = w.set_uuid,
            .hash_seed = std.mem.readInt(u64, &w.hash_seed_le, .little),
        };
        if (std.mem.readInt(u16, &w.reserved0_le, .little) != 0 or !std.mem.allEqual(u8, &w.reserved, 0) or !std.mem.allEqual(u8, buf[fixed_len..], 0)) {
            return refuse(diag, .reserved_not_zero);
        }
        const unknown = sb.features_incompat & ~known;
        if (unknown != 0) return refuse(diag, .{ .unknown_incompat_bits = unknown });
        if (!sb.appender_dir.isNone() and !sb.symmetricPlaneStamped()) return refuse(diag, .appender_dir_without_plane);
        try sb.validateGeometry(diag);
        return sb;
    }

    /// Every geometry field validated before any caller dereferences it:
    /// the ledger/journal/bitmap triple ascends below a heap of whole
    /// `node_size` extents on a `node_size` boundary (the extent index is
    /// `addr / node_size`, so `addr % node_size == 0` is a free misdirect
    /// check everywhere), and the DUR-5 slot the volume length names lies
    /// past the heap.
    pub fn validateGeometry(self: Superblock, diag: ?*Refusal) DecodeError!void {
        if (!nodeSizeValid(self.node_size)) return refuse(diag, .{ .invalid_node_size = self.node_size });
        const node_size: u64 = self.node_size;
        if (self.root_ledger.start < sector_len) return refuse(diag, .{ .ledger_overlaps_sector0 = self.root_ledger.start });
        if (self.root_ledger.len != root_ledger_len) return refuse(diag, .{ .ledger_length = self.root_ledger.len });
        if (self.journal.start < self.root_ledger.end() or self.journal.len == 0 or self.journal.len % journal_page_len != 0) {
            return refuse(diag, .{ .journal_placement = self.journal });
        }
        if (self.alloc_bitmap.start < self.journal.end()) return refuse(diag, .{ .bitmap_overlaps_journal = self.alloc_bitmap.start });
        if (self.heap.start % node_size != 0 or self.heap.len < node_size or self.heap.len % node_size != 0) {
            return refuse(diag, .{ .heap_shape = .{ .heap = self.heap, .node_size = node_size } });
        }
        if (self.alloc_bitmap.end() > self.heap.start) {
            return refuse(diag, .{ .triple_placement = .{ .triple_end = self.alloc_bitmap.end(), .heap_start = self.heap.start } });
        }
        const total = self.totalExtents();
        if (self.alloc_bitmap.len < alloc_ext.bitmapRegionLen(total)) {
            return refuse(diag, .{ .bitmap_too_short = .{ .len = self.alloc_bitmap.len, .total = total } });
        }
        // The same floor `plan` enforces: a crafted (still checksummed)
        // superblock must not hand the allocator a reserve ≥ total.
        const min_extents = alloc_core.compactionReserveExtents(total) + 4;
        if (total < min_extents) return refuse(diag, .{ .heap_below_reserve = .{ .total = total, .min = min_extents } });
        const backup = backupOffsetFor(self.volume_len) orelse return refuse(diag, .{ .backup_slot = .{ .volume_len = self.volume_len, .heap_end = self.heap.end() } });
        if (backup < self.heap.end()) return refuse(diag, .{ .backup_slot = .{ .volume_len = self.volume_len, .heap_end = self.heap.end() } });
        if (self.appender_dir.len != 0 and (self.appender_dir.len != node_size or self.appender_dir.start < self.heap.start or self.appender_dir.end() > self.heap.end() or (self.appender_dir.start - self.heap.start) % node_size != 0)) {
            return refuse(diag, .{ .appender_dir_shape = .{ .dir = self.appender_dir, .node_size = node_size } });
        }
    }

    /// The stamper's pure half: returns false when already set (no write).
    pub fn stampBit(self: *Superblock, bit: u64) bool {
        if (self.features_incompat & bit != 0) return false;
        self.features_incompat |= bit;
        return true;
    }

    /// `SYMMETRIC_PLANE` + its directory in ONE act; a different directory
    /// already stamped is a refusal.
    pub fn setSymmetricPlane(self: *Superblock, dir: ExtentRef, diag: ?*Refusal) error{SymmetricPlaneRepoint}!bool {
        if (self.symmetricPlaneStamped()) {
            if (self.appender_dir.start == dir.start and self.appender_dir.len == dir.len) return false;
            if (diag) |d| d.* = .{ .symmetric_plane_repoint = .{ .have = self.appender_dir, .want = dir } };
            return error.SymmetricPlaneRepoint;
        }
        self.features_incompat |= symmetric_plane;
        self.appender_dir = dir;
        return true;
    }
};

fn extentWire(ext: ExtentRef) ExtentWire {
    var w: ExtentWire = undefined;
    std.mem.writeInt(u64, &w.start_le, ext.start, .little);
    std.mem.writeInt(u64, &w.len_le, ext.len, .little);
    return w;
}

fn extentRef(w: ExtentWire) ExtentRef {
    return .{ .start = std.mem.readInt(u64, &w.start_le, .little), .len = std.mem.readInt(u64, &w.len_le, .little) };
}

// ---------------------------------------------------------------------------
// Refusals: typed, and every string in ONE place.
// ---------------------------------------------------------------------------

pub const DecodeError = error{
    BadLength,
    BadMagic,
    WrongVersion,
    ChecksumMismatch,
    ReservedNotZero,
    UnknownIncompatBits,
    AppenderDirWithoutPlane,
    InvalidNodeSize,
    LedgerOverlapsSector0,
    LedgerLength,
    JournalPlacement,
    BitmapOverlapsJournal,
    HeapShape,
    TriplePlacement,
    BitmapTooShort,
    HeapBelowReserve,
    BackupSlot,
    AppenderDirShape,
    Sector0BadLength,
    InvalidMagic,
    UnsupportedVersion,
};

pub const Refusal = union(enum) {
    bad_length: usize,
    bad_magic,
    wrong_version: u16,
    checksum_mismatch: struct { stored: u64, computed: u64 },
    reserved_not_zero,
    unknown_incompat_bits: u64,
    appender_dir_without_plane,
    invalid_node_size: u32,
    ledger_overlaps_sector0: u64,
    ledger_length: u64,
    journal_placement: ExtentRef,
    bitmap_overlaps_journal: u64,
    heap_shape: struct { heap: ExtentRef, node_size: u64 },
    triple_placement: struct { triple_end: u64, heap_start: u64 },
    bitmap_too_short: struct { len: u64, total: u64 },
    heap_below_reserve: struct { total: u64, min: u64 },
    backup_slot: struct { volume_len: u64, heap_end: u64 },
    appender_dir_shape: struct { dir: ExtentRef, node_size: u64 },
    sector0_bad_length: usize,
    invalid_magic: [8]u8,
    unsupported_version: u16,
    symmetric_plane_repoint: struct { have: ExtentRef, want: ExtentRef },

    pub fn err(self: Refusal) DecodeError {
        return switch (self) {
            .bad_length => error.BadLength,
            .bad_magic => error.BadMagic,
            .wrong_version => error.WrongVersion,
            .checksum_mismatch => error.ChecksumMismatch,
            .reserved_not_zero => error.ReservedNotZero,
            .unknown_incompat_bits => error.UnknownIncompatBits,
            .appender_dir_without_plane => error.AppenderDirWithoutPlane,
            .invalid_node_size => error.InvalidNodeSize,
            .ledger_overlaps_sector0 => error.LedgerOverlapsSector0,
            .ledger_length => error.LedgerLength,
            .journal_placement => error.JournalPlacement,
            .bitmap_overlaps_journal => error.BitmapOverlapsJournal,
            .heap_shape => error.HeapShape,
            .triple_placement => error.TriplePlacement,
            .bitmap_too_short => error.BitmapTooShort,
            .heap_below_reserve => error.HeapBelowReserve,
            .backup_slot => error.BackupSlot,
            .appender_dir_shape => error.AppenderDirShape,
            .sector0_bad_length => error.Sector0BadLength,
            .invalid_magic => error.InvalidMagic,
            .unsupported_version => error.UnsupportedVersion,
            .symmetric_plane_repoint => unreachable,
        };
    }

    /// The refusal text (geometry ones carry the `superblock geometry: `
    /// prefix; the node-size one is the node layer's own message passed
    /// through).
    pub fn write(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .bad_length => |n| try w.print("superblock sector must be {d} bytes, got {d}", .{ sector_len, n }),
            .bad_magic => try w.writeAll("bad superblock magic (corrupted or foreign volume)"),
            .wrong_version => |v| try w.print("v4 decoder handed superblock version {d} (classification bug)", .{v}),
            .checksum_mismatch => |c| try w.print("superblock checksum mismatch: stored 0x{x:0>16}, computed 0x{x:0>16} — corrupted superblock", .{ c.stored, c.computed }),
            .reserved_not_zero => try w.writeAll("superblock reserved bytes are not zero (a writer this binary does not know, or corruption under a valid checksum)"),
            .unknown_incompat_bits => |unknown| {
                try w.writeAll("unknown incompatible feature bits on the superblock: ");
                var first = true;
                var b: u7 = 0;
                while (b < 64) : (b += 1) {
                    if (unknown & (@as(u64, 1) << @intCast(b)) == 0) continue;
                    if (!first) try w.writeAll(", ");
                    first = false;
                    try w.print("bit {d}", .{b});
                }
                try w.print(" (0x{x}) — upgrade squeezefs to mount this volume", .{unknown});
            },
            .appender_dir_without_plane => try w.writeAll("superblock names an appender directory but carries no SYMMETRIC_PLANE capability bit — the directory is part of that bit's meaning (charter §5.5)"),
            .invalid_node_size => |n| try w.print("invalid node_size {d}: must be a 4 KiB multiple in [65536, 1048576]", .{n}),
            .ledger_overlaps_sector0 => |s| try w.print("superblock geometry: root ledger at {d} overlaps sector 0", .{s}),
            .ledger_length => |l| try w.print("superblock geometry: root ledger length {d} != the fixed {d}", .{ l, root_ledger_len }),
            .journal_placement => |j| try w.print("superblock geometry: journal [{d}, +{d}) must follow the ledger in whole 4 KiB pages", .{ j.start, j.len }),
            .bitmap_overlaps_journal => |s| try w.print("superblock geometry: alloc bitmap at {d} overlaps the journal", .{s}),
            .heap_shape => |h| try w.print("superblock geometry: heap [{d}, +{d}) must be whole {d}-byte extents on a {d}-byte boundary", .{ h.heap.start, h.heap.len, h.node_size, h.node_size }),
            .triple_placement => |t| try w.print("superblock geometry: fixed structures (ledger/journal/bitmap ending at {d}) must sit entirely below heap.start {d}", .{ t.triple_end, t.heap_start }),
            .bitmap_too_short => |b| try w.print("superblock geometry: alloc bitmap length {d} cannot cover {d} extents", .{ b.len, b.total }),
            .heap_below_reserve => |h| try w.print("superblock geometry: heap of {d} extents cannot hold the compaction reserve (≥ {d} required)", .{ h.total, h.min }),
            .backup_slot => |b| try w.print("superblock geometry: volume length {d} leaves no redundant-superblock sector past the heap end {d}", .{ b.volume_len, b.heap_end }),
            .appender_dir_shape => |a| try w.print("superblock geometry: appender directory [{d}, {d}) is not one heap extent of {d} bytes", .{ a.dir.start, a.dir.end(), a.node_size }),
            .sector0_bad_length => |n| try w.print("sector-0 image must be {d} bytes, got {d}", .{ sector_len, n }),
            .invalid_magic => |m| {
                try w.writeAll("invalid superblock magic ");
                try writeByteList(w, &m);
                try w.writeAll(" (expected ");
                try writeByteList(w, magic_value);
                try w.writeAll(") — corrupted or foreign volume");
            },
            .unsupported_version => |v| try w.print("unsupported metadata format version {d} (this binary supports exactly {d}) — upgrade squeezefs", .{ v, version_v4 }),
            .symmetric_plane_repoint => |r| try w.print("SYMMETRIC_PLANE is already stamped with appender directory 0x{x}+{d} — refusing to re-point it at 0x{x}+{d}", .{ r.have.start, r.have.len, r.want.start, r.want.len }),
        }
    }
};

/// `[77, 69, 84, …]`: the raw bytes of a magic, for a refusal line.
fn writeByteList(w: *std.Io.Writer, bytes: []const u8) std.Io.Writer.Error!void {
    try w.writeByte('[');
    for (bytes, 0..) |b, i| {
        if (i != 0) try w.writeAll(", ");
        try w.print("{d}", .{b});
    }
    try w.writeByte(']');
}

fn refuse(diag: ?*Refusal, r: Refusal) DecodeError {
    if (diag) |d| d.* = r;
    return r.err();
}

// ---------------------------------------------------------------------------
// classify_sector0: the version gate precedes the checksum.
// ---------------------------------------------------------------------------

pub const VolumeFormat = union(enum) {
    /// All-zero magic: never formatted (mount: loud; format: proceed).
    blank,
    /// The pre-v4 magic (the reference's v2/v3 images): recognised, never
    /// decoded — reformat required.
    legacy: u32,
    v4: Superblock,

    /// The mount's refusal for the two non-v4 classifications.
    pub fn writeMountRefusal(self: VolumeFormat, w: *std.Io.Writer, path: []const u8) std.Io.Writer.Error!void {
        switch (self) {
            .blank => try w.print("{s} is not formatted (zeroed superblock) — run `squeezefs format` first", .{path}),
            .legacy => |v| try w.print("Metadata volume {s} is a pre-v4 squeezefs format (version {d}), which this binary does not mount — reformat required: run `squeezefs format` (destroys the old contents)", .{ path, v }),
            .v4 => {},
        }
    }
};

pub fn classifySector0(sector: []const u8, diag: ?*Refusal) DecodeError!VolumeFormat {
    if (sector.len != sector_len) return refuse(diag, .{ .sector0_bad_length = sector.len });
    const magic = sector[0..8];
    if (std.mem.allEqual(u8, magic, 0)) return .blank;
    if (std.mem.eql(u8, magic, legacy_magic_value)) return .{ .legacy = std.mem.readInt(u32, sector[8..12], .little) };
    if (!std.mem.eql(u8, magic, magic_value)) return refuse(diag, .{ .invalid_magic = magic.* });
    const version = std.mem.readInt(u16, sector[off_version..][0..2], .little);
    if (version == version_v4) return .{ .v4 = try Superblock.decodeSector(sector, diag) };
    return refuse(diag, .{ .unsupported_version = version });
}

/// The DUR-5 generation stamped in an image (0 on anything shorter).
/// Meaningful only for images that classify.
pub fn sectorGeneration(sector: []const u8) u64 {
    if (sector.len < off_sb_generation + 8) return 0;
    return std.mem.readInt(u64, sector[off_sb_generation..][0..8], .little);
}

// ---------------------------------------------------------------------------
// DUR-5: the redundant copy at the LAST aligned sector.
// ---------------------------------------------------------------------------

/// Why the tail and not a second sector beside sector 0: distance from
/// sector 0 is what protects the copy against partition-table and `mkfs`
/// collateral, and the tail needs no layout change (`plan` holds it back
/// out of the heap, one extent at most). `null` below two sectors (it must
/// never alias sector 0).
pub fn backupOffsetFor(volume_len: u64) ?u64 {
    const aligned = (volume_len / sector_len) * sector_len;
    if (aligned < 2 * sector_len) return null;
    return aligned - sector_len;
}

/// `max(gen of VALID primary, gen of VALID backup) + 1` — "valid" is
/// `classifySector0` succeeding (Blank and legacy included, at 0).
pub fn nextSuperblockGeneration(primary: ?[]const u8, backup: ?[]const u8) u64 {
    var newest: u64 = 0;
    for ([_]?[]const u8{ primary, backup }) |img| {
        const sector = img orelse continue;
        _ = classifySector0(sector, null) catch continue;
        newest = @max(newest, sectorGeneration(sector));
    }
    return newest + 1;
}

pub const SuperblockSlot = enum { primary, backup };

pub const Classified = struct { format: VolumeFormat, slot: SuperblockSlot };

/// The loader's choice: a primary that classifies — INCLUDING Blank and
/// legacy — is authoritative and the backup is never consulted (a stale or
/// crafted tail can never displace a readable primary). Only when the
/// primary ERRORS is the tail read (from `backupOffsetFor(device length)`);
/// accepted iff it classifies v4 AND its own `volume_len` names the slot it
/// was read from; otherwise the PRIMARY's error stands.
pub fn classifyVolumeImages(primary: []const u8, backup: ?[]const u8, device_len: u64, diag: ?*Refusal) DecodeError!Classified {
    var primary_refusal: Refusal = undefined;
    const primary_err = blk: {
        const fmt = classifySector0(primary, &primary_refusal) catch |e| break :blk e;
        return .{ .format = fmt, .slot = .primary };
    };
    if (backup) |tail| {
        if (backupOffsetFor(device_len)) |off| {
            if (classifySector0(tail, null)) |fmt| {
                if (fmt == .v4 and fmt.v4.backupOffset() == off) return .{ .format = fmt, .slot = .backup };
            } else |_| {}
        }
    }
    if (diag) |d| d.* = primary_refusal;
    return primary_err;
}

/// The recovery log line (error level) when the backup was selected.
pub fn writeBackupMountNotice(w: *std.Io.Writer, path: []const u8, primary_refusal: Refusal, off: u64, generation: u64) std.Io.Writer.Error!void {
    try w.print("{s}: sector 0 is unreadable (", .{path});
    try primary_refusal.write(w);
    try w.print(") — mounting from the redundant superblock copy at offset {d} (generation {d}). Sector 0 is repaired on the next write mount; investigate the device.", .{ off, generation });
}

// ---------------------------------------------------------------------------
// The write-order contract as a plan: backup → fsync → sector 0 → fsync.
// ---------------------------------------------------------------------------

pub const WriteStep = struct {
    offset: u64,
    /// `fdatasync` after this write: a tear on sector 0 recovers FORWARD from
    /// the already-durable copy; a tear on the copy leaves the primary intact.
    barrier_after: bool,
};

pub const WritePlan = struct {
    /// Both copies carry this one image (same generation).
    image: [sector_len]u8,
    generation: u64,
    steps: [2]WriteStep,

    pub fn slice(self: *const WritePlan) []const WriteStep {
        return &self.steps;
    }

    pub fn bytes(self: *const WritePlan) []const u8 {
        return &self.image;
    }
};

/// `write_superblock_at_generation`: the backup copy first (the geometry
/// always reserves it), barrier, then sector 0, barrier.
pub fn planWrite(sb: Superblock, generation: u64, diag: ?*Refusal) DecodeError!WritePlan {
    var plan: WritePlan = .{ .image = undefined, .generation = generation, .steps = undefined };
    try sb.encodeSector(generation, &plan.image, diag);
    plan.steps[0] = .{ .offset = sb.backupOffset(), .barrier_after = true };
    plan.steps[1] = .{ .offset = 0, .barrier_after = true };
    return plan;
}

pub const RepairPlan = struct {
    image: [sector_len]u8,
    step: WriteStep,
};

/// `repair_primary_superblock` (write mounts that selected the backup):
/// sector 0 alone at the next generation, barrier.
pub fn planRepairPrimary(sb: Superblock, generation: u64, diag: ?*Refusal) DecodeError!RepairPlan {
    var plan: RepairPlan = .{ .image = undefined, .step = .{ .offset = 0, .barrier_after = true } };
    try sb.encodeSector(generation, &plan.image, diag);
    return plan;
}

pub fn writeRepairNotice(w: *std.Io.Writer, path: []const u8, generation: u64) std.Io.Writer.Error!void {
    try w.print("{s}: repaired a damaged sector 0 from the redundant superblock copy (generation {d})", .{ path, generation });
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// The 64 MiB / 64 KiB / 1 MiB-ring fixture geometry (spec §2.1 example 1)
/// with the SYMMETRIC_PLANE bit and its directory.
pub fn testFixture() Superblock {
    return .{
        .node_size = 65536,
        .volume_len = 64 << 20,
        .features_incompat = symmetric_plane,
        .features_ro = 0,
        .root_ledger = .{ .start = 4096, .len = 131072 },
        .journal = .{ .start = 135168, .len = 1048576 },
        .alloc_bitmap = .{ .start = 1183744, .len = 8192 },
        .heap = .{ .start = 1245184, .len = 1004 * 65536 },
        .appender_dir = .{ .start = 1245184 + 2 * 65536, .len = 65536 },
        .uuid = "kv-backend-test!".*,
        .set_uuid = "set-uuid-fixture".*,
        .hash_seed = 0x5EED_CAFE_F00D_D00D,
    };
}

fn msg(buf: []u8, r: Refusal) []const u8 {
    var w: std.Io.Writer = .fixed(buf);
    r.write(&w) catch unreachable;
    return w.buffered();
}

test "incompat table: four capability bits, pairwise disjoint, union == KNOWN, one format-time (the plane) and three stamped-later; ro known is 0" {
    var acc: u64 = 0;
    for (incompat_bits, 0..) |b, i| {
        try testing.expectEqual(@as(u6, @intCast(i)), b.bit);
        const mask = @as(u64, 1) << b.bit;
        try testing.expect(std.math.isPowerOfTwo(mask));
        try testing.expectEqual(@as(u64, 0), acc & mask);
        acc |= mask;
    }
    try testing.expectEqual(features_incompat_known, acc);
    try testing.expectEqual(@as(u64, 0xF), features_incompat_known);
    try testing.expectEqual(@as(u64, 0), features_ro_known);
    try testing.expectEqual(Presence.format_time, incompat_bits[0].presence);
    try testing.expectEqualStrings("SYMMETRIC_PLANE", incompat_bits[0].name);
    for (incompat_bits[1..]) |b| try testing.expectEqual(Presence.stamped_later, b.presence);
    try testing.expectEqual(@as(u16, 4), version_v4);
}

test "sector image: every field at its offset in the 192 B extern struct, checksum LAST over the whole sector with [184,192) zeroed; the zero padding and reserved bytes are checked" {
    const sb = testFixture();
    var img: [sector_len]u8 = undefined;
    try sb.encodeSector(0, &img, null);
    try testing.expectEqualStrings("SQZMETLV", img[0..8]);
    try testing.expectEqual(@as(u16, 4), std.mem.readInt(u16, img[8..10], .little));
    try testing.expectEqualSlices(u8, &.{ 0, 0 }, img[10..12]);
    try testing.expectEqual(@as(u32, 65536), std.mem.readInt(u32, img[12..16], .little));
    try testing.expectEqual(@as(u64, 64 << 20), std.mem.readInt(u64, img[16..24], .little));
    try testing.expectEqual(symmetric_plane, std.mem.readInt(u64, img[24..32], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, img[32..40], .little));
    try testing.expectEqual(@as(u64, 4096), std.mem.readInt(u64, img[40..48], .little));
    try testing.expectEqual(@as(u64, 131072), std.mem.readInt(u64, img[48..56], .little));
    try testing.expectEqual(@as(u64, 135168), std.mem.readInt(u64, img[56..64], .little));
    try testing.expectEqual(@as(u64, 1183744), std.mem.readInt(u64, img[72..80], .little));
    try testing.expectEqual(@as(u64, 1245184), std.mem.readInt(u64, img[88..96], .little));
    try testing.expectEqual(@as(u64, 1245184 + 2 * 65536), std.mem.readInt(u64, img[104..112], .little));
    try testing.expectEqualSlices(u8, "kv-backend-test!", img[120..136]);
    try testing.expectEqualSlices(u8, "set-uuid-fixture", img[136..152]);
    try testing.expectEqual(@as(u64, 0x5EED_CAFE_F00D_D00D), std.mem.readInt(u64, img[152..160], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, img[160..168], .little));
    try testing.expect(std.mem.allEqual(u8, img[168..184], 0));
    try testing.expect(std.mem.allEqual(u8, img[192..], 0));
    // The checksum recipe: the LAST field, over the whole sector.
    var copy = img;
    @memset(copy[184..192], 0);
    try testing.expectEqual(checksum.xxh3(&copy), std.mem.readInt(u64, img[184..192], .little));
    const back = try Superblock.decodeSector(&img, null);
    try testing.expectEqual(sb, back);
    try testing.expectEqual(@as(u64, 1004), back.totalExtents());
    try testing.expectEqual(@as(u64, 256), back.journalPages());
    try testing.expect(back.symmetricPlaneStamped());
    try testing.expect(!back.blockMapTreeStamped());
    try testing.expectEqual(@as(u64, (64 << 20) - 4096), back.backupOffset());
    // The one-member forest: no plane, no directory — every byte of the
    // directory field is zero.
    var flat = sb;
    flat.features_incompat = 0;
    flat.appender_dir = .none;
    var flat_img: [sector_len]u8 = undefined;
    try flat.encodeSector(0, &flat_img, null);
    try testing.expect(std.mem.allEqual(u8, flat_img[104..120], 0));
    try testing.expect(!(try Superblock.decodeSector(&flat_img, null)).symmetricPlaneStamped());
    // Generation lands at 160 and is covered by the checksum.
    var gen_img: [sector_len]u8 = undefined;
    try sb.encodeSector(7, &gen_img, null);
    try testing.expectEqual(@as(u64, 7), sectorGeneration(&gen_img));
    try testing.expect(!std.mem.eql(u8, img[184..192], gen_img[184..192]));
    try testing.expectEqual(@as(u64, 0), sectorGeneration(img[0..100]));
    // Reserved bytes and the padding are CHECKED (restamped so only that rule rejects).
    var diag: Refusal = undefined;
    for ([_]usize{ 10, 170, 300, 4095 }) |at| {
        var bad = img;
        bad[at] = 1;
        checksum.stamp(&bad, off_checksum);
        try testing.expectError(error.ReservedNotZero, Superblock.decodeSector(&bad, &diag));
    }
}

test "decoder check order and refusals: length, magic, version, checksum, reserved, unknown bits (ascending, hex), appender dir without the plane, geometry" {
    const sb = testFixture();
    var img: [sector_len]u8 = undefined;
    try sb.encodeSector(0, &img, null);
    var diag: Refusal = undefined;
    var buf: [512]u8 = undefined;

    try testing.expectError(error.BadLength, Superblock.decodeSector(img[0..4000], &diag));
    try testing.expectEqualStrings("superblock sector must be 4096 bytes, got 4000", msg(&buf, diag));

    var bad = img;
    bad[0] = 'X';
    try testing.expectError(error.BadMagic, Superblock.decodeSector(&bad, &diag));
    try testing.expectEqualStrings("bad superblock magic (corrupted or foreign volume)", msg(&buf, diag));

    bad = img;
    std.mem.writeInt(u16, bad[8..10], 3, .little);
    try testing.expectError(error.WrongVersion, Superblock.decodeSector(&bad, &diag));
    try testing.expectEqualStrings("v4 decoder handed superblock version 3 (classification bug)", msg(&buf, diag));

    // A byte flipped at offset 900 — deep in the padding — fails the checksum.
    bad = img;
    bad[900] ^= 1;
    try testing.expectError(error.ChecksumMismatch, Superblock.decodeSector(&bad, &diag));
    try testing.expect(std.mem.startsWith(u8, msg(&buf, diag), "superblock checksum mismatch: stored 0x"));
    try testing.expect(std.mem.endsWith(u8, msg(&buf, diag), " — corrupted superblock"));

    const Forge = struct {
        fn with(base: Superblock, incompat: u64, out: *[sector_len]u8) void {
            var s = base;
            s.features_incompat = incompat;
            // Forge past the encoder's own gate: lay the fields, then stamp.
            s.encodeSector(0, out, null) catch unreachable;
        }
    };
    Forge.with(sb, symmetric_plane | (1 << 4) | (1 << 40), &bad);
    try testing.expectError(error.UnknownIncompatBits, Superblock.decodeSector(&bad, &diag));
    try testing.expectEqualStrings("unknown incompatible feature bits on the superblock: bit 4, bit 40 (0x10000000010) — upgrade squeezefs to mount this volume", msg(&buf, diag));
    // The old-binary refusal for a LATER capability: a mask without bit 2
    // names bit 2.
    Forge.with(sb, symmetric_plane | block_map_tree, &bad);
    try testing.expectError(error.UnknownIncompatBits, Superblock.decodeSectorWithKnown(&bad, features_incompat_known & ~block_map_tree, &diag));
    try testing.expectEqualStrings("unknown incompatible feature bits on the superblock: bit 2 (0x4) — upgrade squeezefs to mount this volume", msg(&buf, diag));

    // Appender dir named without the plane (dir forged in after the gate).
    Forge.with(sb, 0, &bad);
    try testing.expectError(error.AppenderDirWithoutPlane, Superblock.decodeSector(&bad, &diag));
    try testing.expect(std.mem.startsWith(u8, msg(&buf, diag), "superblock names an appender directory but carries no SYMMETRIC_PLANE"));

    // Unknown ro bits pass; `unknownRo` reports them.
    var ro = sb;
    ro.features_ro = 1 << 3;
    try ro.encodeSector(0, &bad, null);
    const dec = try Superblock.decodeSector(&bad, null);
    try testing.expectEqual(@as(u64, 8), dec.unknownRo());
    try testing.expectEqual(@as(u64, 0), dec.unknownIncompat());
}

test "validate_geometry: every refusal with its text; heap_start % node_size enforced; the triple strictly below the heap; the DUR-5 slot past the heap" {
    const sb = testFixture();
    var diag: Refusal = undefined;
    var buf: [512]u8 = undefined;
    try sb.validateGeometry(null);

    var s = sb;
    s.node_size = 4096;
    try testing.expectError(error.InvalidNodeSize, s.validateGeometry(&diag));
    try testing.expectEqualStrings("invalid node_size 4096: must be a 4 KiB multiple in [65536, 1048576]", msg(&buf, diag));
    s.node_size = 65536 + 1;
    try testing.expectError(error.InvalidNodeSize, s.validateGeometry(&diag));
    s.node_size = 2 * 1048576;
    try testing.expectError(error.InvalidNodeSize, s.validateGeometry(&diag));

    s = sb;
    s.root_ledger.start = 0;
    try testing.expectError(error.LedgerOverlapsSector0, s.validateGeometry(&diag));
    try testing.expectEqualStrings("superblock geometry: root ledger at 0 overlaps sector 0", msg(&buf, diag));

    s = sb;
    s.root_ledger.len = 4096;
    try testing.expectError(error.LedgerLength, s.validateGeometry(&diag));
    try testing.expectEqualStrings("superblock geometry: root ledger length 4096 != the fixed 131072", msg(&buf, diag));

    s = sb;
    s.journal.len = 4000;
    try testing.expectError(error.JournalPlacement, s.validateGeometry(&diag));
    try testing.expectEqualStrings("superblock geometry: journal [135168, +4000) must follow the ledger in whole 4 KiB pages", msg(&buf, diag));
    s.journal = .{ .start = 4096, .len = 8192 };
    try testing.expectError(error.JournalPlacement, s.validateGeometry(&diag));

    s = sb;
    s.alloc_bitmap.start = s.journal.end() - 4096;
    try testing.expectError(error.BitmapOverlapsJournal, s.validateGeometry(&diag));
    try testing.expectEqualStrings("superblock geometry: alloc bitmap at 1179648 overlaps the journal", msg(&buf, diag));

    s = sb;
    s.heap.len = 1004 * 65536 + 4096;
    try testing.expectError(error.HeapShape, s.validateGeometry(&diag));
    try testing.expectEqualStrings("superblock geometry: heap [1245184, +65802240) must be whole 65536-byte extents on a 65536-byte boundary", msg(&buf, diag));
    // A heap start aligned to 4 KiB but not to node_size is refused: the
    // extent index is `addr / node_size`.
    s = sb;
    s.heap.start = 1245184 + 4096;
    s.heap.len = 1003 * 65536;
    s.appender_dir = .none;
    s.features_incompat = 0;
    try testing.expectError(error.HeapShape, s.validateGeometry(&diag));

    // The triple ending inside the heap: no "migrated volume" arm exists.
    s = sb;
    s.heap.start = 1179648;
    s.heap.len = 1005 * 65536;
    s.appender_dir = .none;
    s.features_incompat = 0;
    try testing.expectError(error.TriplePlacement, s.validateGeometry(&diag));
    try testing.expectEqualStrings("superblock geometry: fixed structures (ledger/journal/bitmap ending at 1191936) must sit entirely below heap.start 1179648", msg(&buf, diag));
    s.heap = .{ .start = 0, .len = 1024 * 65536 };
    try testing.expectError(error.TriplePlacement, s.validateGeometry(&diag));

    s = sb;
    s.alloc_bitmap.len = 4096;
    try testing.expectError(error.BitmapTooShort, s.validateGeometry(&diag));
    try testing.expectEqualStrings("superblock geometry: alloc bitmap length 4096 cannot cover 1004 extents", msg(&buf, diag));

    s = sb;
    s.heap.len = 11 * 65536;
    s.appender_dir = .none;
    try testing.expectError(error.HeapBelowReserve, s.validateGeometry(&diag));
    try testing.expectEqualStrings("superblock geometry: heap of 11 extents cannot hold the compaction reserve (≥ 12 required)", msg(&buf, diag));

    // The DUR-5 slot: a heap running to the volume's end, or a volume length
    // below two sectors, leaves no slot.
    s = sb;
    s.heap.len = 1005 * 65536;
    try testing.expectError(error.BackupSlot, s.validateGeometry(&diag));
    try testing.expectEqualStrings("superblock geometry: volume length 67108864 leaves no redundant-superblock sector past the heap end 67108864", msg(&buf, diag));
    s = sb;
    s.volume_len = 4096;
    try testing.expectError(error.BackupSlot, s.validateGeometry(&diag));
    // A longer device than the heap needs is fine: the slot is wherever
    // the stored length says.
    s = sb;
    s.volume_len = 128 << 20;
    try s.validateGeometry(null);
    try testing.expectEqual(@as(u64, (128 << 20) - 4096), s.backupOffset());

    s = sb;
    s.appender_dir = .{ .start = sb.heap.start + 4096, .len = 65536 };
    try testing.expectError(error.AppenderDirShape, s.validateGeometry(&diag));
    try testing.expectEqualStrings("superblock geometry: appender directory [1249280, 1314816) is not one heap extent of 65536 bytes", msg(&buf, diag));
    s.appender_dir = .{ .start = sb.heap.start, .len = 4096 };
    try testing.expectError(error.AppenderDirShape, s.validateGeometry(&diag));
    s.appender_dir = .{ .start = sb.heap.end(), .len = 65536 };
    try testing.expectError(error.AppenderDirShape, s.validateGeometry(&diag));
}

test "classify_sector0: Blank only for all-zero magic; the pre-v4 magic is legacy; foreign magic; v4; another version named BEFORE any checksum check" {
    const sb = testFixture();
    var img: [sector_len]u8 = undefined;
    try sb.encodeSector(0, &img, null);
    var diag: Refusal = undefined;
    var buf: [512]u8 = undefined;

    const zero: [sector_len]u8 = @splat(0);
    try testing.expectEqual(VolumeFormat.blank, try classifySector0(&zero, &diag));
    // A sector with zero magic but junk elsewhere is still Blank (magic decides).
    var junk = zero;
    junk[500] = 0xFF;
    try testing.expectEqual(VolumeFormat.blank, try classifySector0(&junk, &diag));
    try testing.expectError(error.Sector0BadLength, classifySector0(img[0..10], &diag));
    try testing.expectEqualStrings("sector-0 image must be 4096 bytes, got 10", msg(&buf, diag));

    var foreign = img;
    @memcpy(foreign[0..8], "EXT4FS!!");
    try testing.expectError(error.InvalidMagic, classifySector0(&foreign, &diag));
    try testing.expectEqualStrings("invalid superblock magic [69, 88, 84, 52, 70, 83, 33, 33] (expected [83, 81, 90, 77, 69, 84, 76, 86]) — corrupted or foreign volume", msg(&buf, diag));

    // The reference's images: METALV01 at version 3 (and 2) classify legacy.
    var legacy = zero;
    @memcpy(legacy[0..8], "METALV01");
    std.mem.writeInt(u32, legacy[8..12], 3, .little);
    try testing.expectEqual(VolumeFormat{ .legacy = 3 }, try classifySector0(&legacy, &diag));
    std.mem.writeInt(u32, legacy[8..12], 2, .little);
    try testing.expectEqual(VolumeFormat{ .legacy = 2 }, try classifySector0(&legacy, &diag));

    const v4 = try classifySector0(&img, &diag);
    try testing.expectEqual(sb, v4.v4);

    // Version 99 with a now-wrong checksum: "upgrade", never "checksum".
    var future = img;
    std.mem.writeInt(u16, future[8..10], 99, .little);
    try testing.expectError(error.UnsupportedVersion, classifySector0(&future, &diag));
    const m = msg(&buf, diag);
    try testing.expectEqualStrings("unsupported metadata format version 99 (this binary supports exactly 4) — upgrade squeezefs", m);
    try testing.expect(std.mem.indexOf(u8, m, "checksum") == null);

    // A torn v4 primary classifies as a checksum error.
    var torn = img;
    torn[900] ^= 1;
    try testing.expectError(error.ChecksumMismatch, classifySector0(&torn, &diag));

    var mount_buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&mount_buf);
    try (VolumeFormat{ .blank = {} }).writeMountRefusal(&w, "/dev/meta0");
    try testing.expectEqualStrings("/dev/meta0 is not formatted (zeroed superblock) — run `squeezefs format` first", w.buffered());
    w = .fixed(&mount_buf);
    try (VolumeFormat{ .legacy = 3 }).writeMountRefusal(&w, "/dev/meta0");
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "pre-v4") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "version 3") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "reformat required") != null);
}

test "DUR-5: the backup is self-located at the last aligned sector of the superblock's own volume_len; generation = max(valid gens) + 1" {
    try testing.expectEqual(@as(?u64, 67104768), backupOffsetFor(64 << 20));
    try testing.expectEqual(@as(?u64, 268431360), backupOffsetFor(256 << 20));
    try testing.expectEqual(@as(?u64, 8192), backupOffsetFor(12288 + 100));
    try testing.expectEqual(@as(?u64, 4096), backupOffsetFor(8192));
    try testing.expectEqual(@as(?u64, null), backupOffsetFor(8191));
    try testing.expectEqual(@as(?u64, null), backupOffsetFor(0));
    const sb = testFixture();
    try testing.expectEqual(@as(u64, 67104768), sb.backupOffset());

    var p: [sector_len]u8 = undefined;
    var b: [sector_len]u8 = undefined;
    try sb.encodeSector(3, &p, null);
    try sb.encodeSector(5, &b, null);
    try testing.expectEqual(@as(u64, 6), nextSuperblockGeneration(&p, &b));
    try testing.expectEqual(@as(u64, 4), nextSuperblockGeneration(&p, null));
    // An invalid copy contributes nothing; a blank one contributes 0.
    b[900] ^= 1;
    try testing.expectEqual(@as(u64, 4), nextSuperblockGeneration(&p, &b));
    const zero: [sector_len]u8 = @splat(0);
    try testing.expectEqual(@as(u64, 1), nextSuperblockGeneration(&zero, &zero));
    try testing.expectEqual(@as(u64, 1), nextSuperblockGeneration(null, null));
}

test "loader choice: a classifying primary (even Blank/legacy) is authoritative; the backup is consulted only on a primary ERROR, accepted only as a v4 whose own volume_len names the slot it came from; else the primary's error stands" {
    const sb = testFixture();
    const len: u64 = 64 << 20;
    var p: [sector_len]u8 = undefined;
    var b: [sector_len]u8 = undefined;
    try sb.encodeSector(1, &p, null);
    try sb.encodeSector(1, &b, null);
    var diag: Refusal = undefined;

    const ok = try classifyVolumeImages(&p, &b, len, &diag);
    try testing.expectEqual(SuperblockSlot.primary, ok.slot);

    // Blank primary + valid tail: Blank wins (a stale tail never displaces).
    const zero: [sector_len]u8 = @splat(0);
    const blank = try classifyVolumeImages(&zero, &b, len, &diag);
    try testing.expectEqual(SuperblockSlot.primary, blank.slot);
    try testing.expectEqual(VolumeFormat.blank, blank.format);

    // Torn primary: the tail recovers.
    var torn = p;
    torn[900] ^= 1;
    const rec = try classifyVolumeImages(&torn, &b, len, &diag);
    try testing.expectEqual(SuperblockSlot.backup, rec.slot);
    try testing.expectEqual(sb, rec.format.v4);

    // Both torn ⇒ the PRIMARY's checksum error.
    var torn_b = b;
    torn_b[900] ^= 1;
    try testing.expectError(error.ChecksumMismatch, classifyVolumeImages(&torn, &torn_b, len, &diag));
    try testing.expect(diag == .checksum_mismatch);
    // No tail at all, or a blank tail ⇒ the primary's error.
    try testing.expectError(error.ChecksumMismatch, classifyVolumeImages(&torn, null, len, &diag));
    try testing.expectError(error.ChecksumMismatch, classifyVolumeImages(&torn, &zero, len, &diag));
    // A copy whose own volume_len does not name the slot it was read from
    // (a device grown since format read at its NEW tail) is refused: the
    // slot is self-located, not device-located.
    try testing.expectError(error.ChecksumMismatch, classifyVolumeImages(&torn, &b, 128 << 20, &diag));
    // Another version on BOTH copies ⇒ "upgrade", not "checksum".
    var f1 = p;
    var f2 = b;
    std.mem.writeInt(u16, f1[8..10], 99, .little);
    std.mem.writeInt(u16, f2[8..10], 99, .little);
    try testing.expectError(error.UnsupportedVersion, classifyVolumeImages(&f1, &f2, len, &diag));

    var nbuf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&nbuf);
    try writeBackupMountNotice(&w, "/dev/meta0", .{ .checksum_mismatch = .{ .stored = 1, .computed = 2 } }, 67104768, 1);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "/dev/meta0: sector 0 is unreadable (superblock checksum mismatch: stored 0x0000000000000001, computed 0x0000000000000002 — corrupted superblock) — mounting from the redundant superblock copy at offset 67104768 (generation 1)."));
    w = .fixed(&nbuf);
    try writeRepairNotice(&w, "/dev/meta0", 2);
    try testing.expectEqualStrings("/dev/meta0: repaired a damaged sector 0 from the redundant superblock copy (generation 2)", w.buffered());
}

test "write plan: backup → barrier → sector 0 → barrier, one image at one generation; repair = sector 0 alone; an invalid geometry never encodes" {
    const sb = testFixture();
    const plan = try planWrite(sb, 9, null);
    try testing.expectEqual(@as(usize, 2), plan.slice().len);
    try testing.expectEqual(@as(u64, 67104768), plan.slice()[0].offset);
    try testing.expect(plan.slice()[0].barrier_after);
    try testing.expectEqual(@as(u64, 0), plan.slice()[1].offset);
    try testing.expect(plan.slice()[1].barrier_after);
    try testing.expectEqual(@as(u64, 9), sectorGeneration(plan.bytes()));
    try testing.expectEqual(sb, (try classifySector0(plan.bytes(), null)).v4);

    const repair = try planRepairPrimary(sb, 4, null);
    try testing.expectEqual(@as(u64, 0), repair.step.offset);
    try testing.expect(repair.step.barrier_after);
    try testing.expectEqual(@as(u64, 4), sectorGeneration(&repair.image));

    var bad = sb;
    bad.heap.len = 11 * 65536;
    var diag: Refusal = undefined;
    try testing.expectError(error.HeapBelowReserve, planWrite(bad, 1, &diag));
}

test "stampers: a set bit is a no-op; the plane rides with its directory and refuses a re-point" {
    var sb = testFixture();
    try testing.expect(!sb.stampBit(symmetric_plane));
    try testing.expect(sb.stampBit(block_map_tree));
    try testing.expect(sb.blockMapTreeStamped());
    var diag: Refusal = undefined;
    const dir = sb.appender_dir;
    try testing.expect(!try sb.setSymmetricPlane(dir, &diag));
    try testing.expectError(error.SymmetricPlaneRepoint, sb.setSymmetricPlane(.{ .start = dir.start + 65536, .len = 65536 }, &diag));
    var buf: [256]u8 = undefined;
    try testing.expectEqualStrings("SYMMETRIC_PLANE is already stamped with appender directory 0x150000+65536 — refusing to re-point it at 0x160000+65536", msg(&buf, diag));
    var flat = testFixture();
    flat.features_incompat = 0;
    flat.appender_dir = .none;
    try testing.expect(try flat.setSymmetricPlane(dir, &diag));
    try testing.expect(flat.symmetricPlaneStamped());
    try testing.expectEqual(dir, flat.appender_dir);
    try flat.validateGeometry(null);
}

test "property: the sector decoder is total on arbitrary and restamped bytes; a decoded superblock re-encodes to its image" {
    var prng = std.Random.DefaultPrng.init(0x5B5B);
    const r = prng.random();
    var buf: [sector_len]u8 = undefined;
    var base: [sector_len]u8 = undefined;
    try testFixture().encodeSector(2, &base, null);
    for (0..3000) |i| {
        if (i % 3 == 0) {
            r.bytes(&buf);
            _ = classifySector0(&buf, null) catch {};
            continue;
        }
        buf = base;
        const flips = r.intRangeAtMost(usize, 1, 4);
        for (0..flips) |_| buf[r.intRangeLessThan(usize, 0, fixed_len)] ^= @as(u8, 1) << r.int(u3);
        checksum.stamp(&buf, off_checksum);
        if (classifySector0(&buf, null)) |fmt| {
            if (fmt == .v4) {
                var again: [sector_len]u8 = undefined;
                try fmt.v4.encodeSector(sectorGeneration(&buf), &again, null);
                try testing.expectEqualSlices(u8, &buf, &again);
            }
        } else |_| {}
    }
}
