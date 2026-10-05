//! Superblock v3 — sector 0, byte-exact (charter §5.1, §1.7;
//! `docs/porting/kv_alloc_superblock.md` §1). Pure: images in, images out.
//! The loader's primary/backup choice and the write order are expressed over
//! images and as an ordered plan the io task executes.
//!
//! Forward-only (charter §1.7): every incompat bit means ONE complete thing;
//! an unknown incompat bit refuses loud naming the bit; unknown ro bits
//! classify fine and the mount is read-only. The version gate precedes the
//! checksum so a future format is reported as such, never as a tear.

const std = @import("std");
const checksum = @import("../core/checksum.zig");
const alloc_core = @import("alloc_core.zig");
const alloc_ext = @import("alloc_ext.zig");

/// One O_DIRECT sector: the metadata grain and the superblock's durable unit.
pub const sector_len: usize = 4096;
/// Shared with the retired v2 — the version discriminates.
pub const magic_value = "METALV01";
/// v2 was the fixed-geometry format; `version > 2` already failed loud in
/// every shipped binary.
pub const version_v3: u32 = 3;

pub const off_magic = 0;
pub const off_version = 8;
pub const off_node_size = 12;
pub const off_feat_incompat = 16;
pub const off_feat_ro = 24;
pub const off_root_ledger = 32;
pub const off_journal = 48;
pub const off_alloc_bitmap = 64;
pub const off_heap = 80;
pub const off_uuid = 96;
pub const off_hash_seed = 112;
pub const off_checksum = 120;
pub const off_sb_generation = 128;
pub const off_appender_dir = 136;
/// Fixed fields end here; `[152, 4096)` is zero padding under the checksum.
pub const fixed_len = 152;

/// The fixed-field layout, declared for its size and offsets; the codec
/// below reads and writes little-endian at the named offsets.
pub const Sector0Fixed = extern struct {
    magic: [8]u8,
    version: u32,
    node_size: u32,
    features_incompat: u64,
    features_ro: u64,
    root_ledger: [2]u64,
    journal: [2]u64,
    alloc_bitmap: [2]u64,
    heap: [2]u64,
    uuid: [16]u8,
    hash_seed: u64,
    checksum: u64,
    sb_generation: u64,
    appender_dir: [2]u64,
};

comptime {
    std.debug.assert(@sizeOf(Sector0Fixed) == fixed_len);
    std.debug.assert(@offsetOf(Sector0Fixed, "version") == off_version);
    std.debug.assert(@offsetOf(Sector0Fixed, "node_size") == off_node_size);
    std.debug.assert(@offsetOf(Sector0Fixed, "features_incompat") == off_feat_incompat);
    std.debug.assert(@offsetOf(Sector0Fixed, "features_ro") == off_feat_ro);
    std.debug.assert(@offsetOf(Sector0Fixed, "root_ledger") == off_root_ledger);
    std.debug.assert(@offsetOf(Sector0Fixed, "journal") == off_journal);
    std.debug.assert(@offsetOf(Sector0Fixed, "alloc_bitmap") == off_alloc_bitmap);
    std.debug.assert(@offsetOf(Sector0Fixed, "heap") == off_heap);
    std.debug.assert(@offsetOf(Sector0Fixed, "uuid") == off_uuid);
    std.debug.assert(@offsetOf(Sector0Fixed, "hash_seed") == off_hash_seed);
    std.debug.assert(@offsetOf(Sector0Fixed, "checksum") == off_checksum);
    std.debug.assert(@offsetOf(Sector0Fixed, "sb_generation") == off_sb_generation);
    std.debug.assert(@offsetOf(Sector0Fixed, "appender_dir") == off_appender_dir);
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
// features_incompat: every bit means ONE complete thing.
// ---------------------------------------------------------------------------

pub const Presence = enum {
    /// The decoder refuses without it.
    required,
    /// Refused when the deciding binary knows the bit (mask-relative).
    required_when_known,
    /// Set at format by the builder (never a mount gate on its own).
    format_time,
    /// Stamped bit-before-record by the backend; never at format.
    stamped_later,
};

pub const IncompatBit = struct { bit: u6, name: []const u8, presence: Presence };

pub const incompat_bits = [_]IncompatBit{
    .{ .bit = 0, .name = "KV_V3", .presence = .required },
    .{ .bit = 1, .name = "NODE_SEQ_WATERMARK", .presence = .required },
    .{ .bit = 2, .name = "KV_GUEST_SLOTS", .presence = .format_time },
    .{ .bit = 3, .name = "KV_VOLUME_LIFECYCLE", .presence = .stamped_later },
    .{ .bit = 4, .name = "KV_SLOT_MIGRATION", .presence = .format_time },
    .{ .bit = 5, .name = "KV_LAYOUT_DELTAS", .presence = .stamped_later },
    .{ .bit = 6, .name = "KV_DYNAMIC_ROUTING", .presence = .required_when_known },
    .{ .bit = 7, .name = "KV_DURABLE_TERM", .presence = .format_time },
    .{ .bit = 8, .name = "KV_PARTITIONED_APPEND", .presence = .format_time },
    .{ .bit = 9, .name = "KV_BLOCK_REFCOUNTS", .presence = .format_time },
    .{ .bit = 10, .name = "KV_WRITER_SCOPED_STAGING", .presence = .format_time },
    .{ .bit = 11, .name = "KV_MULTI_WRITER_DATA", .presence = .format_time },
    .{ .bit = 12, .name = "KV_INO_LANES", .presence = .format_time },
    .{ .bit = 13, .name = "KV_BLOCK_KEY_INCARNATION", .presence = .format_time },
    .{ .bit = 14, .name = "KV_CLAIM_SET", .presence = .format_time },
    .{ .bit = 15, .name = "KV_LAYOUT_VERSIONS", .presence = .format_time },
    .{ .bit = 16, .name = "KV_BLOCK_MAP_TREE", .presence = .stamped_later },
    .{ .bit = 17, .name = "KV_SYMMETRIC_FOREST", .presence = .format_time },
};

pub const kv_v3: u64 = 1 << 0;
pub const node_seq_watermark: u64 = 1 << 1;
pub const kv_guest_slots: u64 = 1 << 2;
pub const kv_volume_lifecycle: u64 = 1 << 3;
pub const kv_slot_migration: u64 = 1 << 4;
pub const kv_layout_deltas: u64 = 1 << 5;
pub const kv_dynamic_routing: u64 = 1 << 6;
pub const kv_durable_term: u64 = 1 << 7;
pub const kv_partitioned_append: u64 = 1 << 8;
pub const kv_block_refcounts: u64 = 1 << 9;
pub const kv_writer_scoped_staging: u64 = 1 << 10;
/// The TERMINAL multi-writer bit: "bit 11 set ⇒ all nine set".
pub const kv_multi_writer_data: u64 = 1 << 11;
pub const kv_ino_lanes: u64 = 1 << 12;
/// Requires bit 7 (the stamper refuses otherwise).
pub const kv_block_key_incarnation: u64 = 1 << 13;
pub const kv_claim_set: u64 = 1 << 14;
pub const kv_layout_versions: u64 = 1 << 15;
pub const kv_block_map_tree: u64 = 1 << 16;
/// The forest + appender directory, stamped by every default `format`.
pub const kv_symmetric_forest: u64 = 1 << 17;
pub const writer_scoped_staging_bit: u32 = 10;

/// The nine mw bits, stamped as ONE act.
pub const multi_writer_format_bits: u64 = kv_durable_term | kv_partitioned_append | kv_block_refcounts | kv_writer_scoped_staging | kv_multi_writer_data | kv_ino_lanes | kv_block_key_incarnation | kv_claim_set | kv_layout_versions;

/// Bits 0..=17; anything above refuses.
pub const features_incompat_known: u64 = (1 << 18) - 1;
/// No ro bits yet: any set ro bit is unknown ⇒ the mount is read-only.
pub const features_ro_known: u64 = 0;

/// The fresh-format word before the builder's class bits.
pub const plan_features_incompat: u64 = kv_v3 | node_seq_watermark | kv_dynamic_routing | kv_durable_term;

// ---------------------------------------------------------------------------
// The struct and its codec.
// ---------------------------------------------------------------------------

pub const SuperblockV3 = struct {
    node_size: u32,
    features_incompat: u64,
    features_ro: u64,
    root_ledger: ExtentRef,
    journal: ExtentRef,
    alloc_bitmap: ExtentRef,
    heap: ExtentRef,
    uuid: [16]u8,
    hash_seed: u64,
    /// Bit 17 only; all-zero when absent.
    appender_dir: ExtentRef,

    pub fn totalExtents(self: SuperblockV3) u64 {
        return self.heap.len / self.node_size;
    }

    pub fn journalPages(self: SuperblockV3) u64 {
        return self.journal.len / journal_page_len;
    }

    pub fn unknownIncompat(self: SuperblockV3) u64 {
        return self.features_incompat & ~features_incompat_known;
    }

    pub fn unknownRo(self: SuperblockV3) u64 {
        return self.features_ro & ~features_ro_known;
    }

    pub fn blockMapTreeStamped(self: SuperblockV3) bool {
        return self.features_incompat & kv_block_map_tree != 0;
    }

    pub fn symmetricForestStamped(self: SuperblockV3) bool {
        return self.features_incompat & kv_symmetric_forest != 0;
    }

    pub fn multiWriterClass(self: SuperblockV3) bool {
        return self.features_incompat & kv_multi_writer_data != 0;
    }

    /// Validates the geometry, then lays every field at its offset and
    /// stamps the checksum over the whole sector. `appender_dir` is written
    /// only when non-zero, so a flat image is byte-identical to the
    /// pre-bit-17 image.
    pub fn encodeSector(self: SuperblockV3, generation: u64, out: *[sector_len]u8, diag: ?*Refusal) DecodeError!void {
        try self.validateGeometry(diag);
        @memset(out, 0);
        @memcpy(out[off_magic..][0..8], magic_value);
        std.mem.writeInt(u32, out[off_version..][0..4], version_v3, .little);
        std.mem.writeInt(u32, out[off_node_size..][0..4], self.node_size, .little);
        std.mem.writeInt(u64, out[off_feat_incompat..][0..8], self.features_incompat, .little);
        std.mem.writeInt(u64, out[off_feat_ro..][0..8], self.features_ro, .little);
        writeExtent(out, off_root_ledger, self.root_ledger);
        writeExtent(out, off_journal, self.journal);
        writeExtent(out, off_alloc_bitmap, self.alloc_bitmap);
        writeExtent(out, off_heap, self.heap);
        @memcpy(out[off_uuid..][0..16], &self.uuid);
        std.mem.writeInt(u64, out[off_hash_seed..][0..8], self.hash_seed, .little);
        std.mem.writeInt(u64, out[off_sb_generation..][0..8], generation, .little);
        if (!self.appender_dir.isNone()) writeExtent(out, off_appender_dir, self.appender_dir);
        checksum.stamp(out, off_checksum);
    }

    pub fn decodeSector(buf: []const u8, diag: ?*Refusal) DecodeError!SuperblockV3 {
        return decodeSectorWithKnown(buf, features_incompat_known, diag);
    }

    /// The decoder in its exact check order: length → magic → version →
    /// checksum → fields → KV_V3 → watermark → dynamic routing (when the
    /// deciding mask knows it) → unknown bits → appender dir without bit
    /// 17 → geometry. `known` models the deciding binary (tests pass
    /// historical masks to prove the refusal an OLD binary would issue).
    pub fn decodeSectorWithKnown(buf: []const u8, known: u64, diag: ?*Refusal) DecodeError!SuperblockV3 {
        if (buf.len != sector_len) return refuse(diag, .{ .bad_length = buf.len });
        if (!std.mem.eql(u8, buf[off_magic..][0..8], magic_value)) return refuse(diag, .bad_magic);
        const version = std.mem.readInt(u32, buf[off_version..][0..4], .little);
        if (version != version_v3) return refuse(diag, .{ .wrong_version = version });
        const stored = std.mem.readInt(u64, buf[off_checksum..][0..8], .little);
        const computed = checksum.xxh3FieldZeroed(buf, off_checksum);
        if (stored != computed) return refuse(diag, .{ .checksum_mismatch = .{ .stored = stored, .computed = computed } });
        const sb = SuperblockV3{
            .node_size = std.mem.readInt(u32, buf[off_node_size..][0..4], .little),
            .features_incompat = std.mem.readInt(u64, buf[off_feat_incompat..][0..8], .little),
            .features_ro = std.mem.readInt(u64, buf[off_feat_ro..][0..8], .little),
            .root_ledger = readExtent(buf, off_root_ledger),
            .journal = readExtent(buf, off_journal),
            .alloc_bitmap = readExtent(buf, off_alloc_bitmap),
            .heap = readExtent(buf, off_heap),
            .uuid = buf[off_uuid..][0..16].*,
            .hash_seed = std.mem.readInt(u64, buf[off_hash_seed..][0..8], .little),
            .appender_dir = readExtent(buf, off_appender_dir),
        };
        if (sb.features_incompat & kv_v3 == 0) return refuse(diag, .missing_kv_v3);
        if (sb.features_incompat & node_seq_watermark == 0) return refuse(diag, .pre_watermark);
        if (known & kv_dynamic_routing != 0 and sb.features_incompat & kv_dynamic_routing == 0) return refuse(diag, .frozen_routing_width);
        const unknown = sb.features_incompat & ~known;
        if (unknown != 0) return refuse(diag, .{ .unknown_incompat_bits = unknown });
        if (!sb.appender_dir.isNone() and !sb.symmetricForestStamped()) return refuse(diag, .appender_dir_without_bit17);
        try sb.validateGeometry(diag);
        return sb;
    }

    /// Every geometry field validated before any caller dereferences it.
    /// Two legal placements of the ledger/journal/bitmap triple: entirely
    /// BELOW the heap (fresh format) or entirely WITHIN it on reserved
    /// extents (the retired migrator's output — still a legal v3 geometry).
    pub fn validateGeometry(self: SuperblockV3, diag: ?*Refusal) DecodeError!void {
        if (!nodeSizeValid(self.node_size)) return refuse(diag, .{ .invalid_node_size = self.node_size });
        const node_size: u64 = self.node_size;
        if (self.root_ledger.start < sector_len) return refuse(diag, .{ .ledger_overlaps_sector0 = self.root_ledger.start });
        if (self.root_ledger.len != root_ledger_len) return refuse(diag, .{ .ledger_length = self.root_ledger.len });
        if (self.journal.start < self.root_ledger.end() or self.journal.len == 0 or self.journal.len % journal_page_len != 0) {
            return refuse(diag, .{ .journal_placement = self.journal });
        }
        if (self.alloc_bitmap.start < self.journal.end()) return refuse(diag, .{ .bitmap_overlaps_journal = self.alloc_bitmap.start });
        if (self.heap.start % node_page != 0 or self.heap.len < node_size or self.heap.len % node_size != 0) {
            return refuse(diag, .{ .heap_shape = .{ .heap = self.heap, .node_size = node_size } });
        }
        const triple_below = self.alloc_bitmap.end() <= self.heap.start;
        const triple_within = self.heap.start <= self.root_ledger.start and self.alloc_bitmap.end() <= self.heap.end();
        if (!(triple_below or triple_within)) {
            return refuse(diag, .{ .triple_placement = .{ .triple_end = self.alloc_bitmap.end(), .heap = self.heap } });
        }
        const total = self.totalExtents();
        if (self.alloc_bitmap.len < alloc_ext.bitmapRegionLen(total)) {
            return refuse(diag, .{ .bitmap_too_short = .{ .len = self.alloc_bitmap.len, .total = total } });
        }
        // The same floor `plan` enforces: a crafted (still checksummed)
        // superblock must not hand the allocator a reserve ≥ total.
        const min_extents = alloc_core.compactionReserveExtents(total) + 4;
        if (total < min_extents) return refuse(diag, .{ .heap_below_reserve = .{ .total = total, .min = min_extents } });
        if (self.appender_dir.len != 0 and (self.appender_dir.len != node_size or self.appender_dir.start < self.heap.start or self.appender_dir.end() > self.heap.end() or (self.appender_dir.start - self.heap.start) % node_size != 0)) {
            return refuse(diag, .{ .appender_dir_shape = .{ .dir = self.appender_dir, .node_size = node_size } });
        }
    }

    /// The stamper's pure half: returns false when already set (no write).
    pub fn stampBit(self: *SuperblockV3, bit: u64) bool {
        if (self.features_incompat & bit != 0) return false;
        self.features_incompat |= bit;
        return true;
    }

    /// Bit 17 + its directory in ONE act; a different directory already
    /// stamped is a refusal.
    pub fn setSymmetricForest(self: *SuperblockV3, dir: ExtentRef, diag: ?*Refusal) error{SymmetricForestRepoint}!bool {
        if (self.symmetricForestStamped()) {
            if (self.appender_dir.start == dir.start and self.appender_dir.len == dir.len) return false;
            if (diag) |d| d.* = .{ .symmetric_forest_repoint = .{ .have = self.appender_dir, .want = dir } };
            return error.SymmetricForestRepoint;
        }
        self.features_incompat |= kv_symmetric_forest;
        self.appender_dir = dir;
        return true;
    }
};

fn writeExtent(out: *[sector_len]u8, off: usize, ext: ExtentRef) void {
    std.mem.writeInt(u64, out[off..][0..8], ext.start, .little);
    std.mem.writeInt(u64, out[off + 8 ..][0..8], ext.len, .little);
}

fn readExtent(buf: []const u8, off: usize) ExtentRef {
    return .{ .start = std.mem.readInt(u64, buf[off..][0..8], .little), .len = std.mem.readInt(u64, buf[off + 8 ..][0..8], .little) };
}

// ---------------------------------------------------------------------------
// Refusals: typed, and every string in ONE place.
// ---------------------------------------------------------------------------

pub const DecodeError = error{
    BadLength,
    BadMagic,
    WrongVersion,
    ChecksumMismatch,
    MissingKvV3,
    PreWatermark,
    FrozenRoutingWidth,
    UnknownIncompatBits,
    AppenderDirWithoutBit17,
    InvalidNodeSize,
    LedgerOverlapsSector0,
    LedgerLength,
    JournalPlacement,
    BitmapOverlapsJournal,
    HeapShape,
    TriplePlacement,
    BitmapTooShort,
    HeapBelowReserve,
    AppenderDirShape,
    Sector0BadLength,
    InvalidMagic,
    UnsupportedVersion,
};

pub const Refusal = union(enum) {
    bad_length: usize,
    bad_magic,
    wrong_version: u32,
    checksum_mismatch: struct { stored: u64, computed: u64 },
    missing_kv_v3,
    pre_watermark,
    frozen_routing_width,
    unknown_incompat_bits: u64,
    appender_dir_without_bit17,
    invalid_node_size: u32,
    ledger_overlaps_sector0: u64,
    ledger_length: u64,
    journal_placement: ExtentRef,
    bitmap_overlaps_journal: u64,
    heap_shape: struct { heap: ExtentRef, node_size: u64 },
    triple_placement: struct { triple_end: u64, heap: ExtentRef },
    bitmap_too_short: struct { len: u64, total: u64 },
    heap_below_reserve: struct { total: u64, min: u64 },
    appender_dir_shape: struct { dir: ExtentRef, node_size: u64 },
    sector0_bad_length: usize,
    invalid_magic: [8]u8,
    unsupported_version: u32,
    symmetric_forest_repoint: struct { have: ExtentRef, want: ExtentRef },

    pub fn err(self: Refusal) DecodeError {
        return switch (self) {
            .bad_length => error.BadLength,
            .bad_magic => error.BadMagic,
            .wrong_version => error.WrongVersion,
            .checksum_mismatch => error.ChecksumMismatch,
            .missing_kv_v3 => error.MissingKvV3,
            .pre_watermark => error.PreWatermark,
            .frozen_routing_width => error.FrozenRoutingWidth,
            .unknown_incompat_bits => error.UnknownIncompatBits,
            .appender_dir_without_bit17 => error.AppenderDirWithoutBit17,
            .invalid_node_size => error.InvalidNodeSize,
            .ledger_overlaps_sector0 => error.LedgerOverlapsSector0,
            .ledger_length => error.LedgerLength,
            .journal_placement => error.JournalPlacement,
            .bitmap_overlaps_journal => error.BitmapOverlapsJournal,
            .heap_shape => error.HeapShape,
            .triple_placement => error.TriplePlacement,
            .bitmap_too_short => error.BitmapTooShort,
            .heap_below_reserve => error.HeapBelowReserve,
            .appender_dir_shape => error.AppenderDirShape,
            .sector0_bad_length => error.Sector0BadLength,
            .invalid_magic => error.InvalidMagic,
            .unsupported_version => error.UnsupportedVersion,
            .symmetric_forest_repoint => unreachable,
        };
    }

    /// The refusal text VERBATIM (the reference's strings; geometry ones
    /// carry the `v3 superblock geometry: ` prefix, the node-size one does
    /// not — it is the node layer's own message passed through).
    pub fn write(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .bad_length => |n| try w.print("v3 superblock sector must be {d} bytes, got {d}", .{ sector_len, n }),
            .bad_magic => try w.writeAll("bad v3 superblock magic (corrupted or foreign volume)"),
            .wrong_version => |v| try w.print("v3 decoder handed superblock version {d} (classification bug)", .{v}),
            .checksum_mismatch => |c| try w.print("superblock checksum mismatch: stored 0x{x:0>16}, computed 0x{x:0>16} — corrupted superblock", .{ c.stored, c.computed }),
            .missing_kv_v3 => try w.writeAll("v3 superblock without the KV_V3 incompat bit (corrupt feature field)"),
            .pre_watermark => try w.writeAll("pre-watermark v3 volume: formatted before the node-seq mint watermark (Finding A) and no longer supported; reformat required"),
            .frozen_routing_width => try w.writeAll("v3 volume formatted with a frozen routing width (pre-dynamic-meta-routing) — no longer supported: routing widths are derived now, never chosen; reformat required (`squeezefs format --force`, destroys the old contents)"),
            .unknown_incompat_bits => |unknown| {
                try w.writeAll("unknown incompatible feature bits on v3 superblock: ");
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
            .appender_dir_without_bit17 => try w.writeAll("v3 superblock names an appender directory but carries no incompat bit 17 — the directory is part of bit 17's meaning (design-symmetric-metadata §7.1)"),
            .invalid_node_size => |n| try w.print("invalid node_size {d}: must be a 4 KiB multiple in [65536, 1048576]", .{n}),
            .ledger_overlaps_sector0 => |s| try w.print("v3 superblock geometry: root ledger at {d} overlaps sector 0", .{s}),
            .ledger_length => |l| try w.print("v3 superblock geometry: root ledger length {d} != the fixed {d}", .{ l, root_ledger_len }),
            .journal_placement => |j| try w.print("v3 superblock geometry: journal [{d}, +{d}) must follow the ledger in whole 4 KiB pages", .{ j.start, j.len }),
            .bitmap_overlaps_journal => |s| try w.print("v3 superblock geometry: alloc bitmap at {d} overlaps the journal", .{s}),
            .heap_shape => |h| try w.print("v3 superblock geometry: heap [{d}, +{d}) must be whole {d}-byte extents on a {d}-byte boundary", .{ h.heap.start, h.heap.len, h.node_size, node_page }),
            .triple_placement => |t| try w.print("v3 superblock geometry: fixed structures (ledger/journal/bitmap ending at {d}) must sit either entirely below heap.start {d} (fresh format) or entirely within the heap [{d}, {d}) on reserved extents (migrated volume) — got neither", .{ t.triple_end, t.heap.start, t.heap.start, t.heap.end() }),
            .bitmap_too_short => |b| try w.print("v3 superblock geometry: alloc bitmap length {d} cannot cover {d} extents", .{ b.len, b.total }),
            .heap_below_reserve => |h| try w.print("v3 superblock geometry: heap of {d} extents cannot hold the compaction reserve (≥ {d} required)", .{ h.total, h.min }),
            .appender_dir_shape => |a| try w.print("v3 superblock geometry: appender directory [{d}, {d}) is not one heap extent of {d} bytes", .{ a.dir.start, a.dir.end(), a.node_size }),
            .sector0_bad_length => |n| try w.print("sector-0 image must be {d} bytes, got {d}", .{ sector_len, n }),
            .invalid_magic => |m| {
                try w.writeAll("invalid superblock magic ");
                try writeByteList(w, &m);
                try w.writeAll(" (expected ");
                try writeByteList(w, magic_value);
                try w.writeAll(") — corrupted or foreign volume");
            },
            .unsupported_version => |v| try w.print("unsupported metadata format version {d} (this binary supports <= {d}) — upgrade squeezefs", .{ v, version_v3 }),
            .symmetric_forest_repoint => |r| try w.print("bit 17 is already stamped with appender directory 0x{x}+{d} — refusing to re-point it at 0x{x}+{d}", .{ r.have.start, r.have.len, r.want.start, r.want.len }),
        }
    }
};

/// Rust's `{:?}` of a byte slice: `[77, 69, 84, …]`.
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
    /// SqueezeFS magic with version ≤ 2: recognized, never decoded.
    v2_legacy,
    v3: SuperblockV3,

    /// The mount's refusal for the two non-v3 classifications.
    pub fn writeMountRefusal(self: VolumeFormat, w: *std.Io.Writer, path: []const u8) std.Io.Writer.Error!void {
        switch (self) {
            .blank => try w.print("{s} is not formatted (zeroed superblock) — run `squeezefs format` first", .{path}),
            .v2_legacy => try w.print("Metadata volume {s} is format v2, which is no longer supported (this binary mounts only format v3) — v2 volumes must be reformatted: run `squeezefs format` (destroys the old contents)", .{path}),
            .v3 => {},
        }
    }
};

pub fn classifySector0(sector: []const u8, diag: ?*Refusal) DecodeError!VolumeFormat {
    if (sector.len != sector_len) return refuse(diag, .{ .sector0_bad_length = sector.len });
    const magic = sector[off_magic..][0..8];
    if (std.mem.allEqual(u8, magic, 0)) return .blank;
    if (!std.mem.eql(u8, magic, magic_value)) return refuse(diag, .{ .invalid_magic = magic.* });
    const version = std.mem.readInt(u32, sector[off_version..][0..4], .little);
    if (version <= 2) return .v2_legacy;
    if (version == version_v3) return .{ .v3 = try SuperblockV3.decodeSector(sector, diag) };
    return refuse(diag, .{ .unsupported_version = version });
}

/// The DUR-5 generation stamped in an image (0 on anything older or
/// shorter). Meaningful only for images that classify.
pub fn sectorGeneration(sector: []const u8) u64 {
    if (sector.len < off_sb_generation + 8) return 0;
    return std.mem.readInt(u64, sector[off_sb_generation..][0..8], .little);
}

// ---------------------------------------------------------------------------
// DUR-5: the redundant copy at the LAST aligned sector.
// ---------------------------------------------------------------------------

/// Why the tail and not a second sector beside sector 0: every v3 volume
/// puts the ledger at 4096, so there is no reserved space beside sector 0 to
/// A/B into; the tail needs no layout change (`plan` holds it back out of the
/// heap, one extent at most). `null` below two sectors (it must never alias
/// sector 0).
pub fn backupOffset(volume_len: u64) ?u64 {
    const aligned = (volume_len / sector_len) * sector_len;
    if (aligned < 2 * sector_len) return null;
    return aligned - sector_len;
}

/// The slot for `sb`, or null when the heap runs to the tail (pre-DUR-5
/// formats): the copy is not written rather than scribbled over a live node.
pub fn backupSlotFor(sb: SuperblockV3, volume_len: u64) ?u64 {
    const off = backupOffset(volume_len) orelse return null;
    return if (off >= sb.heap.end()) off else null;
}

/// `max(gen of VALID primary, gen of VALID backup) + 1` — "valid" is
/// `classifySector0` succeeding (Blank and V2Legacy included, at 0).
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
/// V2Legacy — is authoritative and the backup is never consulted (a stale or
/// crafted tail can never displace a readable primary). Only when the
/// primary ERRORS is the tail read; accepted iff it classifies V3 AND
/// reserves the slot it came from (`off ≥ heap.end`); otherwise the
/// PRIMARY's error stands.
pub fn classifyVolumeImages(primary: []const u8, backup: ?[]const u8, volume_len: u64, diag: ?*Refusal) DecodeError!Classified {
    var primary_refusal: Refusal = undefined;
    const primary_err = blk: {
        const fmt = classifySector0(primary, &primary_refusal) catch |e| break :blk e;
        return .{ .format = fmt, .slot = .primary };
    };
    if (backup) |tail| {
        if (backupOffset(volume_len)) |off| {
            if (classifySector0(tail, null)) |fmt| {
                if (fmt == .v3 and off >= fmt.v3.heap.end()) return .{ .format = fmt, .slot = .backup };
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
    len: u8,
    /// The heap runs to the tail (pre-DUR-5): no copy; the caller logs the
    /// warning text below once.
    backup_missing: bool,

    pub fn slice(self: *const WritePlan) []const WriteStep {
        return self.steps[0..self.len];
    }

    pub fn bytes(self: *const WritePlan) []const u8 {
        return &self.image;
    }

    pub fn writeNoBackupWarning(w: *std.Io.Writer, path: []const u8) std.Io.Writer.Error!void {
        try w.print("{s}: no room for the redundant superblock copy (the heap runs to the end of the volume — a pre-DUR-5 format); sector 0 has no backup. Reformatting reserves the tail sector.", .{path});
    }
};

/// `write_superblock_at_generation`: the backup copy first (when the geometry
/// reserves it), barrier, then sector 0, barrier.
pub fn planWrite(sb: SuperblockV3, volume_len: u64, generation: u64, diag: ?*Refusal) DecodeError!WritePlan {
    var plan: WritePlan = .{ .image = undefined, .generation = generation, .steps = undefined, .len = 0, .backup_missing = false };
    try sb.encodeSector(generation, &plan.image, diag);
    if (backupSlotFor(sb, volume_len)) |off| {
        plan.steps[plan.len] = .{ .offset = off, .barrier_after = true };
        plan.len += 1;
    } else {
        plan.backup_missing = true;
    }
    plan.steps[plan.len] = .{ .offset = 0, .barrier_after = true };
    plan.len += 1;
    return plan;
}

/// `repair_primary_superblock` (write mounts that selected the backup):
/// sector 0 alone at the next generation, barrier.
pub fn planRepairPrimary(sb: SuperblockV3, generation: u64, diag: ?*Refusal) DecodeError!WritePlan {
    var plan: WritePlan = .{ .image = undefined, .generation = generation, .steps = undefined, .len = 1, .backup_missing = false };
    try sb.encodeSector(generation, &plan.image, diag);
    plan.steps[0] = .{ .offset = 0, .barrier_after = true };
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
/// with the default-format word: bits {0,1,2,4,6,7,8..15,17} = 0x2FFD7 (the
/// spec's `0x3FFD7` transcribes bit 16 in, which the builder never stamps at
/// format — `builder.rs:496-560` sets only 2|4, the nine mw bits and 17).
pub fn testFixture() SuperblockV3 {
    return .{
        .node_size = 65536,
        .features_incompat = 0x2FFD7,
        .features_ro = 0,
        .root_ledger = .{ .start = 4096, .len = 131072 },
        .journal = .{ .start = 135168, .len = 1048576 },
        .alloc_bitmap = .{ .start = 1183744, .len = 8192 },
        .heap = .{ .start = 1245184, .len = 1004 * 65536 },
        .uuid = "kv-backend-test!".*,
        .hash_seed = 0x5EED_CAFE_F00D_D00D,
        .appender_dir = .{ .start = 1245184 + 2 * 65536, .len = 65536 },
    };
}

fn msg(buf: []u8, r: Refusal) []const u8 {
    var w: std.Io.Writer = .fixed(buf);
    r.write(&w) catch unreachable;
    return w.buffered();
}

test "incompat table: 18 single bits, pairwise disjoint, union == KNOWN; the nine mw bits; ro known is 0" {
    var acc: u64 = 0;
    for (incompat_bits, 0..) |b, i| {
        try testing.expectEqual(@as(u6, @intCast(i)), b.bit);
        const mask = @as(u64, 1) << b.bit;
        try testing.expect(std.math.isPowerOfTwo(mask));
        try testing.expectEqual(@as(u64, 0), acc & mask);
        acc |= mask;
    }
    try testing.expectEqual(features_incompat_known, acc);
    try testing.expectEqual(@as(u64, 0xFF80), multi_writer_format_bits);
    try testing.expectEqual(@as(u64, 0), features_ro_known);
    try testing.expectEqual(@as(u64, 0xC3), plan_features_incompat);
    try testing.expectEqual(Presence.required, incompat_bits[0].presence);
    try testing.expectEqual(Presence.required, incompat_bits[1].presence);
    try testing.expectEqual(Presence.required_when_known, incompat_bits[6].presence);
    try testing.expectEqual(@as(u32, 10), writer_scoped_staging_bit);
}

test "sector image: every field at its offset, 152 fixed bytes, checksum over the whole sector with [120,128) zeroed; a flat image leaves appender_dir zero" {
    const sb = testFixture();
    var img: [sector_len]u8 = undefined;
    try sb.encodeSector(0, &img, null);
    try testing.expectEqualSlices(u8, "METALV01", img[0..8]);
    try testing.expectEqual(@as(u32, 3), std.mem.readInt(u32, img[8..12], .little));
    try testing.expectEqual(@as(u32, 65536), std.mem.readInt(u32, img[12..16], .little));
    try testing.expectEqual(@as(u64, 0x2FFD7), std.mem.readInt(u64, img[16..24], .little));
    try testing.expectEqual(@as(u64, 4096), std.mem.readInt(u64, img[32..40], .little));
    try testing.expectEqual(@as(u64, 131072), std.mem.readInt(u64, img[40..48], .little));
    try testing.expectEqual(@as(u64, 1245184), std.mem.readInt(u64, img[80..88], .little));
    try testing.expectEqualSlices(u8, "kv-backend-test!", img[96..112]);
    try testing.expectEqual(@as(u64, 0x5EED_CAFE_F00D_D00D), std.mem.readInt(u64, img[112..120], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, img[128..136], .little));
    try testing.expectEqual(@as(u64, 1245184 + 2 * 65536), std.mem.readInt(u64, img[136..144], .little));
    try testing.expect(std.mem.allEqual(u8, img[152..], 0));
    // The checksum recipe.
    var copy = img;
    @memset(copy[120..128], 0);
    try testing.expectEqual(checksum.xxh3(&copy), std.mem.readInt(u64, img[120..128], .little));
    const back = try SuperblockV3.decodeSector(&img, null);
    try testing.expectEqual(sb, back);
    try testing.expectEqual(@as(u64, 1004), back.totalExtents());
    try testing.expectEqual(@as(u64, 256), back.journalPages());
    try testing.expect(back.symmetricForestStamped());
    try testing.expect(back.multiWriterClass());
    try testing.expect(!back.blockMapTreeStamped());
    // Flat: appender_dir absent ⇒ bytes [136,152) zero, bit 17 absent.
    var flat = sb;
    flat.features_incompat = 0xD7;
    flat.appender_dir = .none;
    var flat_img: [sector_len]u8 = undefined;
    try flat.encodeSector(0, &flat_img, null);
    try testing.expect(std.mem.allEqual(u8, flat_img[136..152], 0));
    // Generation lands at 128 and is covered by the checksum.
    var gen_img: [sector_len]u8 = undefined;
    try sb.encodeSector(7, &gen_img, null);
    try testing.expectEqual(@as(u64, 7), sectorGeneration(&gen_img));
    try testing.expect(!std.mem.eql(u8, img[120..128], gen_img[120..128]));
    try testing.expectEqual(@as(u64, 0), sectorGeneration(img[0..100]));
}

test "decoder check order and verbatim refusals: length, magic, version, checksum, KV_V3, watermark, routing width, unknown bits (ascending, hex), appender dir, geometry" {
    const sb = testFixture();
    var img: [sector_len]u8 = undefined;
    try sb.encodeSector(0, &img, null);
    var diag: Refusal = undefined;
    var buf: [512]u8 = undefined;

    try testing.expectError(error.BadLength, SuperblockV3.decodeSector(img[0..4000], &diag));
    try testing.expectEqualStrings("v3 superblock sector must be 4096 bytes, got 4000", msg(&buf, diag));

    var bad = img;
    bad[0] = 'X';
    try testing.expectError(error.BadMagic, SuperblockV3.decodeSector(&bad, &diag));
    try testing.expectEqualStrings("bad v3 superblock magic (corrupted or foreign volume)", msg(&buf, diag));

    bad = img;
    std.mem.writeInt(u32, bad[8..12], 2, .little);
    try testing.expectError(error.WrongVersion, SuperblockV3.decodeSector(&bad, &diag));
    try testing.expectEqualStrings("v3 decoder handed superblock version 2 (classification bug)", msg(&buf, diag));

    // A byte flipped at offset 900 — deep in the padding — fails the checksum.
    bad = img;
    bad[900] ^= 1;
    try testing.expectError(error.ChecksumMismatch, SuperblockV3.decodeSector(&bad, &diag));
    try testing.expect(std.mem.startsWith(u8, msg(&buf, diag), "superblock checksum mismatch: stored 0x"));
    try testing.expect(std.mem.endsWith(u8, msg(&buf, diag), " — corrupted superblock"));
    try testing.expectEqual(@as(usize, "superblock checksum mismatch: stored 0x0000000000000000, computed 0x0000000000000000 — corrupted superblock".len), msg(&buf, diag).len);

    const Forge = struct {
        fn with(base: SuperblockV3, incompat: u64, out: *[sector_len]u8) void {
            var s = base;
            s.features_incompat = incompat;
            // Forge past the encoder's own gate: lay the fields, then stamp.
            s.encodeSector(0, out, null) catch unreachable;
        }
    };
    Forge.with(sb, 0x2FFD7 & ~kv_v3, &bad);
    try testing.expectError(error.MissingKvV3, SuperblockV3.decodeSector(&bad, &diag));
    try testing.expectEqualStrings("v3 superblock without the KV_V3 incompat bit (corrupt feature field)", msg(&buf, diag));

    Forge.with(sb, 0x2FFD7 & ~node_seq_watermark, &bad);
    try testing.expectError(error.PreWatermark, SuperblockV3.decodeSector(&bad, &diag));
    try testing.expect(std.mem.indexOf(u8, msg(&buf, diag), "watermark") != null);
    try testing.expect(std.mem.indexOf(u8, msg(&buf, diag), "reformat required") != null);

    Forge.with(sb, 0x2FFD7 & ~kv_dynamic_routing, &bad);
    try testing.expectError(error.FrozenRoutingWidth, SuperblockV3.decodeSector(&bad, &diag));
    try testing.expect(std.mem.startsWith(u8, msg(&buf, diag), "v3 volume formatted with a frozen routing width"));
    // An OLD mask that did not know bit 6 never demanded it.
    _ = try SuperblockV3.decodeSectorWithKnown(&bad, features_incompat_known & ~kv_dynamic_routing, &diag);

    Forge.with(sb, 0x2FFD7 | (1 << 18) | (1 << 40), &bad);
    try testing.expectError(error.UnknownIncompatBits, SuperblockV3.decodeSector(&bad, &diag));
    try testing.expectEqualStrings("unknown incompatible feature bits on v3 superblock: bit 18, bit 40 (0x10000040000) — upgrade squeezefs to mount this volume", msg(&buf, diag));
    // The old-binary refusal for a NEWER bit: a mask without 17 names bit 17.
    Forge.with(sb, 0x2FFD7, &bad);
    try testing.expectError(error.UnknownIncompatBits, SuperblockV3.decodeSectorWithKnown(&bad, features_incompat_known & ~kv_symmetric_forest, &diag));
    try testing.expectEqualStrings("unknown incompatible feature bits on v3 superblock: bit 17 (0x20000) — upgrade squeezefs to mount this volume", msg(&buf, diag));

    // Appender dir named without bit 17 (dir forged in after the gate).
    Forge.with(sb, 0xFFD7, &bad);
    try testing.expectError(error.AppenderDirWithoutBit17, SuperblockV3.decodeSector(&bad, &diag));
    try testing.expect(std.mem.startsWith(u8, msg(&buf, diag), "v3 superblock names an appender directory but carries no incompat bit 17"));

    // Unknown ro bits pass; `unknownRo` reports them.
    var ro = sb;
    ro.features_ro = 1 << 3;
    try ro.encodeSector(0, &bad, null);
    const dec = try SuperblockV3.decodeSector(&bad, null);
    try testing.expectEqual(@as(u64, 8), dec.unknownRo());
    try testing.expectEqual(@as(u64, 0), dec.unknownIncompat());
}

test "validate_geometry: every refusal with its text; both legal triple placements accepted" {
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
    try testing.expectEqualStrings("v3 superblock geometry: root ledger at 0 overlaps sector 0", msg(&buf, diag));

    s = sb;
    s.root_ledger.len = 4096;
    try testing.expectError(error.LedgerLength, s.validateGeometry(&diag));
    try testing.expectEqualStrings("v3 superblock geometry: root ledger length 4096 != the fixed 131072", msg(&buf, diag));

    s = sb;
    s.journal.len = 4000;
    try testing.expectError(error.JournalPlacement, s.validateGeometry(&diag));
    try testing.expectEqualStrings("v3 superblock geometry: journal [135168, +4000) must follow the ledger in whole 4 KiB pages", msg(&buf, diag));
    s.journal = .{ .start = 4096, .len = 8192 };
    try testing.expectError(error.JournalPlacement, s.validateGeometry(&diag));

    s = sb;
    s.alloc_bitmap.start = s.journal.end() - 4096;
    try testing.expectError(error.BitmapOverlapsJournal, s.validateGeometry(&diag));
    try testing.expectEqualStrings("v3 superblock geometry: alloc bitmap at 1179648 overlaps the journal", msg(&buf, diag));

    s = sb;
    s.heap.len = 1004 * 65536 + 4096;
    try testing.expectError(error.HeapShape, s.validateGeometry(&diag));
    try testing.expectEqualStrings("v3 superblock geometry: heap [1245184, +65802240) must be whole 65536-byte extents on a 4096-byte boundary", msg(&buf, diag));

    // Triple neither below nor within: heap starting inside the bitmap.
    s = sb;
    s.heap.start = 1183744 + 4096;
    s.appender_dir = .none;
    try testing.expectError(error.TriplePlacement, s.validateGeometry(&diag));
    try testing.expectEqualStrings("v3 superblock geometry: fixed structures (ledger/journal/bitmap ending at 1191936) must sit either entirely below heap.start 1187840 (fresh format) or entirely within the heap [1187840, 66985984) on reserved extents (migrated volume) — got neither", msg(&buf, diag));

    // Triple WITHIN the heap (migrated volume): heap spans from 0.
    s = sb;
    s.heap = .{ .start = 0, .len = 1024 * 65536 };
    s.appender_dir = .none;
    try s.validateGeometry(&diag);

    s = sb;
    s.alloc_bitmap.len = 4096;
    try testing.expectError(error.BitmapTooShort, s.validateGeometry(&diag));
    try testing.expectEqualStrings("v3 superblock geometry: alloc bitmap length 4096 cannot cover 1004 extents", msg(&buf, diag));

    s = sb;
    s.heap.len = 11 * 65536;
    s.appender_dir = .none;
    try testing.expectError(error.HeapBelowReserve, s.validateGeometry(&diag));
    try testing.expectEqualStrings("v3 superblock geometry: heap of 11 extents cannot hold the compaction reserve (≥ 12 required)", msg(&buf, diag));

    s = sb;
    s.appender_dir = .{ .start = sb.heap.start + 4096, .len = 65536 };
    try testing.expectError(error.AppenderDirShape, s.validateGeometry(&diag));
    try testing.expectEqualStrings("v3 superblock geometry: appender directory [1249280, 1314816) is not one heap extent of 65536 bytes", msg(&buf, diag));
    s.appender_dir = .{ .start = sb.heap.start, .len = 4096 };
    try testing.expectError(error.AppenderDirShape, s.validateGeometry(&diag));
    s.appender_dir = .{ .start = sb.heap.end(), .len = 65536 };
    try testing.expectError(error.AppenderDirShape, s.validateGeometry(&diag));
}

test "classify_sector0: Blank only for all-zero magic; foreign magic; v2 legacy; v3; future version named BEFORE any checksum check" {
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
    try testing.expectEqualStrings("invalid superblock magic [69, 88, 84, 52, 70, 83, 33, 33] (expected [77, 69, 84, 65, 76, 86, 48, 49]) — corrupted or foreign volume", msg(&buf, diag));

    var v2 = zero;
    @memcpy(v2[0..8], "METALV01");
    std.mem.writeInt(u32, v2[8..12], 2, .little);
    try testing.expectEqual(VolumeFormat.v2_legacy, try classifySector0(&v2, &diag));
    std.mem.writeInt(u32, v2[8..12], 0, .little);
    try testing.expectEqual(VolumeFormat.v2_legacy, try classifySector0(&v2, &diag));

    const v3 = try classifySector0(&img, &diag);
    try testing.expectEqual(sb, v3.v3);

    // Version 99 with a now-wrong checksum: "upgrade", never "checksum".
    var future = img;
    std.mem.writeInt(u32, future[8..12], 99, .little);
    try testing.expectError(error.UnsupportedVersion, classifySector0(&future, &diag));
    const m = msg(&buf, diag);
    try testing.expectEqualStrings("unsupported metadata format version 99 (this binary supports <= 3) — upgrade squeezefs", m);
    try testing.expect(std.mem.indexOf(u8, m, "checksum") == null);

    // A torn v3 primary classifies as a checksum error.
    var torn = img;
    torn[900] ^= 1;
    try testing.expectError(error.ChecksumMismatch, classifySector0(&torn, &diag));

    var mount_buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&mount_buf);
    try (VolumeFormat{ .blank = {} }).writeMountRefusal(&w, "/dev/meta0");
    try testing.expectEqualStrings("/dev/meta0 is not formatted (zeroed superblock) — run `squeezefs format` first", w.buffered());
    w = .fixed(&mount_buf);
    try (VolumeFormat{ .v2_legacy = {} }).writeMountRefusal(&w, "/dev/meta0");
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "no longer supported") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "v2") != null);
}

test "DUR-5: backup at the last aligned sector; the slot needs the heap to end before it; generation = max(valid gens) + 1" {
    try testing.expectEqual(@as(?u64, 67104768), backupOffset(64 << 20));
    try testing.expectEqual(@as(?u64, 268431360), backupOffset(256 << 20));
    try testing.expectEqual(@as(?u64, 8192), backupOffset(12288 + 100));
    try testing.expectEqual(@as(?u64, 4096), backupOffset(8192));
    try testing.expectEqual(@as(?u64, null), backupOffset(8191));
    try testing.expectEqual(@as(?u64, null), backupOffset(0));
    const sb = testFixture();
    try testing.expectEqual(@as(?u64, 67104768), backupSlotFor(sb, 64 << 20));
    // A heap running to the tail (pre-DUR-5): no slot.
    var to_tail = sb;
    to_tail.heap.len = 1005 * 65536;
    try testing.expectEqual(@as(?u64, null), backupSlotFor(to_tail, 64 << 20));

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

test "loader choice: a classifying primary (even Blank/V2) is authoritative; the backup is consulted only on a primary ERROR, accepted only as a slot-reserving V3; else the primary's error stands" {
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
    try testing.expectEqual(sb, rec.format.v3);

    // Both torn ⇒ the PRIMARY's checksum error.
    var torn_b = b;
    torn_b[900] ^= 1;
    try testing.expectError(error.ChecksumMismatch, classifyVolumeImages(&torn, &torn_b, len, &diag));
    try testing.expect(diag == .checksum_mismatch);
    // No tail at all, or a blank tail ⇒ the primary's error.
    try testing.expectError(error.ChecksumMismatch, classifyVolumeImages(&torn, null, len, &diag));
    try testing.expectError(error.ChecksumMismatch, classifyVolumeImages(&torn, &zero, len, &diag));
    // A tail whose geometry does not reserve the slot it came from is refused.
    var to_tail = sb;
    to_tail.heap.len = 1005 * 65536;
    var b2: [sector_len]u8 = undefined;
    try to_tail.encodeSector(1, &b2, null);
    try testing.expectError(error.ChecksumMismatch, classifyVolumeImages(&torn, &b2, len, &diag));
    // Version 99 on BOTH copies ⇒ "upgrade", not "checksum".
    var f1 = p;
    var f2 = b;
    std.mem.writeInt(u32, f1[8..12], 99, .little);
    std.mem.writeInt(u32, f2[8..12], 99, .little);
    try testing.expectError(error.UnsupportedVersion, classifyVolumeImages(&f1, &f2, len, &diag));

    var nbuf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&nbuf);
    try writeBackupMountNotice(&w, "/dev/meta0", .{ .checksum_mismatch = .{ .stored = 1, .computed = 2 } }, 67104768, 1);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "/dev/meta0: sector 0 is unreadable (superblock checksum mismatch: stored 0x0000000000000001, computed 0x0000000000000002 — corrupted superblock) — mounting from the redundant superblock copy at offset 67104768 (generation 1)."));
    w = .fixed(&nbuf);
    try writeRepairNotice(&w, "/dev/meta0", 2);
    try testing.expectEqualStrings("/dev/meta0: repaired a damaged sector 0 from the redundant superblock copy (generation 2)", w.buffered());
}

test "write plan: backup → barrier → sector 0 → barrier, one image at one generation; no backup slot ⇒ sector 0 alone and the warning; repair = sector 0 alone" {
    const sb = testFixture();
    const plan = try planWrite(sb, 64 << 20, 9, null);
    try testing.expectEqual(@as(usize, 2), plan.slice().len);
    try testing.expectEqual(@as(u64, 67104768), plan.slice()[0].offset);
    try testing.expect(plan.slice()[0].barrier_after);
    try testing.expectEqual(@as(u64, 0), plan.slice()[1].offset);
    try testing.expect(plan.slice()[1].barrier_after);
    try testing.expect(!plan.backup_missing);
    try testing.expectEqual(@as(u64, 9), sectorGeneration(plan.bytes()));
    try testing.expectEqual(sb, (try classifySector0(plan.bytes(), null)).v3);

    var to_tail = sb;
    to_tail.heap.len = 1005 * 65536;
    const bare = try planWrite(to_tail, 64 << 20, 1, null);
    try testing.expectEqual(@as(usize, 1), bare.slice().len);
    try testing.expectEqual(@as(u64, 0), bare.slice()[0].offset);
    try testing.expect(bare.backup_missing);
    var buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try WritePlan.writeNoBackupWarning(&w, "/dev/meta0");
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "/dev/meta0: no room for the redundant superblock copy"));

    const repair = try planRepairPrimary(sb, 4, null);
    try testing.expectEqual(@as(usize, 1), repair.slice().len);
    try testing.expectEqual(@as(u64, 4), sectorGeneration(repair.bytes()));

    // An invalid geometry never encodes.
    var bad = sb;
    bad.heap.len = 11 * 65536;
    var diag: Refusal = undefined;
    try testing.expectError(error.HeapBelowReserve, planWrite(bad, 64 << 20, 1, &diag));
}

test "stampers: a set bit is a no-op; bit 17 rides with its directory and refuses a re-point" {
    var sb = testFixture();
    try testing.expect(!sb.stampBit(kv_block_refcounts));
    sb.features_incompat &= ~kv_block_map_tree;
    try testing.expect(sb.stampBit(kv_block_map_tree));
    try testing.expect(sb.blockMapTreeStamped());
    var diag: Refusal = undefined;
    const dir = sb.appender_dir;
    try testing.expect(!try sb.setSymmetricForest(dir, &diag));
    try testing.expectError(error.SymmetricForestRepoint, sb.setSymmetricForest(.{ .start = dir.start + 65536, .len = 65536 }, &diag));
    var buf: [256]u8 = undefined;
    try testing.expectEqualStrings("bit 17 is already stamped with appender directory 0x150000+65536 — refusing to re-point it at 0x160000+65536", msg(&buf, diag));
    var flat = testFixture();
    flat.features_incompat = 0xD7;
    flat.appender_dir = .none;
    try testing.expect(try flat.setSymmetricForest(dir, &diag));
    try testing.expect(flat.symmetricForestStamped());
    try testing.expectEqual(dir, flat.appender_dir);
    try flat.validateGeometry(null);
}
