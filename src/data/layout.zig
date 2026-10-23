//! The v4 layout record (charter §6.1 layouts; §5.6 layout deltas + publish
//! coalescing; `docs/porting/data_layouts.md` §1.7, §2, §6; decision 0002
//! rows 2–3 and the rung-4 rows). PORTABLE: codecs, derivations, the
//! publish staging over a `tx.KvTx`, the seams the data plane binds.
//!
//! The xattr `layout` VALUE is `LayoutHeaderWire ‖ body`, `value.len == 32 +
//! header.body_len` EXACTLY; every reserved byte is 0 and checked; exactly
//! the encoder's form decodes (§12 "canonical total codecs"). Four kinds —
//! `inline` (the payload IS the record), `staged` (a ring entry keyed by
//! `file_id`, optionally with the promoted/spilled durable copy), `packed`
//! (a tenant window of a shared block — its OWN kind, never "staged plus a
//! decorated block 0"), `striped` (an inline map up to the record cap, or the
//! block-map tree head as typed fields). The tenant binding is ONE 40 B
//! binary `MappingWire` (the string grammar `[be://]off[@inc]:rel:len` and
//! the `damaged:` prefix are gone — 0002 row 2's class); its window law is
//! checked at ONE decode and re-asserted at the ONE read choke point
//! (`Mapping.readWindow`), so a malformed binding can never resolve to another
//! tenant's bytes.
//!
//! The layout DELTA is the ONE `Delta(field_mask, fields)` kind over a field
//! table DERIVED from the wire struct at comptime (like `record.inode_field_
//! table`): bit `i` names row `i`, the Put lays the same rows in the same
//! order, the KEY (xattr kind 03, name `layout`) discriminates the class —
//! never a payload magic (0002 row 3). The version pair (`layout_version`,
//! `base_version`) rides the record header, so the commit gate reads a field.
//!
//! The DMA-before-type-flip law is the API's SHAPE: `publishLayout` takes the
//! `DmaProof`s the DMA completion minted and refuses a layout naming a block
//! no proof covers; the `+ref`/`−ref` records ride the SAME tx through the
//! `RefsSeam` (`data/block_refs.zig` binds it).

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const fuzzkit = @import("../core/fuzzkit.zig");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const key = @import("../meta/key.zig");
const record = @import("../meta/record.zig");
const tx = @import("../meta/tx.zig");
const sync_mutex = @import("../sync/mutex.zig");
const lock_order = @import("../sync/lock_order.zig");

// ---------------------------------------------------------------------------
// Geometry: the two grains every mapping law is written in.
// ---------------------------------------------------------------------------

/// The LBA grain: the O_DIRECT device fd refuses windows that are not
/// multiples of the logical block, and 4096 is the one size every NVMe
/// namespace this product targets formats at (`nvme_derive`'s probe refuses
/// a device below it). No 512-native probe until `pack_slot_pad_bytes` shows
/// the waste is worth it.
pub const GRAIN: u32 = 4096;
/// The striped block = the allocator chunk = the stored-image ceiling: the
/// format's default `CHUNK_SIZE` (a superblock fact once a set is formatted,
/// `const` on the router after open — never a knob).
pub const DEFAULT_CHUNK: u64 = 4 * derive.MiB;
/// The xattr name the layout record lives under.
pub const LAYOUT_XATTR_NAME: []const u8 = "layout";
/// Forward-only: any other value refuses.
pub const LAYOUT_RECORD_VERSION: u8 = 1;

/// The format facts a decode checks against.
pub const Geometry = struct {
    chunk: u64 = DEFAULT_CHUNK,
    /// The inline ceiling THIS set can hold (`inlineMaxCeiling(node_size)`):
    /// a larger `data_len` is a value the format cannot hold.
    inline_ceiling: u64 = inlineMaxCeiling(256 * derive.KiB),

    pub const default: Geometry = .{};
};

pub const stats = struct {
    /// `layout_inline_writes`: inline layouts published.
    pub var layout_inline_writes: counters.ShardedCounter = .{};
    /// `layout_staged_writes`: staged layouts published.
    pub var layout_staged_writes: counters.ShardedCounter = .{};
    /// `layout_striped_writes`: striped layouts published.
    pub var layout_striped_writes: counters.ShardedCounter = .{};
    /// `layout_publish_batches`: coalesced publish windows flushed.
    pub var layout_publish_batches: counters.ShardedCounter = .{};
    /// `layout_publish_batched_blocks`: block inserts those windows carried.
    pub var layout_publish_batched_blocks: counters.ShardedCounter = .{};
    /// `layout_delta_rebases`: a publish that found its chain at the cap
    /// and re-based with a full Put.
    pub var layout_delta_rebases: counters.ShardedCounter = .{};
    /// `inline_max_bytes`: the inline ceiling in force (published).
    pub var inline_max_bytes: counters.Gauge = .{};
    /// `packed_mapping_refusals`: the read choke point refused a mapping
    /// (a DAMAGED quarantine marker, or a window law the decoder already
    /// enforces — reachable only through corruption). Must stay 0.
    pub var packed_mapping_refusals: counters.Tripwire = .init("packed_mapping_refusals");
    /// `unwritten_mapping_served`: an UNWRITTEN binding (a fallocate
    /// reservation) reached the read choke point — every resolve seam
    /// hides one as a hole, so this is a missed seam. Must stay 0.
    pub var unwritten_mapping_served: counters.Tripwire = .init("unwritten_mapping_served");
};

/// `.stats`-ready JSON object of this module's instruments.
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"layout_inline_writes\":{d},\"layout_staged_writes\":{d},\"layout_striped_writes\":{d},\"layout_publish_batches\":{d},\"layout_publish_batched_blocks\":{d},\"layout_delta_rebases\":{d},\"inline_max_bytes\":{d},\"packed_mapping_refusals\":{d},\"unwritten_mapping_served\":{d}}}", .{
        stats.layout_inline_writes.load(),
        stats.layout_staged_writes.load(),
        stats.layout_striped_writes.load(),
        stats.layout_publish_batches.load(),
        stats.layout_publish_batched_blocks.load(),
        stats.layout_delta_rebases.load(),
        stats.inline_max_bytes.load(),
        stats.packed_mapping_refusals.load(),
        stats.unwritten_mapping_served.load(),
    });
}

// ---------------------------------------------------------------------------
// §1.7 The wire structs.
// ---------------------------------------------------------------------------

/// The layout discriminator (the `file_type` string retired).
pub const Kind = enum(u8) {
    inline_ = 0,
    staged = 1,
    packed_ = 2,
    striped = 3,

    pub fn fromByte(b: u8) DecodeError!Kind {
        return switch (b) {
            0 => .inline_,
            1 => .staged,
            2 => .packed_,
            3 => .striped,
            else => error.Corrupt,
        };
    }
};

/// 32 B, little-endian.
pub const LayoutHeaderWire = extern struct {
    layout_kind: u8,
    version: u8,
    flags_le: [2]u8,
    body_len_le: [4]u8,
    size_le: [8]u8,
    /// §6.2-item-9 link version of THIS record (0 = unversioned); a Put IS
    /// a base.
    layout_version_le: [8]u8,
    /// The claim (Delta only; a Put carries 0, checked).
    base_version_le: [8]u8,
};

pub const LAYOUT_HEADER_LEN: usize = @sizeOf(LayoutHeaderWire);

/// 40 B — the ONE binary binding.
pub const MappingWire = extern struct {
    /// The `vol-{16 hex}` decoded verbatim — never an ordinal or an alias.
    vol_tag_le: [8]u8,
    /// Device byte offset, CHUNK-aligned (checked).
    offset_le: [8]u8,
    /// 0 = none.
    incarnation_le: [8]u8,
    /// `% GRAIN == 0` (checked).
    rel_off_le: [4]u8,
    /// Exact stored-image length; `rel_off + ceil(len) ≤ CHUNK` (checked);
    /// 0 under WHOLE_BLOCK.
    len_le: [4]u8,
    flags: u8,
    reserved: [7]u8,
};

pub const MAPPING_WIRE_LEN: usize = @sizeOf(MappingWire);

/// 48 B: one inline-map entry.
pub const MapEntryWire = extern struct {
    block_le: [4]u8,
    reserved_le: [4]u8,
    mapping: MappingWire,

    pub fn block(self: *const MapEntryWire) u32 {
        return std.mem.readInt(u32, &self.block_le, .little);
    }
};

pub const MAP_ENTRY_WIRE_LEN: usize = @sizeOf(MapEntryWire);

comptime {
    assert(LAYOUT_HEADER_LEN == 32);
    assert(@offsetOf(LayoutHeaderWire, "flags_le") == 2);
    assert(@offsetOf(LayoutHeaderWire, "body_len_le") == 4);
    assert(@offsetOf(LayoutHeaderWire, "size_le") == 8);
    assert(@offsetOf(LayoutHeaderWire, "layout_version_le") == 16);
    assert(@offsetOf(LayoutHeaderWire, "base_version_le") == 24);
    assert(MAPPING_WIRE_LEN == 40);
    assert(@offsetOf(MappingWire, "rel_off_le") == 24);
    assert(@offsetOf(MappingWire, "len_le") == 28);
    assert(@offsetOf(MappingWire, "flags") == 32);
    assert(MAP_ENTRY_WIRE_LEN == 48);
    // Byte-array fields only: the wire structs are alignment-1 so a decoded
    // entry table is a zero-copy view of the record bytes.
    assert(@alignOf(MapEntryWire) == 1);
}

/// Header flag bits. Others = 0 (checked).
pub const F_HAS_FILE_ID: u16 = 1 << 0;
/// staged: a promoted/spilled `MappingWire` follows the file id.
pub const F_HAS_DURABLE_COPY: u16 = 1 << 1;
pub const F_MAP_INLINE: u16 = 1 << 2;
pub const F_MAP_KVMAP: u16 = 1 << 3;
pub const F_SWEEP_CURSOR: u16 = 1 << 4;
pub const F_MAP_GEN: u16 = 1 << 5;
const F_KNOWN: u16 = F_HAS_FILE_ID | F_HAS_DURABLE_COPY | F_MAP_INLINE | F_MAP_KVMAP | F_SWEEP_CURSOR | F_MAP_GEN;

/// Mapping flag bits. Others = 0 (checked).
/// The undecorated whole-block binding (`rel_off = len = 0`).
pub const M_WHOLE_BLOCK: u8 = 1 << 0;
/// fsck's quarantine marker: decodes in a record, refuses at every read.
pub const M_DAMAGED: u8 = 1 << 1;
/// A RESERVED block (charter §6.6, decision 0003 §BH): allocated to the
/// file — a durable reference like any other, charged to the volume —
/// whose bytes are NOT the file's: every read serves zeros, every write
/// arm sees a hole, and the write that lands in it writes the WHOLE block
/// and flips the flag (the same reference: no `±ref`, no proof — the DMA
/// law exists so a type flip cannot precede its bytes, and an unwritten
/// binding names none). Whole-block only, never DAMAGED.
pub const M_UNWRITTEN: u8 = 1 << 2;
const M_KNOWN: u8 = M_WHOLE_BLOCK | M_DAMAGED | M_UNWRITTEN;

/// The 16-byte file id of a staged/packed layout (the ring key's identity
/// half; 0002 row: 16 bytes, not the uuid's 36 chars).
pub const FileId = [16]u8;
pub const zero_file_id: FileId = @splat(0);

// ---------------------------------------------------------------------------
// Errors — typed classes; every refusal is one of these.
// ---------------------------------------------------------------------------

/// Structural corruption: bytes no encoder of this layer produces.
pub const DecodeError = error{Corrupt};

pub const MappingError = error{
    /// The window law failed: `offset % CHUNK`, `rel_off % GRAIN`, an empty
    /// tenant, `rel_off + ceil(len) > CHUNK`, a WHOLE_BLOCK with a window,
    /// an unknown flag or a non-zero reserved byte.
    MappingRefused,
    /// fsck's quarantine marker: the bytes are not to be served (EIO).
    MappingDamaged,
};

pub const LayoutError = DecodeError || MappingError || error{
    /// `data_len` above the format's inline ceiling.
    InlineOverCeiling,
    /// Inline-map entries not strictly ascending by block.
    MapNotAscending,
};

pub const FoldError = LayoutError || Allocator.Error || error{
    /// A delta with map inserts applied onto a layout whose map is not an
    /// inline map (a kvmap head, or no map at all).
    DeltaOntoNonInlineMap,
    /// The folded layout fails `validate` (a delta set flags its body
    /// cannot carry).
    DeltaInconsistent,
    /// A versioned link above unversioned ones or vice versa: links of one
    /// base segment are homogeneous.
    LayoutChainHeterogeneous,
    /// A link's `base_version` is not the previous link's `version`: a fork
    /// two writers staged against one base — refused loud, never folded
    /// into a layout neither computed.
    LayoutChainDivergent,
};

// ---------------------------------------------------------------------------
// The mapping — the one binding and its law.
// ---------------------------------------------------------------------------

/// `ceil(len)` to the LBA grain; the pack slot of a stored image. A zero
/// image still occupies one grain (`pack_slot_len(1) = 4096`).
pub fn ceilGrain(len: u64) u64 {
    return @max(derive.alignUp(len, GRAIN), GRAIN);
}

pub const Range = struct { start: u64, end: u64 };

/// What a read resolves a mapping to: `[device_off, device_off + len)` on
/// `vol_tag`, of which `[lo, hi)` are the caller's bytes.
pub const Window = struct {
    vol_tag: u64,
    device_off: u64,
    len: u64,
    lo: usize,
    hi: usize,
};

pub const Mapping = struct {
    vol_tag: u64 = 0,
    offset: u64 = 0,
    incarnation: u64 = 0,
    rel_off: u32 = 0,
    len: u32 = 0,
    whole_block: bool = false,
    damaged: bool = false,
    /// `M_UNWRITTEN`: a reserved block whose bytes are not the file's.
    unwritten: bool = false,

    pub fn wholeBlock(vol_tag: u64, offset: u64, incarnation: u64) Mapping {
        return .{ .vol_tag = vol_tag, .offset = offset, .incarnation = incarnation, .whole_block = true };
    }

    /// A fallocate reservation: the whole block allocated, its bytes not
    /// the file's.
    pub fn unwrittenBlock(vol_tag: u64, offset: u64, incarnation: u64) Mapping {
        return .{ .vol_tag = vol_tag, .offset = offset, .incarnation = incarnation, .whole_block = true, .unwritten = true };
    }

    /// The same reference with the UNWRITTEN flag set (ZERO_RANGE's flip)
    /// or cleared (the landing that wrote the whole block).
    pub fn withUnwritten(self: Mapping, unwritten: bool) Mapping {
        var m = self;
        m.unwritten = unwritten;
        return m;
    }

    pub fn tenant(vol_tag: u64, offset: u64, incarnation: u64, rel_off: u32, len: u32) Mapping {
        return .{ .vol_tag = vol_tag, .offset = offset, .incarnation = incarnation, .rel_off = rel_off, .len = len };
    }

    /// The window law (the ONE check both the decoder and the read run).
    pub fn check(self: Mapping, chunk: u64) MappingError!void {
        // A modulus, not the power-of-two helper: a transformed volume's
        // chunk is `block + 4 KiB` (`transform.chunkSizeFor`), and the
        // helper's assertion panicked the first striped publish there.
        if (chunk == 0 or self.offset % chunk != 0) return error.MappingRefused;
        // UNWRITTEN is the whole-block reservation alone: a tenant window
        // is a stored image, and a quarantine of bytes nobody reads is
        // meaningless.
        if (self.unwritten and (!self.whole_block or self.damaged)) return error.MappingRefused;
        if (self.whole_block) {
            if (self.rel_off != 0 or self.len != 0) return error.MappingRefused;
            return;
        }
        if (self.len == 0) return error.MappingRefused;
        if (!derive.isAligned(self.rel_off, GRAIN)) return error.MappingRefused;
        if (@as(u64, self.rel_off) + ceilGrain(self.len) > chunk) return error.MappingRefused;
    }

    /// The block this mapping references (the refs key's `block_idx`).
    pub fn blockIdx(self: Mapping, chunk: u64) u64 {
        return self.offset / chunk;
    }

    /// The tenant's slot: `ceil(len)`; a whole block is the chunk.
    pub fn slotLen(self: Mapping, chunk: u64) u64 {
        return if (self.whole_block) chunk else ceilGrain(self.len);
    }

    /// Two mappings name the same REFERENCE (the refs key ignores the
    /// window and the stamp): same volume, same block.
    pub fn sameRef(a: Mapping, b: Mapping, chunk: u64) bool {
        return a.vol_tag == b.vol_tag and a.blockIdx(chunk) == b.blockIdx(chunk);
    }

    pub fn eql(a: Mapping, b: Mapping) bool {
        return std.meta.eql(a, b);
    }

    /// The passthrough truncate-shrink: a pure clip of the exact length,
    /// same base, same window start — zero device writes, the same
    /// reference (`sameRef`), so the refs population is unchanged.
    pub fn clip(self: Mapping, new_len: u32) Mapping {
        assert(!self.whole_block and new_len >= 1 and new_len <= self.len);
        var m = self;
        m.len = new_len;
        return m;
    }

    pub fn encode(self: Mapping) MappingWire {
        var w: MappingWire = undefined;
        std.mem.writeInt(u64, &w.vol_tag_le, self.vol_tag, .little);
        std.mem.writeInt(u64, &w.offset_le, self.offset, .little);
        std.mem.writeInt(u64, &w.incarnation_le, self.incarnation, .little);
        std.mem.writeInt(u32, &w.rel_off_le, self.rel_off, .little);
        std.mem.writeInt(u32, &w.len_le, self.len, .little);
        w.flags = (if (self.whole_block) M_WHOLE_BLOCK else 0) | (if (self.damaged) M_DAMAGED else @as(u8, 0)) | (if (self.unwritten) M_UNWRITTEN else @as(u8, 0));
        w.reserved = @splat(0);
        return w;
    }

    /// Structural + the window law; a DAMAGED marker decodes (it is a
    /// record fsck wrote) and refuses only at `readWindow`.
    pub fn decode(w: MappingWire, chunk: u64) MappingError!Mapping {
        if (w.flags & ~M_KNOWN != 0) return error.MappingRefused;
        if (!std.mem.allEqual(u8, &w.reserved, 0)) return error.MappingRefused;
        const m: Mapping = .{
            .vol_tag = std.mem.readInt(u64, &w.vol_tag_le, .little),
            .offset = std.mem.readInt(u64, &w.offset_le, .little),
            .incarnation = std.mem.readInt(u64, &w.incarnation_le, .little),
            .rel_off = std.mem.readInt(u32, &w.rel_off_le, .little),
            .len = std.mem.readInt(u32, &w.len_le, .little),
            .whole_block = w.flags & M_WHOLE_BLOCK != 0,
            .damaged = w.flags & M_DAMAGED != 0,
            .unwritten = w.flags & M_UNWRITTEN != 0,
        };
        try m.check(chunk);
        return m;
    }

    /// THE read choke point (charter §6.1 "a malformed decoration refuses
    /// EIO at ONE parse choke point — never resolves to another tenant's
    /// bytes"). `rel` is the image span the caller wants (a passthrough
    /// sub-window), null = the whole image. The window is
    /// `[floor_GRAIN(start), ceil_GRAIN(end))` at `offset + rel_off`, read
    /// on the mapping's OWN volume — never a prefix read from the block
    /// start (FIND-PK-1). A whole block resolves to the chunk.
    pub fn readWindow(self: Mapping, chunk: u64, rel: ?Range) MappingError!Window {
        if (self.damaged) {
            stats.packed_mapping_refusals.fire("DAMAGED quarantine marker served to a read");
            return error.MappingDamaged;
        }
        // Every resolve seam hides an UNWRITTEN binding as a hole; one
        // reaching the choke point is a missed seam, refused (EIO), never
        // the block's recycled bytes.
        if (self.unwritten) {
            stats.unwritten_mapping_served.fire("an UNWRITTEN binding reached the read choke point");
            return error.MappingRefused;
        }
        self.check(chunk) catch |e| {
            stats.packed_mapping_refusals.fire("a stored mapping failed the window law at read");
            return e;
        };
        if (self.whole_block) {
            const r = rel orelse Range{ .start = 0, .end = chunk };
            const start = @min(r.start, chunk);
            const end = @min(r.end, chunk);
            const ws = derive.alignDown(start, GRAIN);
            const we = derive.alignUp(end, GRAIN);
            return .{ .vol_tag = self.vol_tag, .device_off = self.offset + ws, .len = we - ws, .lo = @intCast(start - ws), .hi = @intCast(end - ws) };
        }
        const img: u64 = self.len;
        const r = rel orelse Range{ .start = 0, .end = img };
        const start = @min(r.start, img);
        const end = @min(@max(r.end, start), img);
        const ws = derive.alignDown(start, GRAIN);
        const we = @min(derive.alignUp(end, GRAIN), ceilGrain(img));
        return .{
            .vol_tag = self.vol_tag,
            .device_off = self.offset + self.rel_off + ws,
            .len = we - ws,
            .lo = @intCast(start - ws),
            .hi = @intCast(end - ws),
        };
    }
};

// ---------------------------------------------------------------------------
// The layout — the flat RAM shape over which Put and Delta share one table.
// ---------------------------------------------------------------------------

/// Which fields a kind's body carries is DERIVED from `kind` + `flags`;
/// `validate` is the one consistency law the decoder, the encoder and the
/// delta fold all run. `durable_copy` is the staged file's promoted/spilled
/// copy AND the packed file's tenant (a tenant IS a durable copy that lives
/// in a shared block) — one field, so the delta table is total over kinds.
pub const Layout = struct {
    kind: Kind,
    flags: u16 = 0,
    size: u64 = 0,
    layout_version: u64 = 0,
    base_version: u64 = 0,
    file_id: FileId = zero_file_id,
    durable_copy: Mapping = .{},
    sweep_cursor: u32 = 0,
    map_gen: u64 = 0,
    /// The inline payload (kind `inline` only).
    data: []const u8 = &.{},
    /// The inline map, strictly ascending by block (striped + MAP_INLINE).
    entries: []const MapEntryWire = &.{},

    // ---- constructors ----

    pub fn inlineData(size: u64, data: []const u8) Layout {
        return .{ .kind = .inline_, .size = size, .data = data };
    }

    pub fn staged(size: u64, file_id: FileId, durable_copy: ?Mapping) Layout {
        return .{
            .kind = .staged,
            .flags = F_HAS_FILE_ID | (if (durable_copy != null) F_HAS_DURABLE_COPY else @as(u16, 0)),
            .size = size,
            .file_id = file_id,
            .durable_copy = durable_copy orelse .{},
        };
    }

    pub fn packedTenant(size: u64, file_id: FileId, tenant: Mapping) Layout {
        return .{ .kind = .packed_, .flags = F_HAS_FILE_ID, .size = size, .file_id = file_id, .durable_copy = tenant };
    }

    pub fn stripedInline(size: u64, entries: []const MapEntryWire) Layout {
        return .{ .kind = .striped, .flags = F_MAP_INLINE, .size = size, .entries = entries };
    }

    /// The block-map tree head: the map lives in tree 7 (`key.blockMapKey`);
    /// `sweep` is a resumed crossing's cursor, `gen` the map generation
    /// (never 0 — the encoder omits an absent generation).
    pub fn stripedKvmap(size: u64, sweep: ?u32, gen: ?u64) Layout {
        var l: Layout = .{ .kind = .striped, .flags = F_MAP_KVMAP, .size = size };
        if (sweep) |s| {
            l.flags |= F_SWEEP_CURSOR;
            l.sweep_cursor = s;
        }
        if (gen) |g| {
            l.flags |= F_MAP_GEN;
            l.map_gen = g;
        }
        return l;
    }

    // ---- accessors ----

    pub fn hasFileId(self: Layout) bool {
        return self.flags & F_HAS_FILE_ID != 0;
    }

    /// The staged file's promoted copy, the packed file's tenant; null
    /// otherwise.
    pub fn durableCopy(self: Layout) ?Mapping {
        return switch (self.kind) {
            .packed_ => self.durable_copy,
            .staged => if (self.flags & F_HAS_DURABLE_COPY != 0) self.durable_copy else null,
            else => null,
        };
    }

    pub fn isKvmap(self: Layout) bool {
        return self.kind == .striped and self.flags & F_MAP_KVMAP != 0;
    }

    pub const KvmapHead = struct { sweep: ?u32, gen: ?u64 };

    pub fn kvmapHead(self: Layout) ?KvmapHead {
        if (!self.isKvmap()) return null;
        return .{
            .sweep = if (self.flags & F_SWEEP_CURSOR != 0) self.sweep_cursor else null,
            .gen = if (self.flags & F_MAP_GEN != 0) self.map_gen else null,
        };
    }

    /// Entry `i` of the inline map decoded (the decoder checked it).
    pub fn entry(self: Layout, chunk: u64, i: usize) MapEntry {
        const e = &self.entries[i];
        return .{ .block = e.block(), .mapping = Mapping.decode(e.mapping, chunk) catch unreachable };
    }

    /// The mapping of `block` in an inline map (binary search; null = hole).
    pub fn lookup(self: Layout, chunk: u64, block: u32) ?Mapping {
        return self.lookupChecked(chunk, block) catch unreachable;
    }

    /// `lookup` checking the ONE entry it lands on as `validate` checks
    /// every entry (a `decodePoint` layout's entries are unchecked).
    fn lookupChecked(self: Layout, chunk: u64, block: u32) LayoutError!?Mapping {
        if (self.flags & F_MAP_INLINE == 0) return null;
        const idx = std.sort.lowerBound(MapEntryWire, self.entries, block, entryBlockOrder);
        if (idx == self.entries.len or self.entries[idx].block() != block) return null;
        return try checkEntry(&self.entries[idx], chunk);
    }

    fn entryBlockOrder(block: u32, e: MapEntryWire) std.math.Order {
        return std.math.order(block, e.block());
    }

    /// The references this layout NAMES: `(block_index, mapping)` — the
    /// durable copy / tenant at index 0, every inline-map entry at its
    /// block. A kvmap head's references are named ELSEWHERE (its kind-7
    /// records, `data/kvmap.zig`), never by nobody: the diff over this
    /// iterator is not the flip's arm (`PublishArm.flip_to_kvmap`).
    pub const NamedIterator = struct {
        layout: *const Layout,
        chunk: u64,
        i: usize = 0,
        done_copy: bool = false,

        pub fn next(self: *NamedIterator) ?MapEntry {
            if (!self.done_copy) {
                self.done_copy = true;
                if (self.layout.durableCopy()) |m| return .{ .block = 0, .mapping = m };
            }
            if (self.layout.flags & F_MAP_INLINE != 0 and self.i < self.layout.entries.len) {
                defer self.i += 1;
                return self.layout.entry(self.chunk, self.i);
            }
            return null;
        }

        /// The next element's block and reference identity off the wire —
        /// no decode, no window check: what a diff of two maps compares.
        pub fn peek(self: *const NamedIterator) ?RefKey {
            if (!self.done_copy) if (self.layout.durableCopy()) |m| return .{ .block = 0, .vol_tag = m.vol_tag, .offset = m.offset };
            if (self.layout.flags & F_MAP_INLINE != 0 and self.i < self.layout.entries.len) {
                const e = &self.layout.entries[self.i];
                return .{ .block = e.block(), .vol_tag = std.mem.readInt(u64, &e.mapping.vol_tag_le, .little), .offset = std.mem.readInt(u64, &e.mapping.offset_le, .little) };
            }
            return null;
        }

        /// Past the peeked element without decoding it.
        pub fn skip(self: *NamedIterator) void {
            if (!self.done_copy) {
                self.done_copy = true;
                if (self.layout.durableCopy() != null) return;
            }
            if (self.layout.flags & F_MAP_INLINE != 0 and self.i < self.layout.entries.len) self.i += 1;
        }
    };

    /// A named element's identity for the diff: its block and the
    /// reference it names — the volume and the block's offset (a checked
    /// mapping's offset is chunk-aligned, so equal offsets are one
    /// `blockIdx`: exactly `Mapping.sameRef`).
    pub const RefKey = struct {
        block: u32,
        vol_tag: u64,
        offset: u64,

        pub fn sameRef(a: RefKey, b: RefKey) bool {
            return a.vol_tag == b.vol_tag and a.offset == b.offset;
        }
    };

    pub fn named(self: *const Layout, chunk: u64) NamedIterator {
        return .{ .layout = self, .chunk = chunk };
    }

    // ---- the consistency law ----

    /// Exactly the admissible shapes: flags per kind, `base_version == 0`
    /// on a Put, the inline ceiling, every mapping's window law, the
    /// ascending map, a present generation ≠ 0, nothing carried a kind does
    /// not own.
    pub fn validate(self: Layout, geo: Geometry) LayoutError!void {
        try self.validateHead(geo);
        var prev: ?u32 = null;
        for (self.entries) |*e| {
            if (!std.mem.allEqual(u8, &e.reserved_le, 0)) return error.Corrupt;
            const b = e.block();
            if (prev) |p| if (b <= p) return error.MapNotAscending;
            prev = b;
            _ = try Mapping.decode(e.mapping, geo.chunk);
        }
    }

    /// `validate` short of the inline map's entries (O(1) in the map).
    fn validateHead(self: Layout, geo: Geometry) LayoutError!void {
        if (self.flags & ~F_KNOWN != 0) return error.Corrupt;
        const admissible: u16 = switch (self.kind) {
            .inline_ => 0,
            .staged => F_HAS_FILE_ID | F_HAS_DURABLE_COPY,
            .packed_ => F_HAS_FILE_ID,
            .striped => F_MAP_INLINE | F_MAP_KVMAP | F_SWEEP_CURSOR | F_MAP_GEN,
        };
        if (self.flags & ~admissible != 0) return error.Corrupt;
        switch (self.kind) {
            .inline_ => {
                if (self.data.len > geo.inline_ceiling) return error.InlineOverCeiling;
                if (self.data.len > std.math.maxInt(u32)) return error.InlineOverCeiling;
            },
            .staged, .packed_ => {
                if (!self.hasFileId()) return error.Corrupt;
                if (self.data.len != 0) return error.Corrupt;
            },
            .striped => {
                if (self.data.len != 0) return error.Corrupt;
                const inline_map = self.flags & F_MAP_INLINE != 0;
                const kvmap = self.flags & F_MAP_KVMAP != 0;
                if (inline_map == kvmap) return error.Corrupt;
                if (!kvmap and self.flags & (F_SWEEP_CURSOR | F_MAP_GEN) != 0) return error.Corrupt;
                if (self.flags & F_MAP_GEN != 0 and self.map_gen == 0) return error.Corrupt;
                if (self.entries.len > std.math.maxInt(u32)) return error.Corrupt;
            },
        }
        if (self.kind != .striped and self.entries.len != 0) return error.Corrupt;
        if (self.durableCopy()) |m| {
            try m.check(geo.chunk);
            // A tenant is a window, never the undecorated binding.
            if (self.kind == .packed_ and m.whole_block) return error.MappingRefused;
        }
    }

    // ---- the canonical total codec ----

    pub fn bodyLen(self: Layout) usize {
        return switch (self.kind) {
            .inline_ => 4 + self.data.len,
            .staged => 16 + @as(usize, if (self.flags & F_HAS_DURABLE_COPY != 0) MAPPING_WIRE_LEN else 0),
            .packed_ => 16 + MAPPING_WIRE_LEN,
            .striped => if (self.flags & F_MAP_INLINE != 0)
                4 + self.entries.len * MAP_ENTRY_WIRE_LEN
            else
                @as(usize, if (self.flags & F_SWEEP_CURSOR != 0) 4 else 0) + @as(usize, if (self.flags & F_MAP_GEN != 0) 8 else 0),
        };
    }

    pub fn encodedLen(self: Layout) usize {
        return LAYOUT_HEADER_LEN + self.bodyLen();
    }

    /// `header ‖ body` into `out` (≥ `encodedLen()`); the layout must
    /// `validate` (asserted — a caller bug, never a wire outcome).
    pub fn encodeInto(self: Layout, out: []u8) []u8 {
        const n = self.encodedLen();
        assert(out.len >= n);
        assert(self.base_version == 0);
        var h: LayoutHeaderWire = .{
            .layout_kind = @backingInt(self.kind),
            .version = LAYOUT_RECORD_VERSION,
            .flags_le = undefined,
            .body_len_le = undefined,
            .size_le = undefined,
            .layout_version_le = undefined,
            .base_version_le = undefined,
        };
        std.mem.writeInt(u16, &h.flags_le, self.flags, .little);
        std.mem.writeInt(u32, &h.body_len_le, @intCast(self.bodyLen()), .little);
        std.mem.writeInt(u64, &h.size_le, self.size, .little);
        std.mem.writeInt(u64, &h.layout_version_le, self.layout_version, .little);
        std.mem.writeInt(u64, &h.base_version_le, 0, .little);
        out[0..LAYOUT_HEADER_LEN].* = std.mem.toBytes(h);
        var pos: usize = LAYOUT_HEADER_LEN;
        switch (self.kind) {
            .inline_ => {
                std.mem.writeInt(u32, out[pos..][0..4], @intCast(self.data.len), .little);
                pos += 4;
                @memcpy(out[pos..][0..self.data.len], self.data);
                pos += self.data.len;
            },
            .staged, .packed_ => {
                out[pos..][0..16].* = self.file_id;
                pos += 16;
                if (self.kind == .packed_ or self.flags & F_HAS_DURABLE_COPY != 0) {
                    out[pos..][0..MAPPING_WIRE_LEN].* = std.mem.toBytes(self.durable_copy.encode());
                    pos += MAPPING_WIRE_LEN;
                }
            },
            .striped => {
                if (self.flags & F_MAP_INLINE != 0) {
                    std.mem.writeInt(u32, out[pos..][0..4], @intCast(self.entries.len), .little);
                    pos += 4;
                    const bytes = std.mem.sliceAsBytes(self.entries);
                    @memcpy(out[pos..][0..bytes.len], bytes);
                    pos += bytes.len;
                } else {
                    if (self.flags & F_SWEEP_CURSOR != 0) {
                        std.mem.writeInt(u32, out[pos..][0..4], self.sweep_cursor, .little);
                        pos += 4;
                    }
                    if (self.flags & F_MAP_GEN != 0) {
                        std.mem.writeInt(u64, out[pos..][0..8], self.map_gen, .little);
                        pos += 8;
                    }
                }
            },
        }
        assert(pos == n);
        return out[0..n];
    }

    pub fn encodeAlloc(self: Layout, gpa: Allocator) Allocator.Error![]u8 {
        const out = try gpa.alloc(u8, self.encodedLen());
        return self.encodeInto(out);
    }

    /// Total: exactly `32 + body_len` bytes, version 1, a known kind, zero
    /// `base_version`, the kind's body in the encoder's form, the layout
    /// `validate`s. `data`/`entries` borrow from `bytes`.
    pub fn decode(bytes: []const u8, geo: Geometry) LayoutError!Layout {
        const l = try decodeShape(bytes, geo);
        try l.validate(geo);
        return l;
    }

    /// `decode` short of `validate`: the header and the body's framing.
    fn decodeShape(bytes: []const u8, geo: Geometry) LayoutError!Layout {
        if (bytes.len < LAYOUT_HEADER_LEN) return error.Corrupt;
        const h = std.mem.bytesToValue(LayoutHeaderWire, bytes[0..LAYOUT_HEADER_LEN]);
        if (h.version != LAYOUT_RECORD_VERSION) return error.Corrupt;
        const kind = try Kind.fromByte(h.layout_kind);
        const body_len: usize = std.mem.readInt(u32, &h.body_len_le, .little);
        if (bytes.len - LAYOUT_HEADER_LEN != body_len) return error.Corrupt;
        if (std.mem.readInt(u64, &h.base_version_le, .little) != 0) return error.Corrupt;
        var l: Layout = .{
            .kind = kind,
            .flags = std.mem.readInt(u16, &h.flags_le, .little),
            .size = std.mem.readInt(u64, &h.size_le, .little),
            .layout_version = std.mem.readInt(u64, &h.layout_version_le, .little),
        };
        if (l.flags & ~F_KNOWN != 0) return error.Corrupt;
        const body = bytes[LAYOUT_HEADER_LEN..];
        switch (kind) {
            .inline_ => {
                if (body.len < 4) return error.Corrupt;
                const n: usize = std.mem.readInt(u32, body[0..4], .little);
                if (body.len - 4 != n) return error.Corrupt;
                l.data = body[4..];
            },
            .staged, .packed_ => {
                const with_copy = kind == .packed_ or l.flags & F_HAS_DURABLE_COPY != 0;
                const want: usize = 16 + @as(usize, if (with_copy) MAPPING_WIRE_LEN else 0);
                if (body.len != want) return error.Corrupt;
                l.file_id = body[0..16].*;
                if (with_copy) l.durable_copy = try Mapping.decode(std.mem.bytesToValue(MappingWire, body[16..][0..MAPPING_WIRE_LEN]), geo.chunk);
            },
            .striped => {
                if (l.flags & F_MAP_INLINE != 0) {
                    if (body.len < 4) return error.Corrupt;
                    const n: usize = std.mem.readInt(u32, body[0..4], .little);
                    if (body.len - 4 != n * MAP_ENTRY_WIRE_LEN) return error.Corrupt;
                    l.entries = std.mem.bytesAsSlice(MapEntryWire, body[4..]);
                } else {
                    var pos: usize = 0;
                    if (l.flags & F_SWEEP_CURSOR != 0) {
                        if (body.len < pos + 4) return error.Corrupt;
                        l.sweep_cursor = std.mem.readInt(u32, body[pos..][0..4], .little);
                        pos += 4;
                    }
                    if (l.flags & F_MAP_GEN != 0) {
                        if (body.len < pos + 8) return error.Corrupt;
                        l.map_gen = std.mem.readInt(u64, body[pos..][0..8], .little);
                        pos += 8;
                    }
                    if (pos != body.len) return error.Corrupt;
                }
            },
        }
        return l;
    }
};

/// What one read or write needs of a stored layout: its head and the ONE
/// binding the op's block names.
pub const Point = struct {
    kind: Kind,
    size: u64,
    file_id: FileId,
    durable_copy: ?Mapping,
    /// A tree-mapped head: the block resolves in tree 7, never here.
    kvmap: ?Layout.KvmapHead,
    /// The inline map's binding of the asked block (null = a hole, or none asked).
    binding: ?Mapping,
};

/// `decode` short of the inline map's entries: the header and the body's
/// framing checked, `entries` borrowed UNCHECKED from `bytes` (the read
/// view copies them and checks the one entry each read lands on, as
/// `resolvePoint` does).
pub fn decodeHead(bytes: []const u8, geo: Geometry) LayoutError!Layout {
    const l = try Layout.decodeShape(bytes, geo);
    try l.validateHead(geo);
    return l;
}

/// The one inline-map entry's check `resolvePoint` applies where it lands
/// (`validate`'s per-entry law): zero reserved bytes, the window law.
pub fn checkEntry(e: *const MapEntryWire, chunk: u64) LayoutError!Mapping {
    if (!std.mem.allEqual(u8, &e.reserved_le, 0)) return error.Corrupt;
    return try Mapping.decode(e.mapping, chunk);
}

/// The per-op read of a layout record, O(log entries) whatever the file's
/// size: `decode`'s checks on the header and on the ONE inline-map entry the
/// search lands on, never on the rest of the map (the full `decode` of every
/// publish's read-modify-write and fsck's walk check those).
pub fn resolvePoint(bytes: []const u8, geo: Geometry, block: ?u32) LayoutError!Point {
    const l = try decodeHead(bytes, geo);
    return .{
        .kind = l.kind,
        .size = l.size,
        .file_id = l.file_id,
        .durable_copy = l.durableCopy(),
        .kvmap = l.kvmapHead(),
        .binding = if (block) |b| try l.lookupChecked(geo.chunk, b) else null,
    };
}

/// One `(block → mapping)` pair.
pub const MapEntry = struct {
    block: u32,
    mapping: Mapping,

    pub fn encode(self: MapEntry) MapEntryWire {
        var w: MapEntryWire = .{ .block_le = undefined, .reserved_le = @splat(0), .mapping = self.mapping.encode() };
        std.mem.writeInt(u32, &w.block_le, self.block, .little);
        return w;
    }
};

/// Encode entries (ANY order, unique blocks) into a strictly ascending
/// `MapEntryWire` table — the canonical order is a decode law.
pub fn sortedEntries(gpa: Allocator, entries: []const MapEntry) Allocator.Error![]MapEntryWire {
    const out = try gpa.alloc(MapEntryWire, entries.len);
    for (entries, 0..) |e, i| out[i] = e.encode();
    std.mem.sort(MapEntryWire, out, {}, entryWireLess);
    return out;
}

fn entryWireLess(_: void, a: MapEntryWire, b: MapEntryWire) bool {
    return a.block() < b.block();
}

// ---------------------------------------------------------------------------
// The ONE field table — derived from the wire header at comptime; the body
// rows appended in declaration order. Bit i = row i for Put AND Delta.
// ---------------------------------------------------------------------------

const FieldSpec = struct { bit: u16, name: []const u8, width: usize };

/// The body rows a delta may carry absolutely (the inline payload is not
/// one: an inline write is a full Put).
const body_field_rows = [_]struct { name: []const u8, width: usize }{
    .{ .name = "file_id", .width = 16 },
    .{ .name = "durable_copy", .width = MAPPING_WIRE_LEN },
    .{ .name = "sweep_cursor", .width = 4 },
    .{ .name = "map_gen", .width = 8 },
    // Variable: `entry_count u32 ‖ MapEntryWire[]`.
    .{ .name = "map_inserts", .width = 0 },
};

pub const layout_field_table = blk: {
    const info = @typeInfo(LayoutHeaderWire).@"struct";
    var rows: [info.field_names.len + body_field_rows.len]FieldSpec = undefined;
    var n: usize = 0;
    for (info.field_names, info.field_types) |name, T| {
        // The record's framing, never a field a delta carries.
        if (std.mem.eql(u8, name, "version") or std.mem.eql(u8, name, "body_len_le")) continue;
        const base = if (std.mem.endsWith(u8, name, "_le")) name[0 .. name.len - "_le".len] else name;
        rows[n] = .{ .bit = 1 << n, .name = base, .width = @sizeOf(T) };
        n += 1;
    }
    for (body_field_rows) |r| {
        rows[n] = .{ .bit = 1 << n, .name = r.name, .width = r.width };
        n += 1;
    }
    break :blk rows[0..n].*;
};

fn fieldBit(comptime name: []const u8) u16 {
    for (layout_field_table) |f| if (std.mem.eql(u8, f.name, name)) return f.bit;
    unreachable;
}

pub const DELTA_LAYOUT_KIND: u16 = fieldBit("layout_kind");
pub const DELTA_FLAGS: u16 = fieldBit("flags");
pub const DELTA_SIZE: u16 = fieldBit("size");
pub const DELTA_LAYOUT_VERSION: u16 = fieldBit("layout_version");
pub const DELTA_BASE_VERSION: u16 = fieldBit("base_version");
pub const DELTA_FILE_ID: u16 = fieldBit("file_id");
pub const DELTA_DURABLE_COPY: u16 = fieldBit("durable_copy");
pub const DELTA_SWEEP_CURSOR: u16 = fieldBit("sweep_cursor");
pub const DELTA_MAP_GEN: u16 = fieldBit("map_gen");
pub const DELTA_MAP_INSERTS: u16 = fieldBit("map_inserts");
pub const DELTA_MASK_ALL: u16 = (1 << layout_field_table.len) - 1;

comptime {
    assert(layout_field_table.len == 10);
    assert(DELTA_LAYOUT_KIND == 1 << 0);
    assert(DELTA_BASE_VERSION == 1 << 4);
    assert(DELTA_FILE_ID == 1 << 5);
    assert(DELTA_MAP_INSERTS == 1 << 9);
    assert(DELTA_MASK_ALL == 0x3FF);
    // The reference's layout-delta magic (0x4C31) carries bits outside the
    // mask, so a stray legacy payload refuses here rather than folding.
    assert(0x4C31 & ~DELTA_MASK_ALL != 0);
    // The Δinode mask space (11 bits) and this one are told apart by the
    // KEY, never by the payload — the spaces overlap by design.
    assert(record.DELTA_MASK_ALL & DELTA_MASK_ALL != 0);
}

// ---------------------------------------------------------------------------
// The layout Delta — the ONE Delta kind over the layout table.
// ---------------------------------------------------------------------------

/// `field_mask u16 ‖ the masked fields in table order ‖ [entry_count u32 ‖
/// MapEntryWire[] iff MAP_INSERTS]`. Fold = overwrite the masked fields,
/// insert (or replace) the entries; removals take a full Put.
pub const LayoutDelta = struct {
    mask: u16,
    /// Unmasked entries are ignored by `apply` and never encoded.
    fields: Layout = .{ .kind = .inline_ },
    /// Ascending by block, unique.
    inserts: []const MapEntryWire = &.{},

    /// The size-only publish (the degenerate merge).
    pub fn sizeOnly(size: u64) LayoutDelta {
        return .{ .mask = DELTA_SIZE, .fields = .{ .kind = .inline_, .size = size } };
    }

    /// The block-publish delta: `size` plus map inserts, versioned iff
    /// `version != 0` (`base_version` = the previous link's version or 0).
    pub fn mapInserts(size: u64, inserts: []const MapEntryWire, base_version: u64, version: u64) LayoutDelta {
        var d: LayoutDelta = .{ .mask = DELTA_SIZE | DELTA_MAP_INSERTS, .fields = .{ .kind = .inline_, .size = size }, .inserts = inserts };
        if (version != 0) {
            d.mask |= DELTA_LAYOUT_VERSION | DELTA_BASE_VERSION;
            d.fields.layout_version = version;
            d.fields.base_version = base_version;
        }
        return d;
    }

    pub fn versioned(self: LayoutDelta) bool {
        return self.mask & DELTA_LAYOUT_VERSION != 0 and self.fields.layout_version != 0;
    }

    pub fn encodedLen(self: LayoutDelta) usize {
        var n: usize = 2;
        inline for (layout_field_table) |f| {
            if (self.mask & f.bit != 0) n += f.width;
        }
        if (self.mask & DELTA_MAP_INSERTS != 0) n += 4 + self.inserts.len * MAP_ENTRY_WIRE_LEN;
        return n;
    }

    fn writeField(self: LayoutDelta, comptime name: []const u8, out: []u8) void {
        if (comptime std.mem.eql(u8, name, "layout_kind")) {
            out[0] = @backingInt(self.fields.kind);
        } else if (comptime std.mem.eql(u8, name, "flags")) {
            std.mem.writeInt(u16, out[0..2], self.fields.flags, .little);
        } else if (comptime std.mem.eql(u8, name, "size")) {
            std.mem.writeInt(u64, out[0..8], self.fields.size, .little);
        } else if (comptime std.mem.eql(u8, name, "layout_version")) {
            std.mem.writeInt(u64, out[0..8], self.fields.layout_version, .little);
        } else if (comptime std.mem.eql(u8, name, "base_version")) {
            std.mem.writeInt(u64, out[0..8], self.fields.base_version, .little);
        } else if (comptime std.mem.eql(u8, name, "file_id")) {
            out[0..16].* = self.fields.file_id;
        } else if (comptime std.mem.eql(u8, name, "durable_copy")) {
            out[0..MAPPING_WIRE_LEN].* = std.mem.toBytes(self.fields.durable_copy.encode());
        } else if (comptime std.mem.eql(u8, name, "sweep_cursor")) {
            std.mem.writeInt(u32, out[0..4], self.fields.sweep_cursor, .little);
        } else if (comptime std.mem.eql(u8, name, "map_gen")) {
            std.mem.writeInt(u64, out[0..8], self.fields.map_gen, .little);
        } else {
            comptime unreachable;
        }
    }

    fn readField(self: *LayoutDelta, comptime name: []const u8, in: []const u8, geo: Geometry) LayoutError!void {
        if (comptime std.mem.eql(u8, name, "layout_kind")) {
            self.fields.kind = try Kind.fromByte(in[0]);
        } else if (comptime std.mem.eql(u8, name, "flags")) {
            self.fields.flags = std.mem.readInt(u16, in[0..2], .little);
            if (self.fields.flags & ~F_KNOWN != 0) return error.Corrupt;
        } else if (comptime std.mem.eql(u8, name, "size")) {
            self.fields.size = std.mem.readInt(u64, in[0..8], .little);
        } else if (comptime std.mem.eql(u8, name, "layout_version")) {
            self.fields.layout_version = std.mem.readInt(u64, in[0..8], .little);
        } else if (comptime std.mem.eql(u8, name, "base_version")) {
            self.fields.base_version = std.mem.readInt(u64, in[0..8], .little);
        } else if (comptime std.mem.eql(u8, name, "file_id")) {
            self.fields.file_id = in[0..16].*;
        } else if (comptime std.mem.eql(u8, name, "durable_copy")) {
            self.fields.durable_copy = try Mapping.decode(std.mem.bytesToValue(MappingWire, in[0..MAPPING_WIRE_LEN]), geo.chunk);
        } else if (comptime std.mem.eql(u8, name, "sweep_cursor")) {
            self.fields.sweep_cursor = std.mem.readInt(u32, in[0..4], .little);
        } else if (comptime std.mem.eql(u8, name, "map_gen")) {
            self.fields.map_gen = std.mem.readInt(u64, in[0..8], .little);
        } else {
            comptime unreachable;
        }
    }

    pub fn encodeInto(self: LayoutDelta, out: []u8) []u8 {
        assert(self.mask != 0 and self.mask & ~DELTA_MASK_ALL == 0);
        // A versioned link carries its claim; an unversioned one none.
        assert((self.mask & DELTA_BASE_VERSION != 0) == (self.mask & DELTA_LAYOUT_VERSION != 0 and self.fields.layout_version != 0));
        const n = self.encodedLen();
        assert(out.len >= n);
        std.mem.writeInt(u16, out[0..2], self.mask, .little);
        var pos: usize = 2;
        inline for (layout_field_table) |f| {
            if (f.width != 0 and self.mask & f.bit != 0) {
                self.writeField(f.name, out[pos..][0..f.width]);
                pos += f.width;
            }
        }
        if (self.mask & DELTA_MAP_INSERTS != 0) {
            std.mem.writeInt(u32, out[pos..][0..4], @intCast(self.inserts.len), .little);
            pos += 4;
            const bytes = std.mem.sliceAsBytes(self.inserts);
            @memcpy(out[pos..][0..bytes.len], bytes);
            pos += bytes.len;
        }
        assert(pos == n);
        return out[0..n];
    }

    pub fn encodeAlloc(self: LayoutDelta, gpa: Allocator) Allocator.Error![]u8 {
        const out = try gpa.alloc(u8, self.encodedLen());
        return self.encodeInto(out);
    }

    /// Total: a non-zero known mask, every masked field present, a claim
    /// iff a non-zero version, strictly ascending inserts whose mappings
    /// pass the window law, no trailing bytes. `inserts` borrow from
    /// `bytes`.
    pub fn decode(bytes: []const u8, geo: Geometry) LayoutError!LayoutDelta {
        if (bytes.len < 2) return error.Corrupt;
        const mask = std.mem.readInt(u16, bytes[0..2], .little);
        if (mask == 0 or mask & ~DELTA_MASK_ALL != 0) return error.Corrupt;
        var d: LayoutDelta = .{ .mask = mask };
        var pos: usize = 2;
        inline for (layout_field_table) |f| {
            if (f.width != 0 and mask & f.bit != 0) {
                if (bytes.len < pos + f.width) return error.Corrupt;
                try d.readField(f.name, bytes[pos..][0..f.width], geo);
                pos += f.width;
            }
        }
        const has_claim = mask & DELTA_BASE_VERSION != 0;
        const has_version = mask & DELTA_LAYOUT_VERSION != 0 and d.fields.layout_version != 0;
        if (has_claim != has_version) return error.Corrupt;
        if (mask & DELTA_MAP_INSERTS != 0) {
            if (bytes.len < pos + 4) return error.Corrupt;
            const n: usize = std.mem.readInt(u32, bytes[pos..][0..4], .little);
            pos += 4;
            if (bytes.len - pos != n * MAP_ENTRY_WIRE_LEN) return error.Corrupt;
            d.inserts = std.mem.bytesAsSlice(MapEntryWire, bytes[pos..]);
            pos = bytes.len;
            var prev: ?u32 = null;
            for (d.inserts) |*e| {
                if (!std.mem.allEqual(u8, &e.reserved_le, 0)) return error.Corrupt;
                if (prev) |p| if (e.block() <= p) return error.MapNotAscending;
                prev = e.block();
                _ = try Mapping.decode(e.mapping, geo.chunk);
            }
        }
        if (pos != bytes.len) return error.Corrupt;
        return d;
    }

    /// The in-place fold onto `base`: masked fields OVERWRITE, inserts
    /// merge into the inline map (replacing a same-block entry) written to
    /// `scratch`, which the returned layout borrows. Idempotent: replaying
    /// a delta over an already-folded base changes nothing.
    pub fn applyAlloc(self: LayoutDelta, gpa: Allocator, base: Layout, scratch: *std.ArrayList(MapEntryWire), geo: Geometry) FoldError!Layout {
        var l = base;
        if (self.mask & DELTA_LAYOUT_KIND != 0) l.kind = self.fields.kind;
        if (self.mask & DELTA_FLAGS != 0) l.flags = self.fields.flags;
        if (self.mask & DELTA_SIZE != 0) l.size = self.fields.size;
        if (self.mask & DELTA_LAYOUT_VERSION != 0) l.layout_version = self.fields.layout_version;
        // The claim is the link's, never the folded record's (a fold IS a
        // base).
        l.base_version = 0;
        if (self.mask & DELTA_FILE_ID != 0) l.file_id = self.fields.file_id;
        if (self.mask & DELTA_DURABLE_COPY != 0) l.durable_copy = self.fields.durable_copy;
        if (self.mask & DELTA_SWEEP_CURSOR != 0) l.sweep_cursor = self.fields.sweep_cursor;
        if (self.mask & DELTA_MAP_GEN != 0) l.map_gen = self.fields.map_gen;
        if (self.mask & DELTA_MAP_INSERTS != 0 and self.inserts.len != 0) {
            if (l.kind != .striped or l.flags & F_MAP_INLINE == 0) return error.DeltaOntoNonInlineMap;
            scratch.clearRetainingCapacity();
            try scratch.ensureTotalCapacity(gpa, l.entries.len + self.inserts.len);
            // Two-way merge of two ascending runs; an insert replaces.
            var i: usize = 0;
            var j: usize = 0;
            while (i < l.entries.len or j < self.inserts.len) {
                if (j == self.inserts.len or (i < l.entries.len and l.entries[i].block() < self.inserts[j].block())) {
                    scratch.appendAssumeCapacity(l.entries[i]);
                    i += 1;
                } else {
                    if (i < l.entries.len and l.entries[i].block() == self.inserts[j].block()) i += 1;
                    scratch.appendAssumeCapacity(self.inserts[j]);
                    j += 1;
                }
            }
            l.entries = scratch.items;
        }
        l.validate(geo) catch return error.DeltaInconsistent;
        return l;
    }
};

/// Fold a delta CHAIN (oldest first) onto an encoded base Put and answer the
/// canonical re-encoding — the layout class's half of `record.foldDeltas
/// OntoPut`, with the §6.2-item-9 link law: per base segment all-versioned
/// or all-unversioned, the first versioned link claims 0, every later one
/// claims the previous link's version. Exact-seq duplicates are the caller's
/// to drop (the fold here sees each link once).
pub fn foldChain(gpa: Allocator, base_value: []const u8, deltas_oldest_first: []const []const u8, geo: Geometry) FoldError![]u8 {
    var layout = try Layout.decode(base_value, geo);
    var a: std.ArrayList(MapEntryWire) = .empty;
    defer a.deinit(gpa);
    var b: std.ArrayList(MapEntryWire) = .empty;
    defer b.deinit(gpa);
    var prev_version: ?u64 = null;
    var saw_unversioned = false;
    for (deltas_oldest_first, 0..) |payload, i| {
        const d = try LayoutDelta.decode(payload, geo);
        if (d.versioned()) {
            if (saw_unversioned) return error.LayoutChainHeterogeneous;
            const want = prev_version orelse 0;
            if (d.fields.base_version != want) return error.LayoutChainDivergent;
            prev_version = d.fields.layout_version;
        } else {
            if (prev_version != null) return error.LayoutChainHeterogeneous;
            saw_unversioned = true;
        }
        // Alternate scratch lists so the base's borrowed entries outlive
        // the merge that reads them.
        layout = try d.applyAlloc(gpa, layout, if (i % 2 == 0) &a else &b, geo);
    }
    return layout.encodeAlloc(gpa);
}

// ---------------------------------------------------------------------------
// §5.6 The delta chain cap and per-ino publish coalescing.
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN`: a publish onto a chain at the cap
/// re-bases with a full Put; 0 = full Puts only.
pub const DeltaPolicy = struct {
    max_chain: u32,

    pub fn fromEnv(env: *const std.process.Environ.Map) DeltaPolicy {
        return .{ .max_chain = knobs.intKnob("SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN", u32, env) };
    }

    /// May the next publish ride a delta given `chain_depth` links already
    /// above the base? Counts the re-base when not.
    pub fn useDelta(self: DeltaPolicy, chain_depth: u32) bool {
        if (chain_depth < self.max_chain) return true;
        stats.layout_delta_rebases.inc();
        return false;
    }
};

/// `SQUEEZEFS_PUBLISH_COALESCE_MAX`: the per-ino window gathering block
/// inserts into ONE delta. Owned by the ino's publisher (one window per
/// in-flight ino); `≤ 1` = serialized (every insert its own record).
pub const PublishWindow = struct {
    max: u32,
    pending: std.ArrayList(MapEntryWire) = .empty,

    pub fn fromEnv(env: *const std.process.Environ.Map) PublishWindow {
        return .{ .max = knobs.intKnob("SQUEEZEFS_PUBLISH_COALESCE_MAX", u32, env) };
    }

    pub fn deinit(self: *PublishWindow, gpa: Allocator) void {
        self.pending.deinit(gpa);
    }

    /// Queue one insert; true when the window is full and must flush.
    pub fn push(self: *PublishWindow, gpa: Allocator, e: MapEntry) Allocator.Error!bool {
        try self.pending.append(gpa, e.encode());
        return self.pending.items.len >= @max(self.max, 1);
    }

    /// The window's inserts as ONE ascending table (sorted in place; the
    /// caller encodes a delta and `clear`s). Counted as one batch.
    pub fn take(self: *PublishWindow) []MapEntryWire {
        std.mem.sort(MapEntryWire, self.pending.items, {}, entryWireLess);
        stats.layout_publish_batches.inc();
        stats.layout_publish_batched_blocks.add(self.pending.items.len);
        return self.pending.items;
    }

    pub fn clear(self: *PublishWindow) void {
        self.pending.clearRetainingCapacity();
    }
};

// ---------------------------------------------------------------------------
// §6 The inline bound.
// ---------------------------------------------------------------------------

/// One page: the cost priced is per PAYLOAD BYTE on the metadata plane (an
/// inline write rides the plane twice — journal entry + CoW node append ≈
/// 2× its bytes — against the staged layout's fixed ≈ 0.3 KB per file); one
/// page is where the record's payload costs about what a block mapping does.
/// Measured (`.benchmarks/2026-09-09-inline-raise-sweep-local.md`): 16 KiB
/// inline = −34 % files/s, 2.0× fsync µs, 59× journal bytes/file, 50× node-
/// append bytes/file; 32 KiB = −68 %, 3.3×, 117×/96× and a 1 GiB metadata
/// volume heap-exhausted and FAIL-STOPPED in 20 s.
pub const INLINE_MAX_FLOOR: u64 = 4096;
/// The layout wire's framing allowance under the record value cap: the 32 B
/// header + the 4 B length, rounded to the page so the bound is a page
/// multiple at every node size.
pub const LAYOUT_INLINE_HEADROOM: u64 = 4096;
/// The registry's range top: the Linux `XATTR_SIZE_MAX` record cap minus
/// the headroom (the 256 KiB node's bound).
pub const INLINE_MAX_CEILING: u64 = record.RECORD_VALUE_CAP_CEILING - LAYOUT_INLINE_HEADROOM;

/// The format bound at `node_size`: `max(cap − headroom, floor)` with `cap
/// = min(64 KiB, node_size/4)` — 60 KiB at 256 KiB, 12 KiB at the 64 KiB
/// node floor.
pub fn inlineMaxCeiling(node_size: u64) u64 {
    return @max(record.xattrValueCap(@intCast(node_size)) - LAYOUT_INLINE_HEADROOM, INLINE_MAX_FLOOR);
}

/// The derived default: one page, never above the bound.
pub fn derivedInlineMax(facts: derive.Facts, node_size: u64) u64 {
    return @min(@max(facts.page_size, INLINE_MAX_FLOOR), inlineMaxCeiling(node_size));
}

pub const InlineBoundError = error{InlineMaxAboveFormatBound};

/// `explicit > derived`; an explicit override above THIS set's bound
/// REFUSES (charter §1.6: never clamped — the startup gate cannot know the
/// node size, so the mount is where the refusal lands, naming the bound).
/// Publishes `inline_max_bytes`.
pub fn resolveInlineMax(explicit: ?u64, facts: derive.Facts, node_size: u64) InlineBoundError!u64 {
    const bound = inlineMaxCeiling(node_size);
    const v = if (explicit) |e| blk: {
        if (e > bound) {
            log.err("SQUEEZEFS_INLINE_MAX_BYTES={d} is above this set's format bound {d} (min(64 KiB, node_size/4) − 4 KiB at node_size {d}) — refusing the mount", .{ e, bound, node_size });
            return error.InlineMaxAboveFormatBound;
        }
        break :blk e;
    } else derivedInlineMax(facts, node_size);
    stats.inline_max_bytes.set(v);
    return v;
}

pub fn inlineMaxFromEnv(env: *const std.process.Environ.Map, facts: derive.Facts, node_size: u64) InlineBoundError!u64 {
    return resolveInlineMax(knobs.optIntKnob("SQUEEZEFS_INLINE_MAX_BYTES", u64, env), facts, node_size);
}

// ---------------------------------------------------------------------------
// Lock levels 3.5 and 3.7: the per-ino block-map meta stripe and the plane's
// per-ino layout lock (charter §2; `data/plane.zig` module doc declares 3.7).
// ---------------------------------------------------------------------------

/// 4096-way: the read→merge→save hold of a block-map publish, never held
/// across a terminal block free (RES-1) — so one stripe per 4096 inos keeps
/// a collision to one short section in 4096 at full handler parallelism,
/// the same load factor the 4a stripes use.
pub const INODE_META_STRIPES: usize = 4096;
/// The layout lock's width: the same load factor (one short commit-length
/// section in 4096 at full handler parallelism).
pub const LAYOUT_LOCK_STRIPES: usize = 4096;

/// A PARKING mutex, not a spin: the bound `MapSeam.merge` commits the
/// layout transaction through the conveyor INSIDE the section (the
/// read→stage→commit must be one atomic step against a sibling publish),
/// and a commit parks the fiber. A spinning fiber on the holder's own lane
/// would never let the holder resume (the lane runs one fiber at a time)
/// — a deterministic hang at the first same-stripe collision.
pub const MetaStripes = struct {
    locks: [INODE_META_STRIPES]sync_mutex.Mutex = @splat(.init),

    pub const Guard = struct {
        lock: *sync_mutex.Mutex,

        pub fn release(self: Guard, io: Io) void {
            self.lock.unlock(io);
            lock_order.released(.meta_stripe);
        }
    };

    /// Fibonacci-spread: consecutive mints land on different lines.
    pub fn stripeOf(ino: u64) usize {
        return @intCast((ino *% 0x9E3779B97F4A7C15) >> (64 - 12));
    }

    pub fn acquire(self: *MetaStripes, io: Io, ino: u64) Guard {
        const l = &self.locks[stripeOf(ino)];
        lock_order.acquiring(.meta_stripe);
        l.lockUncancelable(io);
        return .{ .lock = l };
    }

    /// Parked acquirers on `ino`'s stripe (a test observable).
    pub fn waiters(self: *MetaStripes, io: Io, ino: u64) u32 {
        return self.locks[stripeOf(ino)].waiters(io);
    }
};

/// Level (3.7) — the plane's per-ino LAYOUT lock: every layout-record
/// read→stage→commit of one ino serializes here (the merge, the crossing,
/// the sweep chunk, the promotion, kv_fs's truncate and corpse
/// transactions). It sits BETWEEN (3.5) and (4a): a block-map publish takes
/// it INSIDE the upload's meta stripe (`accumulate`, `device_overlay`,
/// `writeback` hold (3.5) around `MapSeam.merge`, which takes this), and
/// kv_fs takes it BEFORE its 4a set. A parking mutex (the commit parks;
/// the same lane argument as `MetaStripes`). Never held across a terminal
/// block free (RES-1).
pub const LayoutLocks = struct {
    locks: [LAYOUT_LOCK_STRIPES]sync_mutex.Mutex = @splat(.init),

    pub const Guard = struct {
        lock: *sync_mutex.Mutex,

        pub fn release(self: Guard, io: Io) void {
            self.lock.unlock(io);
            lock_order.released(.layout);
        }
    };

    /// The op guard's mix (`dlm.splitmix64`), so an ino's two tables keep
    /// one shape; a different mix from the meta stripe's, so the two
    /// levels' collisions are independent.
    pub fn stripeOf(ino: u64) usize {
        return @intCast(splitmix64(ino) & (LAYOUT_LOCK_STRIPES - 1));
    }

    pub fn acquire(self: *LayoutLocks, io: Io, ino: u64) Guard {
        const l = &self.locks[stripeOf(ino)];
        lock_order.acquiring(.layout);
        l.lockUncancelable(io);
        return .{ .lock = l };
    }

    /// Parked acquirers on `ino`'s stripe (a test observable).
    pub fn waiters(self: *LayoutLocks, io: Io, ino: u64) u32 {
        return self.locks[stripeOf(ino)].waiters(io);
    }
};

/// splitmix64 finalizer — `meta/dlm.zig`'s ONE mix, restated here so the
/// portable layout module owes the metadata plane nothing.
fn splitmix64(v: u64) u64 {
    var x = v;
    x ^= x >> 30;
    x *%= 0xbf58476d1ce4e5b9;
    x ^= x >> 27;
    x *%= 0x94d049bb133111eb;
    x ^= x >> 31;
    return x;
}

/// A test seam: a promotion parks here between its two acquisitions.
pub const PromotionPark = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque) void,
};

/// The two publish levels together with the ONE order a staged promotion
/// takes them in — (3.5) THEN (3.7). The promotion's identity re-check and
/// its commit need both: the stripe against a sibling block-map publish of
/// the ino, the layout lock against the merge / the sweep / kv_fs. Taking
/// them the other way round inverted the charter's order against every
/// upload (which holds (3.5) and merges under (3.7)): two inos colliding on
/// both tables deadlocked (`plane_locks_model.PromotionOrderModel` proves
/// the inverted order fails; 0002 row 209).
pub const PublishLocks = struct {
    stripes: MetaStripes = .{},
    layouts: LayoutLocks = .{},
    /// Runs between the stripe and the layout lock; null = inert.
    promotion_park: ?PromotionPark = null,

    pub const PromotionGuard = struct {
        stripe: MetaStripes.Guard,
        layout: LayoutLocks.Guard,

        /// Reverse order of acquisition.
        pub fn release(self: PromotionGuard, io: Io) void {
            self.layout.release(io);
            self.stripe.release(io);
        }
    };

    pub fn lockPromotion(self: *PublishLocks, io: Io, ino: u64) PromotionGuard {
        const stripe = self.stripes.acquire(io, ino);
        if (self.promotion_park) |p| p.call(p.ctx);
        const l = self.layouts.acquire(io, ino);
        return .{ .stripe = stripe, .layout = l };
    }
};

comptime {
    assert(INODE_META_STRIPES == 1 << 12);
    assert(LAYOUT_LOCK_STRIPES == 1 << 12);
}

// ---------------------------------------------------------------------------
// The seams: DMA proof, the refs ledger, the publish.
// ---------------------------------------------------------------------------

/// The witness that a device DMA of `[offset, offset + len)` on `vol_tag`
/// COMPLETED — minted only by a `DmaSeam.write` completion (the data plane's
/// device binding), consumed by `publishLayout`. "Block I/O strictly before
/// the metadata type flip" (charter §6.1) is therefore a type a publish
/// cannot be called without.
pub const DmaProof = struct {
    vol_tag: u64,
    offset: u64,
    len: u64,
    completed_ns: u64,

    /// Does this proof cover the bytes `m` names? A tenant's proof covers its
    /// whole slot. A WHOLE-BLOCK DMA is the complete payload or image by
    /// construction — a transformed image is self-delimiting (its header
    /// bounds every read; the chunk's tail past it is never read) and a
    /// passthrough payload is bounded by the layout's `size` — so its proof
    /// covers the mapping from the slot's start at any grain-padded length:
    /// a block that compressed lands an image shorter than the chunk, and
    /// the whole-slot law refused every one of them (`ProofMissing`, the
    /// upload retrying forever).
    pub fn covers(self: DmaProof, m: Mapping, chunk: u64) bool {
        if (self.vol_tag != m.vol_tag) return false;
        const start = m.offset + m.rel_off;
        const end = if (m.whole_block) start + GRAIN else start + m.slotLen(chunk);
        return self.offset <= start and end <= self.offset + self.len;
    }
};

pub const DmaError = error{ InputOutput, NoSpaceLeft, QueueFull, WorkerShutdown, VolumeFailed, WriterGuardFenced, OutOfMemory };

/// ONE DMA at `offset` on `vol_tag` of `data` padded to the LBA grain — the
/// device's unaligned arm WRITES the zero tail `[data.len, ceil(data.len))`
/// (`nvme_dev.Device.writeBlock`; `nvme_unaligned_write_fallbacks` is the
/// expected growth of a packed population), so the proof covers the whole
/// slot. The completion mints the proof. The device binding (through the
/// router's `vol_tag → Device` table) implements it; tests bind a RAM
/// device.
pub const DmaSeam = struct {
    ctx: *anyopaque,
    write_fn: *const fn (ctx: *anyopaque, vol_tag: u64, offset: u64, data: []const u8) DmaError!void,

    pub fn write(self: DmaSeam, vol_tag: u64, offset: u64, data: []const u8, now_ns: u64) DmaError!DmaProof {
        try self.write_fn(self.ctx, vol_tag, offset, data);
        return .{ .vol_tag = vol_tag, .offset = offset, .len = ceilGrain(data.len), .completed_ns = now_ns };
    }
};

/// One durable reference op: the refs record `key.blockRefKey(vol_tag,
/// block_idx, owner_ino, block_index)` added or released in the SAME tx as
/// the layout record (charter §5.6).
pub const RefOp = struct {
    vol_tag: u64,
    block_idx: u64,
    owner_ino: u64,
    block_index: u32,
};

/// The durable-refs ledger's staging face. BINDING: `data/block_refs.zig`
/// (the allocator/refs agent) installs `stage_add` = stage the `Put` of the
/// ref record (`record.BlockRefValue`) and note the RAM pin, `stage_release`
/// = stage its `Delete` and queue the RAM decrement for after the commit
/// lands (a release decrements RAM only if THIS owner's durable record
/// existed). `ctx` is that module's ledger.
pub const RefsSeam = struct {
    ctx: *anyopaque,
    stage_add: *const fn (ctx: *anyopaque, t: *tx.KvTx, op: RefOp) Allocator.Error!void,
    stage_release: *const fn (ctx: *anyopaque, t: *tx.KvTx, op: RefOp) Allocator.Error!void,
};

/// The layout transitions (`docs/porting/data_layouts.md` §2): a same-kind
/// publish is always a transition to itself; `null` is a fresh file. Packed
/// is reached only by promotion of a staged file; striped is terminal.
pub fn validTransition(from: ?Kind, to: Kind) bool {
    const f = from orelse return to != .packed_;
    if (f == to) return true;
    return switch (f) {
        .inline_ => to == .staged or to == .striped,
        .staged => true,
        .packed_ => true,
        .striped => false,
    };
}

/// The map's own law inside the striped kind: a tree-mapped head is STICKY
/// (charter §5.6) — a shrinking map never collapses back to an inline map
/// (the collapse would need its own sweep and buys nothing: the head is
/// ~45 B either way).
pub fn validMapTransition(from: ?Layout, to: Layout) bool {
    const f = from orelse return true;
    return !(f.isKvmap() and to.kind == .striped and to.flags & F_MAP_INLINE != 0);
}

pub const PublishError = Allocator.Error || LayoutError || error{
    /// `validTransition(prev, new)` is false, or a tree-mapped head would
    /// regress to an inline map.
    InvalidTransition,
    /// A newly named block has no completed DMA behind it.
    ProofMissing,
};

/// How the publish treats the references the previous layout named.
pub const PublishArm = union(enum) {
    /// The named-reference diff: `+ref` for every block the new layout
    /// names and the old did not (each needs its proof), `−ref` for every
    /// one the old named and the new does not.
    diff,
    /// THE FLIP to the block-map tree: the inline map's `moved` entries
    /// became kind-7 records in the same train, and a reference's identity
    /// `(vol_tag, block_idx, owner, index)` is independent of where the MAP
    /// lives — so NOTHING is released and nothing is added here (a kvmap
    /// head "names nothing" to the diff, which would otherwise stage a
    /// `−ref` for every block of the file). The publishing window's own
    /// `±ref` are the caller's, staged beside this into the same tx.
    flip_to_kvmap: struct { moved: usize },
};

pub const Publish = struct {
    /// The LOCAL key ino.
    ino: u64,
    name_key: *const key.NameKey,
    /// The layout xattr's chain slot (the caller resolved it).
    coll_seq: u8 = 0,
    /// The layout being replaced (its named references are diffed against
    /// the new layout's); null = a fresh file.
    prev: ?Layout,
    layout: Layout,
    /// The completed DMAs behind every block the new layout names and the
    /// old one did not.
    proofs: []const DmaProof,
    arm: PublishArm = .diff,
    /// A COLLAPSE/INSERT shift (decision 0003 §BH.5): the map's entries
    /// re-indexed, the data unmoved — every reference the old layout
    /// named at another index needs no proof (its bytes landed before
    /// that earlier publish); the `±ref` diff re-keys them as usual.
    shift: bool = false,
};

/// Stage the layout record (`Put` of `XattrValue{"layout", encoded}`) and
/// the `+ref`/`−ref` diff in ONE tx. Refuses an inadmissible layout, an
/// illegal transition, and a new block without its proof. The caller holds
/// the ino's meta stripe (3.5) across read → publish → commit and frees any
/// displaced block only after the guard drops (RES-1).
pub fn publishLayout(t: *tx.KvTx, p: Publish, refs: RefsSeam, geo: Geometry) PublishError!void {
    try p.layout.validate(geo);
    if (!validTransition(if (p.prev) |pl| pl.kind else null, p.layout.kind)) return error.InvalidTransition;
    if (!validMapTransition(p.prev, p.layout)) return error.InvalidTransition;
    switch (p.arm) {
        .diff => {},
        .flip_to_kvmap => |f| {
            // The flip is exactly inline-striped → kvmap over the moved map.
            const prev = p.prev orelse return error.InvalidTransition;
            if (prev.kind != .striped or prev.flags & F_MAP_INLINE == 0 or !p.layout.isKvmap()) return error.InvalidTransition;
            assert(prev.entries.len == f.moved);
        },
    }
    // Every reference the new layout names and the old did not needs a
    // proof (checked before anything is staged) and a `+ref`; every one
    // the old named and the new does not a `−ref`. Both sequences are
    // ascending by block (`named`), so the diff is ONE merge walk per
    // pass, an entry decoded only where the walk lands on a difference —
    // a 768-block map paid 2,300 binary searches and 4,600 decodes per
    // publish (21 % of the handler lanes' samples on a streaming write).
    const old: ?*const Layout = if (p.arm == .diff) (if (p.prev) |*pl| pl else null) else null;
    // The shift's exemption set: every reference the old layout names,
    // sorted for the search (the shift path alone pays it).
    const old_refs: ?[]RefId = if (p.shift and old != null) blk: {
        var it = old.?.named(geo.chunk);
        var n: usize = 0;
        while (it.peek() != null) : (it.skip()) n += 1;
        const ids = try t.arena.allocator().alloc(RefId, n);
        it = old.?.named(geo.chunk);
        var i: usize = 0;
        while (it.peek()) |rk| : (it.skip()) {
            ids[i] = .{ .vol_tag = rk.vol_tag, .offset = rk.offset };
            i += 1;
        }
        std.mem.sort(RefId, ids, {}, RefId.lessThan);
        break :blk ids;
    } else null;
    var walk = RefDiff.init(&p.layout, old, geo.chunk);
    while (walk.next()) |d| {
        if (d != .added) continue;
        // An UNWRITTEN binding names no bytes: nothing to precede its flip.
        if (d.added.mapping.unwritten) continue;
        // A shift re-keys references the old layout already named: their
        // bytes landed before that earlier publish.
        if (old_refs) |ids| if (std.sort.binarySearch(RefId, ids, RefId.of(d.added.mapping), RefId.order) != null) continue;
        var covered = false;
        for (p.proofs) |pr| {
            if (pr.covers(d.added.mapping, geo.chunk)) {
                covered = true;
                break;
            }
        }
        if (!covered) return error.ProofMissing;
    }
    const xk = key.xattrKey(p.ino, key.xattrNameHash56(LAYOUT_XATTR_NAME, p.name_key), p.coll_seq);
    const enc_len = (XattrLayout{ .layout = p.layout }).encodedLen();
    // The record value staged ONCE: the xattr envelope + the layout bytes
    // are built straight into the tx arena and staged in place.
    const buf = try t.arena.allocator().alloc(u8, enc_len);
    const value = (XattrLayout{ .layout = p.layout }).encodeInto(buf);
    try t.stagePutArena(key.TREE_XATTRS, &xk, value);
    walk = RefDiff.init(&p.layout, old, geo.chunk);
    while (walk.next()) |d| switch (d) {
        .added => |e| try refs.stage_add(refs.ctx, t, refOf(p.ino, e, geo.chunk)),
        .released => |e| try refs.stage_release(refs.ctx, t, refOf(p.ino, e, geo.chunk)),
    };
    switch (p.layout.kind) {
        .inline_ => stats.layout_inline_writes.inc(),
        .staged => stats.layout_staged_writes.inc(),
        .striped => stats.layout_striped_writes.inc(),
        .packed_ => {},
    }
}

fn refOf(ino: u64, e: MapEntry, chunk: u64) RefOp {
    return .{ .vol_tag = e.mapping.vol_tag, .block_idx = e.mapping.blockIdx(chunk), .owner_ino = ino, .block_index = e.block };
}

/// A reference's identity for the shift's exemption set (`Mapping.sameRef`
/// over a checked mapping: the volume and the block's offset).
const RefId = struct {
    vol_tag: u64,
    offset: u64,

    fn of(m: Mapping) RefId {
        return .{ .vol_tag = m.vol_tag, .offset = m.offset };
    }

    fn order(a: RefId, b: RefId) std.math.Order {
        if (a.vol_tag != b.vol_tag) return std.math.order(a.vol_tag, b.vol_tag);
        return std.math.order(a.offset, b.offset);
    }

    fn lessThan(_: void, a: RefId, b: RefId) bool {
        return order(a, b) == .lt;
    }
};

/// The reference diff of a publish as ONE merge walk over the two ascending
/// named sequences (`NamedIterator`): `added` = a `(block, reference)` the
/// new layout names and the old does not; `released` = the converse. The
/// same block naming the same reference in both is no step and costs no
/// decode (`peek` reads the identity off the wire); a block rebound to
/// another reference yields its release, then its add. Null `old` = a
/// fresh file or the flip arm (nothing released).
const RefDiff = struct {
    new: Layout.NamedIterator,
    old: ?Layout.NamedIterator,
    pending_add: ?MapEntry = null,

    const Step = union(enum) { added: MapEntry, released: MapEntry };

    fn init(new: *const Layout, old: ?*const Layout, chunk: u64) RefDiff {
        return .{ .new = new.named(chunk), .old = if (old) |o| o.named(chunk) else null };
    }

    fn next(self: *RefDiff) ?Step {
        if (self.pending_add) |e| {
            self.pending_add = null;
            return .{ .added = e };
        }
        while (true) {
            const nk = self.new.peek();
            const ok: ?Layout.RefKey = if (self.old) |*o| o.peek() else null;
            if (nk == null and ok == null) return null;
            if (ok == null or (nk != null and nk.?.block < ok.?.block)) return .{ .added = self.new.next().? };
            if (nk == null or ok.?.block < nk.?.block) return .{ .released = self.old.?.next().? };
            if (nk.?.sameRef(ok.?)) {
                self.new.skip();
                self.old.?.skip();
                continue;
            }
            const released = self.old.?.next().?;
            self.pending_add = self.new.next().?;
            return .{ .released = released };
        }
    }
};

/// `XattrValue{"layout", layout}` encoded in one pass (no intermediate
/// buffer).
pub const XattrLayout = struct {
    layout: Layout,

    pub fn encodedLen(self: XattrLayout) usize {
        return 1 + LAYOUT_XATTR_NAME.len + self.layout.encodedLen();
    }

    pub fn encodeInto(self: XattrLayout, out: []u8) []u8 {
        const n = self.encodedLen();
        assert(out.len >= n);
        out[0] = @intCast(LAYOUT_XATTR_NAME.len);
        @memcpy(out[1..][0..LAYOUT_XATTR_NAME.len], LAYOUT_XATTR_NAME);
        _ = self.layout.encodeInto(out[1 + LAYOUT_XATTR_NAME.len ..]);
        return out[0..n];
    }

    /// The layout out of a stored xattr record value.
    pub fn decode(bytes: []const u8, geo: Geometry) LayoutError!Layout {
        const x = record.XattrValue.decode(bytes) catch return error.Corrupt;
        if (!std.mem.eql(u8, x.name, LAYOUT_XATTR_NAME)) return error.Corrupt;
        return Layout.decode(x.value, geo);
    }
};

// ---------------------------------------------------------------------------
// Tests — the wire pinned byte by byte, totality, the table, the laws.
// ---------------------------------------------------------------------------

const testing = std.testing;
const tgeo: Geometry = .default;

fn fid(seed: u8) FileId {
    var f: FileId = undefined;
    for (&f, 0..) |*b, i| b.* = seed +% @as(u8, @intCast(i));
    return f;
}

test "the header is 32 B LE with the version at 1, the mapping 40 B, the entry 48 B; the inline record with a 4 KiB payload is exactly 4,132 B and packed 88 B" {
    const data: [4096]u8 = @splat(0xAB);
    const l = Layout.inlineData(4096, &data);
    try testing.expectEqual(@as(usize, 4132), l.encodedLen());
    const enc = try l.encodeAlloc(testing.allocator);
    defer testing.allocator.free(enc);
    try testing.expectEqualSlices(u8, &.{ 0, 1, 0, 0, 0x04, 0x10, 0, 0 }, enc[0..8]);
    try testing.expectEqualSlices(u8, &.{ 0, 0x10, 0, 0, 0, 0, 0, 0 }, enc[8..16]);
    try testing.expectEqualSlices(u8, &@as([16]u8, @splat(0)), enc[16..32]);
    try testing.expectEqualSlices(u8, &.{ 0, 0x10, 0, 0 }, enc[32..36]);
    const back = try Layout.decode(enc, tgeo);
    try testing.expectEqual(Kind.inline_, back.kind);
    try testing.expectEqualSlices(u8, &data, back.data);
    const pk = Layout.packedTenant(100, fid(1), Mapping.tenant(0x11, 8 * derive.MiB, 7, 8192, 100));
    try testing.expectEqual(@as(usize, 88), pk.encodedLen());
    const penc = try pk.encodeAlloc(testing.allocator);
    defer testing.allocator.free(penc);
    try testing.expectEqual(@as(u8, 2), penc[0]);
    try testing.expectEqualSlices(u8, &.{ 1, 0 }, penc[2..4]);
    try testing.expectEqualSlices(u8, &fid(1), penc[32..48]);
    // The mapping bytes: vol_tag ‖ offset ‖ incarnation ‖ rel_off ‖ len ‖ flags ‖ reserved.
    try testing.expectEqualSlices(u8, &.{ 0x11, 0, 0, 0, 0, 0, 0, 0 }, penc[48..56]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0x80, 0, 0, 0, 0, 0 }, penc[56..64]);
    try testing.expectEqualSlices(u8, &.{ 7, 0, 0, 0, 0, 0, 0, 0 }, penc[64..72]);
    try testing.expectEqualSlices(u8, &.{ 0, 0x20, 0, 0, 100, 0, 0, 0, 0 }, penc[72..81]);
    try testing.expectEqualSlices(u8, &@as([7]u8, @splat(0)), penc[81..88]);
    const pback = try Layout.decode(penc, tgeo);
    try testing.expectEqual(Kind.packed_, pback.kind);
    try testing.expect(pback.durableCopy().?.eql(pk.durable_copy));
}

test "encode ∘ decode = id for every kind and flag shape; exactly the encoder's form decodes" {
    const gpa = testing.allocator;
    const entries = try sortedEntries(gpa, &.{
        .{ .block = 7, .mapping = Mapping.wholeBlock(2, 4 * derive.MiB, 0) },
        .{ .block = 0, .mapping = Mapping.wholeBlock(1, 0, 5) },
        .{ .block = 3, .mapping = Mapping.tenant(1, 8 * derive.MiB, 0, 4096, 4097) },
    });
    defer gpa.free(entries);
    const shapes = [_]Layout{
        Layout.inlineData(0, ""),
        Layout.inlineData(10, "0123456789"),
        Layout.staged(5000, fid(2), null),
        Layout.staged(5000, fid(3), Mapping.tenant(9, 0, 1, 0, 5000)),
        Layout.staged(5000, fid(3), Mapping.wholeBlock(9, 0, 1)),
        Layout.packedTenant(12, fid(4), Mapping.tenant(9, 4 * derive.MiB, 0, 4096 * 3, 12)),
        Layout.stripedInline(40 * derive.MiB, entries),
        Layout.stripedInline(0, &.{}),
        Layout.stripedKvmap(1 << 40, null, null),
        Layout.stripedKvmap(1 << 40, 512, null),
        Layout.stripedKvmap(1 << 40, null, 77),
        Layout.stripedKvmap(1 << 40, 4096, 1),
    };
    for (shapes) |l| {
        const enc = try l.encodeAlloc(gpa);
        defer gpa.free(enc);
        try testing.expectEqual(l.encodedLen(), enc.len);
        const back = try Layout.decode(enc, tgeo);
        const enc2 = try back.encodeAlloc(gpa);
        defer gpa.free(enc2);
        try testing.expectEqualSlices(u8, enc, enc2);
        try testing.expectEqual(l.kind, back.kind);
        try testing.expectEqual(l.flags, back.flags);
        try testing.expectEqual(l.size, back.size);
        // Length refusals: one short, one long.
        try testing.expectError(error.Corrupt, Layout.decode(enc[0 .. enc.len - 1], tgeo));
        const long = try gpa.alloc(u8, enc.len + 1);
        defer gpa.free(long);
        @memcpy(long[0..enc.len], enc);
        long[enc.len] = 0;
        try testing.expectError(error.Corrupt, Layout.decode(long, tgeo));
        // A wrong version, a non-zero base_version on a Put, an unknown kind.
        var bad = try gpa.dupe(u8, enc);
        defer gpa.free(bad);
        bad[1] = 2;
        try testing.expectError(error.Corrupt, Layout.decode(bad, tgeo));
        bad[1] = 1;
        bad[24] = 1;
        try testing.expectError(error.Corrupt, Layout.decode(bad, tgeo));
        bad[24] = 0;
        bad[0] = 4;
        try testing.expectError(error.Corrupt, Layout.decode(bad, tgeo));
    }
    // The kvmap head as fields: the presence of each round-trips.
    const kv_enc = try shapes[11].encodeAlloc(gpa);
    defer gpa.free(kv_enc);
    try testing.expectEqual(@as(usize, 32 + 4 + 8), kv_enc.len);
    const head = (try Layout.decode(kv_enc, tgeo)).kvmapHead().?;
    try testing.expectEqual(@as(?u32, 4096), head.sweep);
    try testing.expectEqual(@as(?u64, 1), head.gen);
    const bare = (try Layout.decode(shapes[8].encodeInto(kv_enc), tgeo)).kvmapHead().?;
    try testing.expectEqual(@as(?u32, null), bare.sweep);
    try testing.expectEqual(@as(?u64, null), bare.gen);
    try testing.expectEqual(@as(?Layout.KvmapHead, null), shapes[6].kvmapHead());
}

test "resolvePoint = decode ∘ lookup on every shape and every block of a 3 GiB file's 768-block map, and it reads only the entry its search lands on: an entry the full decode refuses leaves every other block resolvable and refuses only its own" {
    const gpa = testing.allocator;
    var named: std.ArrayList(MapEntry) = .empty;
    defer named.deinit(gpa);
    var b: u32 = 0;
    // Every third block a hole.
    while (b < 768) : (b += 1) if (b % 3 != 2) try named.append(gpa, .{ .block = b, .mapping = Mapping.wholeBlock(1, @as(u64, b) * DEFAULT_CHUNK, b) });
    const entries = try sortedEntries(gpa, named.items);
    defer gpa.free(entries);
    const shapes = [_]Layout{
        Layout.inlineData(10, "0123456789"),
        Layout.staged(5000, fid(3), Mapping.tenant(9, 0, 1, 0, 5000)),
        Layout.packedTenant(12, fid(4), Mapping.tenant(9, 4 * derive.MiB, 0, 4096 * 3, 12)),
        Layout.stripedInline(768 * DEFAULT_CHUNK, entries),
        Layout.stripedKvmap(1 << 40, 512, 77),
    };
    for (shapes) |l| {
        const enc = try l.encodeAlloc(gpa);
        defer gpa.free(enc);
        const full = try Layout.decode(enc, tgeo);
        var blk: u32 = 0;
        while (blk < 770) : (blk += 1) {
            const p = try resolvePoint(enc, tgeo, blk);
            try testing.expectEqual(full.kind, p.kind);
            try testing.expectEqual(full.size, p.size);
            try testing.expectEqual(full.file_id, p.file_id);
            try testing.expectEqual(full.durableCopy(), p.durable_copy);
            try testing.expectEqual(full.kvmapHead(), p.kvmap);
            try testing.expectEqual(full.lookup(tgeo.chunk, blk), p.binding);
        }
        try testing.expectEqual(@as(?Mapping, null), (try resolvePoint(enc, tgeo, null)).binding);
        try testing.expectError(error.Corrupt, resolvePoint(enc[0 .. enc.len - 1], tgeo, 0));
    }
    const enc = try shapes[3].encodeAlloc(gpa);
    defer gpa.free(enc);
    const at = for (entries, 0..) |e, i| {
        if (e.block() == 400) break LAYOUT_HEADER_LEN + 4 + i * MAP_ENTRY_WIRE_LEN;
    } else unreachable;
    for ([_]usize{ 4, 8 + 8 }) |field| {
        // A reserved byte, then a misaligned device offset.
        enc[at + field] ^= 1;
        defer enc[at + field] ^= 1;
        if (Layout.decode(enc, tgeo)) |_| return error.TestUnexpectedResult else |_| {}
        if (resolvePoint(enc, tgeo, 400)) |_| return error.TestUnexpectedResult else |_| {}
        b = 0;
        while (b < 768) : (b += 1) if (b != 400) try testing.expectEqual(shapes[3].lookup(tgeo.chunk, b), (try resolvePoint(enc, tgeo, b)).binding);
    }
}

test "per-kind flag admissibility, the ascending-map law, the inline ceiling and the mapping window law all refuse typed" {
    const gpa = testing.allocator;
    // inline with any flag ⇒ Corrupt.
    var l = Layout.inlineData(1, "x");
    l.flags = F_HAS_FILE_ID;
    try testing.expectError(error.Corrupt, l.validate(tgeo));
    // staged without HAS_FILE_ID.
    var s = Layout.staged(1, fid(1), null);
    s.flags = 0;
    try testing.expectError(error.Corrupt, s.validate(tgeo));
    // packed with a whole-block "tenant" refuses the mapping class.
    try testing.expectError(error.MappingRefused, Layout.packedTenant(1, fid(1), Mapping.wholeBlock(1, 0, 0)).validate(tgeo));
    // striped: both map bits, neither, sweep without kvmap, gen 0 under MAP_GEN.
    var st = Layout.stripedInline(1, &.{});
    st.flags = F_MAP_INLINE | F_MAP_KVMAP;
    try testing.expectError(error.Corrupt, st.validate(tgeo));
    st.flags = 0;
    try testing.expectError(error.Corrupt, st.validate(tgeo));
    st.flags = F_MAP_INLINE | F_SWEEP_CURSOR;
    try testing.expectError(error.Corrupt, st.validate(tgeo));
    var kv = Layout.stripedKvmap(1, null, 5);
    kv.map_gen = 0;
    try testing.expectError(error.Corrupt, kv.validate(tgeo));
    // Descending / duplicate entries.
    const desc = [_]MapEntryWire{ (MapEntry{ .block = 2, .mapping = Mapping.wholeBlock(1, 0, 0) }).encode(), (MapEntry{ .block = 1, .mapping = Mapping.wholeBlock(1, 0, 0) }).encode() };
    try testing.expectError(error.MapNotAscending, Layout.stripedInline(1, &desc).validate(tgeo));
    const dup = [_]MapEntryWire{ desc[0], desc[0] };
    try testing.expectError(error.MapNotAscending, Layout.stripedInline(1, &dup).validate(tgeo));
    // The inline ceiling at this geometry (60 KiB at the 256 KiB node).
    const big = try gpa.alloc(u8, tgeo.inline_ceiling + 1);
    defer gpa.free(big);
    @memset(big, 0);
    try testing.expectError(error.InlineOverCeiling, Layout.inlineData(1, big).validate(tgeo));
    _ = try Layout.inlineData(1, big[0..tgeo.inline_ceiling]).validate(tgeo);
    // The window law through the record: an unaligned rel_off, an overrun,
    // an unaligned base, an empty tenant, an unknown flag, a reserved byte.
    const chunk = tgeo.chunk;
    try testing.expectError(error.MappingRefused, Mapping.tenant(1, 0, 0, 4095, 1).check(chunk));
    try testing.expectError(error.MappingRefused, Mapping.tenant(1, 0, 0, @intCast(chunk - 4096), 4097).check(chunk));
    try testing.expectError(error.MappingRefused, Mapping.tenant(1, 4096, 0, 0, 1).check(chunk));
    try testing.expectError(error.MappingRefused, Mapping.tenant(1, 0, 0, 0, 0).check(chunk));
    try testing.expectError(error.MappingRefused, (Mapping{ .vol_tag = 1, .whole_block = true, .rel_off = 4096 }).check(chunk));
    _ = try Mapping.tenant(1, 0, 0, @intCast(chunk - 4096), 4096).check(chunk);
    var w = Mapping.tenant(1, 0, 0, 0, 1).encode();
    w.flags = 8;
    try testing.expectError(error.MappingRefused, Mapping.decode(w, chunk));
    w.flags = M_UNWRITTEN; // a tenant window is never a reservation
    try testing.expectError(error.MappingRefused, Mapping.decode(w, chunk));
    w.flags = 0;
    w.reserved[3] = 1;
    try testing.expectError(error.MappingRefused, Mapping.decode(w, chunk));
    // A record carrying a refused mapping refuses with the mapping class.
    var pk = Layout.packedTenant(1, fid(1), Mapping.tenant(1, 0, 0, 0, 1));
    const enc = try pk.encodeAlloc(gpa);
    defer gpa.free(enc);
    enc[72] = 1; // rel_off = 1
    try testing.expectError(error.MappingRefused, Layout.decode(enc, tgeo));
    pk.durable_copy.damaged = true;
    const denc = try pk.encodeAlloc(gpa);
    defer gpa.free(denc);
    // DAMAGED decodes in a record (fsck wrote it) …
    try testing.expect((try Layout.decode(denc, tgeo)).durable_copy.damaged);
}

test "the UNWRITTEN binding (charter §6.6, decision 0003 §BH): flag bit 2 on the 40 B wire, whole-block only and never DAMAGED, encode ∘ decode = id, the same reference as its written twin (no ±ref on the flip), refused at the read choke point with the tripwire; a publish naming a fresh UNWRITTEN block needs no proof, naming its WRITTEN twin later needs none either (same reference), and a shift's re-keyed references need none" {
    const gpa = testing.allocator;
    const chunk = tgeo.chunk;
    const u = Mapping.unwrittenBlock(0x11, 5 * chunk, 9);
    const w = u.encode();
    try testing.expectEqual(M_WHOLE_BLOCK | M_UNWRITTEN, w.flags);
    const back = try Mapping.decode(w, chunk);
    try testing.expect(back.unwritten and back.whole_block and !back.damaged);
    try testing.expect(back.eql(u));
    try testing.expect(u.sameRef(u.withUnwritten(false), chunk));
    try testing.expect(u.withUnwritten(false).eql(Mapping.wholeBlock(0x11, 5 * chunk, 9)));
    // The law: a tenant window is never unwritten; neither is a quarantine.
    var t_ = Mapping.tenant(0x11, 0, 0, 0, 100);
    t_.unwritten = true;
    try testing.expectError(error.MappingRefused, t_.check(chunk));
    var d = u;
    d.damaged = true;
    try testing.expectError(error.MappingRefused, d.check(chunk));
    try testing.expectError(error.MappingRefused, Mapping.decode(d.encode(), chunk));
    // The choke point refuses it, counted (the tripwire's one line).
    const fired = stats.unwritten_mapping_served.load();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.MappingRefused, u.readWindow(chunk, null));
    try log.testing.expectAllFired();
    try testing.expectEqual(fired + 1, stats.unwritten_mapping_served.load());
    // The kvmap's kind-2 form carries it (a flag the run forms cannot spell).
    const kvmap = @import("kvmap.zig");
    const rec = kvmap.Rec.point(3, u);
    try testing.expect(rec.special != null);
    var buf: [MAPPING_WIRE_LEN]u8 = undefined;
    const rt = try kvmap.Rec.fromEntry(3, rec.entryInto(&buf), chunk);
    try testing.expect(rt.bindingAt(3, chunk).eql(u));
    // publishLayout: a reservation needs no proof; its +ref rides the tx.
    var refs: TestRefs = .{};
    defer refs.deinit();
    const seed: key.NameKey = .init(0x5EED);
    const ino: u64 = 42;
    const reserved = try sortedEntries(gpa, &.{
        .{ .block = 0, .mapping = Mapping.wholeBlock(0x11, 7 * chunk, 1) },
        .{ .block = 2, .mapping = u },
    });
    defer gpa.free(reserved);
    const base = try sortedEntries(gpa, &.{.{ .block = 0, .mapping = Mapping.wholeBlock(0x11, 7 * chunk, 1) }});
    defer gpa.free(base);
    const prev = Layout.stripedInline(chunk, base);
    const with_res = Layout.stripedInline(chunk, reserved);
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        try publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = prev, .layout = with_res, .proofs = &.{} }, refs.seam(), tgeo);
        try testing.expectEqual(@as(usize, 1), refs.adds.items.len);
        try testing.expectEqual(RefOp{ .vol_tag = 0x11, .block_idx = 5, .owner_ino = ino, .block_index = 2 }, refs.adds.items[0]);
        try testing.expectEqual(@as(usize, 0), refs.releases.items.len);
    }
    // The landing's flip to WRITTEN: the same reference, no proof, no ref op.
    const written = try sortedEntries(gpa, &.{
        .{ .block = 0, .mapping = Mapping.wholeBlock(0x11, 7 * chunk, 1) },
        .{ .block = 2, .mapping = u.withUnwritten(false) },
    });
    defer gpa.free(written);
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        refs.adds.clearRetainingCapacity();
        try publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = with_res, .layout = Layout.stripedInline(3 * chunk, written), .proofs = &.{} }, refs.seam(), tgeo);
        try testing.expectEqual(@as(usize, 0), refs.adds.items.len);
        try testing.expectEqual(@as(usize, 0), refs.releases.items.len);
        try testing.expectEqual(@as(usize, 1), t.staged.items.len);
    }
    // ZERO_RANGE's flip back to UNWRITTEN: the same.
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        try publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = Layout.stripedInline(3 * chunk, written), .layout = with_res, .proofs = &.{} }, refs.seam(), tgeo);
        try testing.expectEqual(@as(usize, 0), refs.adds.items.len);
        try testing.expectEqual(@as(usize, 0), refs.releases.items.len);
    }
    // A COLLAPSE of block 0 of {0, 1, 2}: {1, 2} become {0, 1} — without
    // the shift arm every re-keyed reference wants a proof; with it none
    // does, block 0's reference is released and the moved ones re-keyed.
    const three = try sortedEntries(gpa, &.{
        .{ .block = 0, .mapping = Mapping.wholeBlock(0x11, 7 * chunk, 1) },
        .{ .block = 1, .mapping = Mapping.wholeBlock(0x11, 8 * chunk, 1) },
        .{ .block = 2, .mapping = Mapping.wholeBlock(0x22, 1 * chunk, 1) },
    });
    defer gpa.free(three);
    const shifted = try sortedEntries(gpa, &.{
        .{ .block = 0, .mapping = Mapping.wholeBlock(0x11, 8 * chunk, 1) },
        .{ .block = 1, .mapping = Mapping.wholeBlock(0x22, 1 * chunk, 1) },
    });
    defer gpa.free(shifted);
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        try testing.expectError(error.ProofMissing, publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = Layout.stripedInline(3 * chunk, three), .layout = Layout.stripedInline(2 * chunk, shifted), .proofs = &.{} }, refs.seam(), tgeo));
        refs.adds.clearRetainingCapacity();
        refs.releases.clearRetainingCapacity();
        try publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = Layout.stripedInline(3 * chunk, three), .layout = Layout.stripedInline(2 * chunk, shifted), .proofs = &.{}, .shift = true }, refs.seam(), tgeo);
        try testing.expectEqual(@as(usize, 2), refs.adds.items.len);
        try testing.expectEqual(@as(usize, 3), refs.releases.items.len);
        try testing.expectEqual(RefOp{ .vol_tag = 0x11, .block_idx = 8, .owner_ino = ino, .block_index = 0 }, refs.adds.items[0]);
        try testing.expectEqual(RefOp{ .vol_tag = 0x22, .block_idx = 1, .owner_ino = ino, .block_index = 1 }, refs.adds.items[1]);
        try testing.expectEqual(RefOp{ .vol_tag = 0x11, .block_idx = 7, .owner_ino = ino, .block_index = 0 }, refs.releases.items[0]);
        // A reference the old layout never named still needs its proof under the shift arm.
        const alien = try sortedEntries(gpa, &.{
            .{ .block = 0, .mapping = Mapping.wholeBlock(0x11, 8 * chunk, 1) },
            .{ .block = 1, .mapping = Mapping.wholeBlock(0x33, 1 * chunk, 1) },
        });
        defer gpa.free(alien);
        try testing.expectError(error.ProofMissing, publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = Layout.stripedInline(3 * chunk, three), .layout = Layout.stripedInline(2 * chunk, alien), .proofs = &.{}, .shift = true }, refs.seam(), tgeo));
    }
}

test "mapping law ties: ceil(len) = pack slot — 1 → 4096, 4096 → 4096, 4097 → 8192; the window read is grain-outward on the mapping's own volume, never the block prefix" {
    try testing.expectEqual(@as(u64, 4096), ceilGrain(1));
    try testing.expectEqual(@as(u64, 4096), ceilGrain(0));
    try testing.expectEqual(@as(u64, 4096), ceilGrain(4096));
    try testing.expectEqual(@as(u64, 8192), ceilGrain(4097));
    // A transformed volume's chunk is not a power of two (block + 4 KiB):
    // its mappings check by modulus, never by the power-of-two helper.
    const xchunk: u64 = 4 * derive.MiB + 4096;
    try Mapping.wholeBlock(0x77, 3 * xchunk, 0).check(xchunk);
    try testing.expectError(error.MappingRefused, Mapping.wholeBlock(0x77, 3 * xchunk + 4096, 0).check(xchunk));
    const m = Mapping.tenant(0x77, 8 * derive.MiB, 0, 12288, 10_000);
    try testing.expectEqual(@as(u64, 2), m.blockIdx(tgeo.chunk));
    try testing.expectEqual(@as(u64, 12288), m.slotLen(tgeo.chunk));
    const whole = try m.readWindow(tgeo.chunk, null);
    try testing.expectEqual(@as(u64, 0x77), whole.vol_tag);
    try testing.expectEqual(8 * derive.MiB + 12288, whole.device_off);
    try testing.expectEqual(@as(u64, 12288), whole.len);
    try testing.expectEqual(@as(usize, 0), whole.lo);
    try testing.expectEqual(@as(usize, 10_000), whole.hi);
    // A sub-window [5000, 6000) ⇒ grain window [4096, 8192) at +4096.
    const sub = try m.readWindow(tgeo.chunk, .{ .start = 5000, .end = 6000 });
    try testing.expectEqual(8 * derive.MiB + 12288 + 4096, sub.device_off);
    try testing.expectEqual(@as(u64, 4096), sub.len);
    try testing.expectEqual(@as(usize, 904), sub.lo);
    try testing.expectEqual(@as(usize, 1904), sub.hi);
    // Past the image: clipped to it.
    const tail = try m.readWindow(tgeo.chunk, .{ .start = 9000, .end = 20_000 });
    try testing.expectEqual(@as(u64, 4096), tail.len);
    try testing.expectEqual(@as(usize, 808), tail.lo);
    try testing.expectEqual(@as(usize, 1808), tail.hi);
    // A whole block reads the chunk.
    const wb = try Mapping.wholeBlock(1, tgeo.chunk, 0).readWindow(tgeo.chunk, null);
    try testing.expectEqual(tgeo.chunk, wb.len);
    try testing.expectEqual(tgeo.chunk, wb.device_off);
    // DAMAGED refuses EIO at the choke point and trips the wire.
    const before = stats.packed_mapping_refusals.load();
    var d = m;
    d.damaged = true;
    log.testing.expectErrorLogs(1);
    stats.packed_mapping_refusals.resetLogBudgetForTest();
    try testing.expectError(error.MappingDamaged, d.readWindow(tgeo.chunk, null));
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 1, stats.packed_mapping_refusals.load());
    // The clip: same reference, zero writes.
    const clipped = m.clip(100);
    try testing.expect(clipped.sameRef(m, tgeo.chunk));
    try testing.expectEqual(@as(u32, 100), clipped.len);
    try testing.expectEqual(m.rel_off, clipped.rel_off);
}

test "the field table is derived from the wire header plus the body rows: 10 rows, bit i = row i, header widths = wire widths; the inode delta magic lies outside" {
    try testing.expectEqual(@as(usize, 10), layout_field_table.len);
    try testing.expectEqualStrings("layout_kind", layout_field_table[0].name);
    try testing.expectEqualStrings("flags", layout_field_table[1].name);
    try testing.expectEqualStrings("size", layout_field_table[2].name);
    try testing.expectEqualStrings("layout_version", layout_field_table[3].name);
    try testing.expectEqualStrings("base_version", layout_field_table[4].name);
    try testing.expectEqualStrings("file_id", layout_field_table[5].name);
    try testing.expectEqualStrings("durable_copy", layout_field_table[6].name);
    try testing.expectEqualStrings("sweep_cursor", layout_field_table[7].name);
    try testing.expectEqualStrings("map_gen", layout_field_table[8].name);
    try testing.expectEqualStrings("map_inserts", layout_field_table[9].name);
    var acc: u16 = 0;
    for (layout_field_table, 0..) |f, i| {
        try testing.expectEqual(@as(u16, 1) << @intCast(i), f.bit);
        acc |= f.bit;
    }
    try testing.expectEqual(DELTA_MASK_ALL, acc);
    try testing.expectEqual(@as(usize, 1), layout_field_table[0].width);
    try testing.expectEqual(@as(usize, 2), layout_field_table[1].width);
    try testing.expectEqual(@as(usize, 8), layout_field_table[4].width);
    try testing.expectEqual(@as(usize, 40), layout_field_table[6].width);
    try testing.expectEqual(@as(usize, 0), layout_field_table[9].width);
    // A legacy 0x4C31 payload refuses as a layout delta.
    try testing.expectError(error.Corrupt, LayoutDelta.decode(&.{ 0x31, 0x4C, 0 }, tgeo));
}

test "layout delta: encode ∘ decode = id, the claim rides iff a version, inserts ascending, every refusal; the fold overwrites fields, inserts replace, and is idempotent" {
    const gpa = testing.allocator;
    const ins = try sortedEntries(gpa, &.{
        .{ .block = 5, .mapping = Mapping.wholeBlock(2, 8 * derive.MiB, 3) },
        .{ .block = 1, .mapping = Mapping.wholeBlock(2, 4 * derive.MiB, 2) },
    });
    defer gpa.free(ins);
    const d = LayoutDelta.mapInserts(24 * derive.MiB, ins, 0, 0x1_0000_0001);
    try testing.expectEqual(@as(u16, DELTA_SIZE | DELTA_MAP_INSERTS | DELTA_LAYOUT_VERSION | DELTA_BASE_VERSION), d.mask);
    const enc = try d.encodeAlloc(gpa);
    defer gpa.free(enc);
    try testing.expectEqual(@as(usize, 2 + 8 + 8 + 8 + 4 + 2 * 48), enc.len);
    try testing.expectEqualSlices(u8, &.{ 0x1C, 0x02 }, enc[0..2]);
    const back = try LayoutDelta.decode(enc, tgeo);
    try testing.expectEqual(d.mask, back.mask);
    try testing.expectEqual(d.fields.size, back.fields.size);
    try testing.expectEqual(d.fields.layout_version, back.fields.layout_version);
    try testing.expectEqual(@as(usize, 2), back.inserts.len);
    const enc2 = try back.encodeAlloc(gpa);
    defer gpa.free(enc2);
    try testing.expectEqualSlices(u8, enc, enc2);
    // Refusals: zero mask, unknown bit, a claim without a version, a version
    // without a claim, truncation, trailing bytes, descending inserts.
    try testing.expectError(error.Corrupt, LayoutDelta.decode(&.{ 0, 0 }, tgeo));
    try testing.expectError(error.Corrupt, LayoutDelta.decode(&.{ 0, 0x04 }, tgeo));
    try testing.expectError(error.Corrupt, LayoutDelta.decode(&([_]u8{ 0x10, 0 } ++ @as([8]u8, @splat(0))), tgeo));
    try testing.expectError(error.Corrupt, LayoutDelta.decode(&([_]u8{ 0x08, 0 } ++ [_]u8{1} ++ @as([7]u8, @splat(0))), tgeo));
    try testing.expectError(error.Corrupt, LayoutDelta.decode(enc[0 .. enc.len - 1], tgeo));
    const long = try gpa.alloc(u8, enc.len + 1);
    defer gpa.free(long);
    @memcpy(long[0..enc.len], enc);
    long[enc.len] = 0;
    try testing.expectError(error.Corrupt, LayoutDelta.decode(long, tgeo));
    var desc = try gpa.dupe(u8, enc);
    defer gpa.free(desc);
    // Swap the two entries' block numbers (1 ↔ 5).
    desc[30] = 5;
    desc[78] = 1;
    try testing.expectError(error.MapNotAscending, LayoutDelta.decode(desc, tgeo));
    // The size-only delta is 10 B.
    const so = LayoutDelta.sizeOnly(99);
    try testing.expectEqual(@as(usize, 10), so.encodedLen());

    // Fold onto a striped inline base: insert 1 and 5 around existing 3,
    // replace an existing 5, keep the size, re-fold idempotently.
    const base_entries = try sortedEntries(gpa, &.{
        .{ .block = 3, .mapping = Mapping.wholeBlock(1, 0, 1) },
        .{ .block = 5, .mapping = Mapping.wholeBlock(1, 4 * derive.MiB, 1) },
    });
    defer gpa.free(base_entries);
    const base = Layout.stripedInline(20 * derive.MiB, base_entries);
    var scratch: std.ArrayList(MapEntryWire) = .empty;
    defer scratch.deinit(gpa);
    const folded = try d.applyAlloc(gpa, base, &scratch, tgeo);
    try testing.expectEqual(@as(usize, 3), folded.entries.len);
    try testing.expectEqual(@as(u32, 1), folded.entries[0].block());
    try testing.expectEqual(@as(u32, 3), folded.entries[1].block());
    try testing.expectEqual(@as(u32, 5), folded.entries[2].block());
    try testing.expectEqual(@as(u64, 8 * derive.MiB), folded.lookup(tgeo.chunk, 5).?.offset);
    try testing.expectEqual(24 * derive.MiB, folded.size);
    try testing.expectEqual(@as(u64, 0x1_0000_0001), folded.layout_version);
    try testing.expectEqual(@as(u64, 0), folded.base_version);
    const once = try folded.encodeAlloc(gpa);
    defer gpa.free(once);
    var scratch2: std.ArrayList(MapEntryWire) = .empty;
    defer scratch2.deinit(gpa);
    const twice = try (try d.applyAlloc(gpa, folded, &scratch2, tgeo)).encodeAlloc(gpa);
    defer gpa.free(twice);
    try testing.expectEqualSlices(u8, once, twice);
    // Inserts onto a kvmap head or a non-striped layout refuse typed; a
    // delta whose flags its body cannot carry is inconsistent.
    try testing.expectError(error.DeltaOntoNonInlineMap, d.applyAlloc(gpa, Layout.stripedKvmap(1, null, null), &scratch, tgeo));
    try testing.expectError(error.DeltaOntoNonInlineMap, d.applyAlloc(gpa, Layout.inlineData(1, "x"), &scratch, tgeo));
    const badflags = LayoutDelta{ .mask = DELTA_FLAGS, .fields = .{ .kind = .inline_, .flags = F_MAP_KVMAP } };
    try testing.expectError(error.DeltaInconsistent, badflags.applyAlloc(gpa, Layout.inlineData(1, "x"), &scratch, tgeo));
    // A kind flip by delta: staged → packed with the tenant as durable_copy.
    const flip = LayoutDelta{ .mask = DELTA_LAYOUT_KIND | DELTA_FLAGS | DELTA_DURABLE_COPY, .fields = .{ .kind = .packed_, .flags = F_HAS_FILE_ID, .durable_copy = Mapping.tenant(1, 0, 0, 0, 10) } };
    const pk = try flip.applyAlloc(gpa, Layout.staged(10, fid(9), null), &scratch, tgeo);
    try testing.expectEqual(Kind.packed_, pk.kind);
    try testing.expectEqualSlices(u8, &fid(9), &pk.file_id);
    try testing.expect(pk.durableCopy().?.eql(Mapping.tenant(1, 0, 0, 0, 10)));
}

test "foldChain: oldest-first links fold to the from-scratch layout; versioned links claim the previous version (first claims 0); a divergent or heterogeneous chain refuses loud" {
    const gpa = testing.allocator;
    const base = try Layout.stripedInline(0, &.{}).encodeAlloc(gpa);
    defer gpa.free(base);
    const e1 = try sortedEntries(gpa, &.{.{ .block = 0, .mapping = Mapping.wholeBlock(1, 0, 1) }});
    defer gpa.free(e1);
    const e2 = try sortedEntries(gpa, &.{.{ .block = 1, .mapping = Mapping.wholeBlock(1, 4 * derive.MiB, 2) }});
    defer gpa.free(e2);
    const d1 = try LayoutDelta.mapInserts(4 * derive.MiB, e1, 0, 10).encodeAlloc(gpa);
    defer gpa.free(d1);
    const d2 = try LayoutDelta.mapInserts(8 * derive.MiB, e2, 10, 11).encodeAlloc(gpa);
    defer gpa.free(d2);
    const folded = try foldChain(gpa, base, &.{ d1, d2 }, tgeo);
    defer gpa.free(folded);
    const l = try Layout.decode(folded, tgeo);
    try testing.expectEqual(@as(usize, 2), l.entries.len);
    try testing.expectEqual(8 * derive.MiB, l.size);
    try testing.expectEqual(@as(u64, 11), l.layout_version);
    // The same records as one scratch-built Put: byte-identical.
    const both = try sortedEntries(gpa, &.{ .{ .block = 0, .mapping = Mapping.wholeBlock(1, 0, 1) }, .{ .block = 1, .mapping = Mapping.wholeBlock(1, 4 * derive.MiB, 2) } });
    defer gpa.free(both);
    var want = Layout.stripedInline(8 * derive.MiB, both);
    want.layout_version = 11;
    const want_enc = try want.encodeAlloc(gpa);
    defer gpa.free(want_enc);
    try testing.expectEqualSlices(u8, want_enc, folded);
    // Divergent: a link claiming 10 when the head is 11.
    const fork = try LayoutDelta.mapInserts(8 * derive.MiB, e2, 10, 12).encodeAlloc(gpa);
    defer gpa.free(fork);
    try testing.expectError(error.LayoutChainDivergent, foldChain(gpa, base, &.{ d1, d2, fork }, tgeo));
    // A first versioned link claiming non-zero.
    try testing.expectError(error.LayoutChainDivergent, foldChain(gpa, base, &.{d2}, tgeo));
    // Heterogeneous: an unversioned link after a versioned one, and vice versa.
    const unv = try LayoutDelta.mapInserts(8 * derive.MiB, e2, 0, 0).encodeAlloc(gpa);
    defer gpa.free(unv);
    try testing.expectError(error.LayoutChainHeterogeneous, foldChain(gpa, base, &.{ d1, unv }, tgeo));
    try testing.expectError(error.LayoutChainHeterogeneous, foldChain(gpa, base, &.{ unv, d1 }, tgeo));
    // An all-unversioned chain folds.
    const unv1 = try LayoutDelta.mapInserts(4 * derive.MiB, e1, 0, 0).encodeAlloc(gpa);
    defer gpa.free(unv1);
    const f2 = try foldChain(gpa, base, &.{ unv1, unv }, tgeo);
    defer gpa.free(f2);
    try testing.expectEqual(@as(usize, 2), (try Layout.decode(f2, tgeo)).entries.len);
}

test "delta policy: 64 links then a re-base (counted); 0 = full Puts only; the publish window flushes at the coalesce max, sorts, counts batches and blocks; ≤ 1 serializes" {
    const p: DeltaPolicy = .{ .max_chain = 64 };
    const before = stats.layout_delta_rebases.load();
    try testing.expect(p.useDelta(0));
    try testing.expect(p.useDelta(63));
    try testing.expect(!p.useDelta(64));
    try testing.expectEqual(before + 1, stats.layout_delta_rebases.load());
    try testing.expect(!(DeltaPolicy{ .max_chain = 0 }).useDelta(0));
    // The registry defaults are what the derivation says.
    try testing.expectEqual(@as(u32, 64), knobs.intDefault("SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN", u32));
    try testing.expectEqual(@as(u32, 64), knobs.intDefault("SQUEEZEFS_PUBLISH_COALESCE_MAX", u32));
    const gpa = testing.allocator;
    var w: PublishWindow = .{ .max = 3 };
    defer w.deinit(gpa);
    try testing.expect(!try w.push(gpa, .{ .block = 9, .mapping = Mapping.wholeBlock(1, 0, 0) }));
    try testing.expect(!try w.push(gpa, .{ .block = 2, .mapping = Mapping.wholeBlock(1, 0, 0) }));
    try testing.expect(try w.push(gpa, .{ .block = 5, .mapping = Mapping.wholeBlock(1, 0, 0) }));
    const b0 = stats.layout_publish_batches.load();
    const k0 = stats.layout_publish_batched_blocks.load();
    const taken = w.take();
    try testing.expectEqual(@as(usize, 3), taken.len);
    try testing.expectEqual(@as(u32, 2), taken[0].block());
    try testing.expectEqual(@as(u32, 9), taken[2].block());
    try testing.expectEqual(b0 + 1, stats.layout_publish_batches.load());
    try testing.expectEqual(k0 + 3, stats.layout_publish_batched_blocks.load());
    w.clear();
    try testing.expectEqual(@as(usize, 0), w.pending.items.len);
    var serial: PublishWindow = .{ .max = 0 };
    defer serial.deinit(gpa);
    try testing.expect(try serial.push(gpa, .{ .block = 1, .mapping = Mapping.wholeBlock(1, 0, 0) }));
}

test "inline bound tie: the default is one page at every node size; the bound is the KV value cap minus the 4 KiB headroom (60 KiB at 256 KiB, 12 KiB at 64 KiB); an override above the bound refuses the mount; the gauge publishes" {
    const facts = derive.Facts.synthetic(8, 64 * derive.GiB);
    try testing.expectEqual(@as(u64, 4096), derivedInlineMax(facts, 256 * derive.KiB));
    try testing.expectEqual(@as(u64, 4096), derivedInlineMax(facts, 64 * derive.KiB));
    try testing.expectEqual(@as(u64, 4096), derivedInlineMax(facts, 1 * derive.MiB));
    try testing.expectEqual(@as(u64, 60 * derive.KiB), inlineMaxCeiling(256 * derive.KiB));
    try testing.expectEqual(@as(u64, 12 * derive.KiB), inlineMaxCeiling(64 * derive.KiB));
    try testing.expectEqual(@as(u64, 28 * derive.KiB), inlineMaxCeiling(128 * derive.KiB));
    try testing.expectEqual(INLINE_MAX_CEILING, inlineMaxCeiling(1 * derive.MiB));
    try testing.expectEqual(@as(u64, 61_440), INLINE_MAX_CEILING);
    // The registry row's range top IS the ceiling.
    try testing.expectEqual(@as(i128, INLINE_MAX_CEILING), knobs.row("SQUEEZEFS_INLINE_MAX_BYTES").kind.int.hi);
    try testing.expectEqual(@as(i128, INLINE_MAX_FLOOR), knobs.row("SQUEEZEFS_INLINE_MAX_BYTES").kind.int.lo);
    // Explicit verbatim up to the bound.
    try testing.expectEqual(@as(u64, 16 * derive.KiB), try resolveInlineMax(16 * derive.KiB, facts, 256 * derive.KiB));
    try testing.expectEqual(@as(u64, 60 * derive.KiB), try resolveInlineMax(60 * derive.KiB, facts, 256 * derive.KiB));
    try testing.expectEqual(@as(u64, 60 * derive.KiB), stats.inline_max_bytes.load());
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.InlineMaxAboveFormatBound, resolveInlineMax(16 * derive.KiB, facts, 64 * derive.KiB));
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u64, 4096), try resolveInlineMax(null, facts, 64 * derive.KiB));
    // A 16 KiB page machine derives 16 KiB (the page is the unit) but never above the bound.
    var big_page = facts;
    big_page.page_size = 16384;
    try testing.expectEqual(@as(u64, 16384), derivedInlineMax(big_page, 256 * derive.KiB));
    try testing.expectEqual(@as(u64, 12 * derive.KiB), derivedInlineMax(big_page, 64 * derive.KiB));
}

test "the transition table: fresh → inline/staged/striped; inline → staged/striped; staged → anything; packed → anything; striped terminal; same kind always" {
    const kinds = [_]Kind{ .inline_, .staged, .packed_, .striped };
    for (kinds) |to| try testing.expectEqual(to != .packed_, validTransition(null, to));
    for (kinds) |k| try testing.expect(validTransition(k, k));
    try testing.expect(validTransition(.inline_, .staged));
    try testing.expect(validTransition(.inline_, .striped));
    try testing.expect(!validTransition(.inline_, .packed_));
    for (kinds) |to| try testing.expect(validTransition(.staged, to));
    for (kinds) |to| try testing.expect(validTransition(.packed_, to));
    try testing.expect(!validTransition(.striped, .inline_));
    try testing.expect(!validTransition(.striped, .staged));
    try testing.expect(!validTransition(.striped, .packed_));
}

const TestRefs = struct {
    adds: std.ArrayList(RefOp) = .empty,
    releases: std.ArrayList(RefOp) = .empty,

    fn seam(self: *TestRefs) RefsSeam {
        return .{ .ctx = self, .stage_add = add, .stage_release = release };
    }

    fn add(ctx: *anyopaque, _: *tx.KvTx, op: RefOp) Allocator.Error!void {
        const self: *TestRefs = @ptrCast(@alignCast(ctx));
        try self.adds.append(testing.allocator, op);
    }

    fn release(ctx: *anyopaque, _: *tx.KvTx, op: RefOp) Allocator.Error!void {
        const self: *TestRefs = @ptrCast(@alignCast(ctx));
        try self.releases.append(testing.allocator, op);
    }

    fn deinit(self: *TestRefs) void {
        self.adds.deinit(testing.allocator);
        self.releases.deinit(testing.allocator);
    }
};

test "publishLayout stages ONE xattr Put of the record and the ±ref diff in the same tx; a new block needs its DMA proof; a clip and a same-reference swap stage no ref op; the transition law is enforced" {
    const gpa = testing.allocator;
    var refs: TestRefs = .{};
    defer refs.deinit();
    const seed: key.NameKey = .init(0x5EED);
    const ino: u64 = 42;
    const chunk = tgeo.chunk;
    // A fresh staged file: no refs.
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        try publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = null, .layout = Layout.staged(100, fid(1), null), .proofs = &.{} }, refs.seam(), tgeo);
        try testing.expectEqual(@as(usize, 1), t.staged.items.len);
        try testing.expectEqual(key.TREE_XATTRS, t.staged.items[0].tree_id);
        const xk = try key.XattrKey.decode(t.staged.items[0].key);
        try testing.expectEqual(ino, xk.ino());
        try testing.expectEqual(key.xattrNameHash56(LAYOUT_XATTR_NAME, &seed), xk.hash56());
        const l = try XattrLayout.decode(t.staged.items[0].value, tgeo);
        try testing.expectEqual(Kind.staged, l.kind);
        try testing.expectEqual(@as(usize, 0), refs.adds.items.len);
    }
    // Promotion to packed: the tenant is a NEW block ⇒ a proof is required; +ref staged.
    const tenant = Mapping.tenant(0x11, 3 * chunk, 0, 8192, 100);
    const pk = Layout.packedTenant(100, fid(1), tenant);
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        try testing.expectError(error.ProofMissing, publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = Layout.staged(100, fid(1), null), .layout = pk, .proofs = &.{} }, refs.seam(), tgeo));
        // A proof for the wrong window is no proof.
        const wrong: DmaProof = .{ .vol_tag = 0x11, .offset = 3 * chunk, .len = 8192, .completed_ns = 1 };
        try testing.expectError(error.ProofMissing, publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = Layout.staged(100, fid(1), null), .layout = pk, .proofs = &.{wrong} }, refs.seam(), tgeo));
        const proof: DmaProof = .{ .vol_tag = 0x11, .offset = 3 * chunk + 8192, .len = 4096, .completed_ns = 1 };
        try testing.expect(proof.covers(tenant, chunk));
        try publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = Layout.staged(100, fid(1), null), .layout = pk, .proofs = &.{proof} }, refs.seam(), tgeo);
        try testing.expectEqual(@as(usize, 1), refs.adds.items.len);
        try testing.expectEqual(RefOp{ .vol_tag = 0x11, .block_idx = 3, .owner_ino = ino, .block_index = 0 }, refs.adds.items[0]);
        try testing.expectEqual(@as(usize, 0), refs.releases.items.len);
    }
    // A clip of the tenant: same reference ⇒ no ref ops, one record.
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        const n = refs.adds.items.len;
        try publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = pk, .layout = Layout.packedTenant(50, fid(1), tenant.clip(50)), .proofs = &.{} }, refs.seam(), tgeo);
        try testing.expectEqual(n, refs.adds.items.len);
        try testing.expectEqual(@as(usize, 0), refs.releases.items.len);
        try testing.expectEqual(@as(usize, 1), t.staged.items.len);
    }
    // Growth to striped: the tenant's block is displaced (−ref), two new blocks (+ref ×2, both proven).
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        const entries = try sortedEntries(gpa, &.{
            .{ .block = 0, .mapping = Mapping.wholeBlock(0x11, 7 * chunk, 1) },
            .{ .block = 1, .mapping = Mapping.wholeBlock(0x22, 2 * chunk, 1) },
        });
        defer gpa.free(entries);
        const striped = Layout.stripedInline(5 * derive.MiB, entries);
        const proofs = [_]DmaProof{
            .{ .vol_tag = 0x11, .offset = 7 * chunk, .len = chunk, .completed_ns = 2 },
            .{ .vol_tag = 0x22, .offset = 2 * chunk, .len = chunk, .completed_ns = 2 },
        };
        try testing.expectError(error.ProofMissing, publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = pk, .layout = striped, .proofs = proofs[0..1] }, refs.seam(), tgeo));
        // A whole-block DMA is the complete payload or image by construction
        // (a transformed image is self-delimiting; a passthrough payload is
        // bounded by the layout's size), so a proof from the slot's start of
        // ANY grain-padded length covers the mapping — a compressed block's
        // image is shorter than the chunk, and the old whole-slot law refused
        // every block that compressed (the publish retried forever).
        const short_image: DmaProof = .{ .vol_tag = 0x22, .offset = 2 * chunk, .len = 1 << 20, .completed_ns = 2 };
        try testing.expect(short_image.covers(Mapping.wholeBlock(0x22, 2 * chunk, 1), chunk));
        try testing.expect(!(DmaProof{ .vol_tag = 0x22, .offset = 2 * chunk + 4096, .len = chunk, .completed_ns = 2 }).covers(Mapping.wholeBlock(0x22, 2 * chunk, 1), chunk));
        try testing.expect(!(DmaProof{ .vol_tag = 0x22, .offset = 2 * chunk, .len = 100, .completed_ns = 2 }).covers(Mapping.wholeBlock(0x22, 2 * chunk, 1), chunk));
        // A tenant's proof still has to cover its whole slot.
        try testing.expect(!(DmaProof{ .vol_tag = 0x11, .offset = 3 * chunk + 8192, .len = 2048, .completed_ns = 1 }).covers(tenant, chunk));
        refs.adds.clearRetainingCapacity();
        try publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = pk, .layout = striped, .proofs = &.{ proofs[0], short_image } }, refs.seam(), tgeo);
        try testing.expectEqual(@as(usize, 2), refs.adds.items.len);
        try testing.expectEqual(@as(usize, 1), refs.releases.items.len);
        try testing.expectEqual(RefOp{ .vol_tag = 0x11, .block_idx = 3, .owner_ino = ino, .block_index = 0 }, refs.releases.items[0]);
        // Striped is terminal.
        try testing.expectError(error.InvalidTransition, publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = striped, .layout = Layout.inlineData(1, "x"), .proofs = &.{} }, refs.seam(), tgeo));
        // A fresh file cannot be packed.
        try testing.expectError(error.InvalidTransition, publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = null, .layout = pk, .proofs = &.{} }, refs.seam(), tgeo));
        // An inadmissible layout refuses before anything is staged.
        var bad = Layout.inlineData(1, "x");
        bad.flags = F_MAP_GEN;
        const staged_before = t.staged.items.len;
        try testing.expectError(error.Corrupt, publishLayout(&t, .{ .ino = ino, .name_key = &seed, .prev = null, .layout = bad, .proofs = &.{} }, refs.seam(), tgeo));
        try testing.expectEqual(staged_before, t.staged.items.len);
    }
}

test "property: the publish's merge-walk diff ≡ the per-entry search (+ref for every reference the new layout names and the old does not, −ref for the converse, a same-reference rebind neither) over random inline maps, a staged durable copy against a map, and a fresh file; the walk decodes only the differing entries" {
    const gpa = testing.allocator;
    const seed: key.NameKey = .init(7);
    const chunk = tgeo.chunk;
    var prng = std.Random.DefaultPrng.init(0xD1FF);
    const r = prng.random();
    // The reference: does `l` name the same reference as `e`? (the search
    // the walk replaced).
    const Naive = struct {
        fn names(l: ?Layout, e: MapEntry, c: u64) bool {
            const lay = l orelse return false;
            if (e.block == 0) if (lay.durableCopy()) |m| if (m.sameRef(e.mapping, c)) return true;
            const m = lay.lookup(c, e.block) orelse return false;
            return m.sameRef(e.mapping, c);
        }
    };
    var round: usize = 0;
    while (round < 200) : (round += 1) {
        // Two maps over blocks 0..15: each block absent, or bound to one of
        // three volumes at a random block offset (a small range so rebinds
        // to the same reference happen).
        var old_list: std.ArrayList(MapEntry) = .empty;
        defer old_list.deinit(gpa);
        var new_list: std.ArrayList(MapEntry) = .empty;
        defer new_list.deinit(gpa);
        var b: u32 = 0;
        while (b < 16) : (b += 1) {
            if (r.uintLessThan(u8, 4) != 0) try old_list.append(gpa, .{ .block = b, .mapping = Mapping.wholeBlock(0x10 + r.uintLessThan(u64, 3), chunk * r.uintLessThan(u64, 6), 1) });
            if (r.uintLessThan(u8, 4) != 0) try new_list.append(gpa, .{ .block = b, .mapping = Mapping.wholeBlock(0x10 + r.uintLessThan(u64, 3), chunk * r.uintLessThan(u64, 6), 2) });
        }
        const old_entries = try sortedEntries(gpa, old_list.items);
        defer gpa.free(old_entries);
        const new_entries = try sortedEntries(gpa, new_list.items);
        defer gpa.free(new_entries);
        const shape = round % 3;
        const prev: ?Layout = switch (shape) {
            0 => Layout.stripedInline(16 * chunk, old_entries),
            1 => Layout.staged(100, fid(1), Mapping.tenant(0x10 + r.uintLessThan(u64, 3), chunk * r.uintLessThan(u64, 6), 1, 0, 100)),
            else => null,
        };
        const new = Layout.stripedInline(16 * chunk, new_entries);
        // Every reference the new layout names is proven (the proof check is
        // not under test here).
        var proofs: std.ArrayList(DmaProof) = .empty;
        defer proofs.deinit(gpa);
        for (new_list.items) |e| try proofs.append(gpa, .{ .vol_tag = e.mapping.vol_tag, .offset = e.mapping.offset, .len = chunk, .completed_ns = 1 });
        var refs: TestRefs = .{};
        defer refs.deinit();
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        try publishLayout(&t, .{ .ino = 5, .name_key = &seed, .prev = prev, .layout = new, .proofs = proofs.items }, refs.seam(), tgeo);
        // The naive diff.
        var want_adds: std.ArrayList(RefOp) = .empty;
        defer want_adds.deinit(gpa);
        var want_rels: std.ArrayList(RefOp) = .empty;
        defer want_rels.deinit(gpa);
        var it = new.named(chunk);
        while (it.next()) |e| if (!Naive.names(prev, e, chunk)) try want_adds.append(gpa, refOf(5, e, chunk));
        if (prev) |*pl| {
            var ot = pl.named(chunk);
            while (ot.next()) |e| if (!Naive.names(new, e, chunk)) try want_rels.append(gpa, refOf(5, e, chunk));
        }
        try testing.expectEqualSlices(RefOp, want_adds.items, refs.adds.items);
        try testing.expectEqualSlices(RefOp, want_rels.items, refs.releases.items);
        // One record staged, in place (the arena-built value is the staged value).
        try testing.expectEqual(@as(usize, 1), t.staged.items.len);
    }
    // A same-block, same-reference pair is skipped off the wire: neither
    // side decodes; a rebound block yields its release, then its add.
    const a = try sortedEntries(gpa, &.{ .{ .block = 3, .mapping = Mapping.wholeBlock(1, 2 * chunk, 1) }, .{ .block = 4, .mapping = Mapping.wholeBlock(1, 9 * chunk, 1) } });
    defer gpa.free(a);
    const bb = try sortedEntries(gpa, &.{ .{ .block = 3, .mapping = Mapping.wholeBlock(1, 2 * chunk, 7) }, .{ .block = 4, .mapping = Mapping.wholeBlock(2, 9 * chunk, 1) } });
    defer gpa.free(bb);
    const la = Layout.stripedInline(chunk, a);
    const lb = Layout.stripedInline(chunk, bb);
    var walk = RefDiff.init(&lb, &la, chunk);
    const s1 = walk.next().?;
    try testing.expectEqual(@as(u32, 4), s1.released.block);
    try testing.expectEqual(@as(u64, 1), s1.released.mapping.vol_tag);
    const s2 = walk.next().?;
    try testing.expectEqual(@as(u32, 4), s2.added.block);
    try testing.expectEqual(@as(u64, 2), s2.added.mapping.vol_tag);
    try testing.expect(walk.next() == null);
    // The record is staged IN PLACE: a 1,000-entry map (48 KiB) grows the
    // tx's arena by less than two copies of itself (the duplicate into its
    // own arena was the publish's second 37 KiB allocation per block).
    var big_list: std.ArrayList(MapEntry) = .empty;
    defer big_list.deinit(gpa);
    var pf: std.ArrayList(DmaProof) = .empty;
    defer pf.deinit(gpa);
    var blk: u32 = 0;
    while (blk < 1000) : (blk += 1) {
        try big_list.append(gpa, .{ .block = blk, .mapping = Mapping.wholeBlock(1, blk * chunk, 1) });
        try pf.append(gpa, .{ .vol_tag = 1, .offset = blk * chunk, .len = chunk, .completed_ns = 1 });
    }
    const big_entries = try sortedEntries(gpa, big_list.items);
    defer gpa.free(big_entries);
    var refs: TestRefs = .{};
    defer refs.deinit();
    var t = tx.KvTx.init(gpa, @src());
    defer t.deinit();
    try publishLayout(&t, .{ .ino = 6, .name_key = &seed, .prev = null, .layout = Layout.stripedInline(1000 * chunk, big_entries), .proofs = pf.items }, refs.seam(), tgeo);
    const staged_len = t.staged.items[0].value.len;
    try testing.expect(staged_len > 48 * 1000);
    try testing.expect(t.arena.queryCapacity() < 2 * staged_len);
}

test "the flip to the block-map tree releases NOTHING (the diff arm would release every moved entry — the port hazard); a kvmap head never regresses to an inline map; a kvmap → kvmap head Put stages no ref op; the flip arm refuses any other shape" {
    const gpa = testing.allocator;
    const one_key: key.NameKey = .init(1);
    var refs: TestRefs = .{};
    defer refs.deinit();
    const chunk = tgeo.chunk;
    const entries = try sortedEntries(gpa, &.{
        .{ .block = 0, .mapping = Mapping.wholeBlock(0x11, 7 * chunk, 1) },
        .{ .block = 1, .mapping = Mapping.wholeBlock(0x22, 2 * chunk, 1) },
        .{ .block = 2, .mapping = Mapping.wholeBlock(0x11, 8 * chunk, 2) },
    });
    defer gpa.free(entries);
    const inline_map = Layout.stripedInline(3 * chunk, entries);
    const head = Layout.stripedKvmap(3 * chunk, null, 1);
    // The diff arm WOULD release all three (the hazard, pinned so the law stays visible).
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        try publishLayout(&t, .{ .ino = 9, .name_key = &one_key, .prev = inline_map, .layout = head, .proofs = &.{} }, refs.seam(), tgeo);
        try testing.expectEqual(@as(usize, 3), refs.releases.items.len);
        refs.releases.clearRetainingCapacity();
    }
    // The flip arm stages the head Put alone.
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        try publishLayout(&t, .{ .ino = 9, .name_key = &one_key, .prev = inline_map, .layout = head, .proofs = &.{}, .arm = .{ .flip_to_kvmap = .{ .moved = 3 } } }, refs.seam(), tgeo);
        try testing.expectEqual(@as(usize, 0), refs.releases.items.len);
        try testing.expectEqual(@as(usize, 0), refs.adds.items.len);
        try testing.expectEqual(@as(usize, 1), t.staged.items.len);
        try testing.expect((try XattrLayout.decode(t.staged.items[0].value, tgeo)).isKvmap());
    }
    // Sticky: kvmap → inline refuses; kvmap → kvmap (size, cursor) is a head Put with no ref op.
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        try testing.expectError(error.InvalidTransition, publishLayout(&t, .{ .ino = 9, .name_key = &one_key, .prev = head, .layout = inline_map, .proofs = &.{} }, refs.seam(), tgeo));
        try publishLayout(&t, .{ .ino = 9, .name_key = &one_key, .prev = head, .layout = Layout.stripedKvmap(1 * chunk, 1, 1), .proofs = &.{} }, refs.seam(), tgeo);
        try testing.expectEqual(@as(usize, 0), refs.releases.items.len + refs.adds.items.len);
        try testing.expectEqual(@as(usize, 1), t.staged.items.len);
    }
    // The flip arm on any other shape refuses.
    {
        var t = tx.KvTx.init(gpa, @src());
        defer t.deinit();
        try testing.expectError(error.InvalidTransition, publishLayout(&t, .{ .ino = 9, .name_key = &one_key, .prev = null, .layout = head, .proofs = &.{}, .arm = .{ .flip_to_kvmap = .{ .moved = 0 } } }, refs.seam(), tgeo));
        try testing.expectError(error.InvalidTransition, publishLayout(&t, .{ .ino = 9, .name_key = &one_key, .prev = head, .layout = head, .proofs = &.{}, .arm = .{ .flip_to_kvmap = .{ .moved = 0 } } }, refs.seam(), tgeo));
        try testing.expectError(error.InvalidTransition, publishLayout(&t, .{ .ino = 9, .name_key = &one_key, .prev = inline_map, .layout = Layout.stripedInline(3 * chunk, entries), .proofs = &.{}, .arm = .{ .flip_to_kvmap = .{ .moved = 3 } } }, refs.seam(), tgeo));
    }
    try testing.expect(validMapTransition(null, head));
    try testing.expect(validMapTransition(inline_map, head));
    try testing.expect(!validMapTransition(head, inline_map));
    try testing.expect(validMapTransition(head, Layout.stripedKvmap(0, 0, 1)));
}

test "the meta stripe is 4096-way, spreads consecutive inos, and a guard releases its PARKING lock (a second acquirer is refused while held, admitted after)" {
    try testing.expectEqual(@as(usize, 4096), INODE_META_STRIPES);
    const io = testing.io;
    const stripes = try testing.allocator.create(MetaStripes);
    defer testing.allocator.destroy(stripes);
    stripes.* = .{};
    var seen: [4096]bool = @splat(false);
    var distinct: usize = 0;
    for (0..256) |i| {
        const s = MetaStripes.stripeOf(@intCast(i + 2));
        if (!seen[s]) distinct += 1;
        seen[s] = true;
    }
    // Fibonacci hashing: 256 consecutive inos hit ≥ 240 distinct stripes.
    try testing.expect(distinct >= 240);
    const g = stripes.acquire(io, 7);
    try testing.expect(!stripes.locks[MetaStripes.stripeOf(7)].tryLock(io));
    g.release(io);
    try testing.expect(stripes.locks[MetaStripes.stripeOf(7)].tryLock(io));
    stripes.locks[MetaStripes.stripeOf(7)].unlock(io);
}

/// Two inos that collide on BOTH publish tables (found by a scan from
/// ino 8 upward against ino 7; the tie test restates both hashes): the
/// shape under which an inverted promotion order deadlocks against an
/// upload.
pub const colliding_inos = struct {
    pub const promoted: u64 = 7;
    pub const uploaded: u64 = 57_110_667;
    pub const layout_stripe: usize = 3860;
    pub const meta_stripe: usize = 1336;
};

test "the layout lock is 4096-way on the op guard's mix; the recorded pair collides on BOTH tables (the tie test of the two-thread test's shape)" {
    try testing.expectEqual(@as(usize, 4096), LAYOUT_LOCK_STRIPES);
    try testing.expectEqual(colliding_inos.layout_stripe, LayoutLocks.stripeOf(colliding_inos.promoted));
    try testing.expectEqual(colliding_inos.layout_stripe, LayoutLocks.stripeOf(colliding_inos.uploaded));
    try testing.expectEqual(colliding_inos.meta_stripe, MetaStripes.stripeOf(colliding_inos.promoted));
    try testing.expectEqual(colliding_inos.meta_stripe, MetaStripes.stripeOf(colliding_inos.uploaded));
    // Consecutive inos spread on the layout table too.
    var seen: [4096]bool = @splat(false);
    var distinct: usize = 0;
    for (0..256) |i| {
        const s = LayoutLocks.stripeOf(@intCast(i + 2));
        if (!seen[s]) distinct += 1;
        seen[s] = true;
    }
    try testing.expect(distinct >= 240);
}

/// The two-thread lock-order test's wall: well past any scheduler hiccup,
/// far short of the gate's patience. Monotonic time, never a sleep.
const LOCK_ORDER_WALL_NS: u64 = 20 * std.time.ns_per_s;

test "a staged promotion ((3.5) then (3.7)) completes against an upload holding (3.5) and merging under (3.7) on inos colliding on BOTH tables; the park seam holds the promotion between its two acquisitions until the upload is parked; the rail stays silent" {
    if (!lock_order.enabled) return testkit.skip(@src(), .debug_only, "the lock-order rail is compiled out of this build mode");
    const io = testing.io;
    const locks = try testing.allocator.create(PublishLocks);
    defer testing.allocator.destroy(locks);
    locks.* = .{};
    const X = colliding_inos.promoted;
    const Y = colliding_inos.uploaded;
    const Park = struct {
        locks: *PublishLocks,
        parked: std.atomic.Value(u32) = .init(0),
        /// The promotion holds its first lock until the upload is parked on
        /// EITHER table (whichever the order under test makes it wait on),
        /// so the inverted order's wedge is reached deterministically.
        fn call(ctx: *anyopaque) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            _ = self.parked.fetchAdd(1, .acq_rel);
            while (self.locks.stripes.waiters(testing.io, Y) + self.locks.layouts.waiters(testing.io, Y) == 0) std.Thread.yield() catch {};
        }
    };
    var park: Park = .{ .locks = locks };
    locks.promotion_park = .{ .ctx = &park, .call = &Park.call };
    const Promotion = struct {
        fn run(io_: Io, l: *PublishLocks, done: *std.atomic.Value(bool)) void {
            const g = l.lockPromotion(io_, X);
            g.release(io_);
            done.store(true, .release);
        }
    };
    // `accumulate.uploadParkedBlock`'s shape: (3.5) around `MapSeam.merge`,
    // which takes (3.7) first thing (`plane.mapMerge`).
    const Upload = struct {
        fn run(io_: Io, l: *PublishLocks, done: *std.atomic.Value(bool)) void {
            const sg = l.stripes.acquire(io_, Y);
            const lg = l.layouts.acquire(io_, Y);
            lg.release(io_);
            sg.release(io_);
            done.store(true, .release);
        }
    };
    const inv0 = lock_order.lock_order_inversions.load();
    var promo_done: std.atomic.Value(bool) = .init(false);
    var upload_done: std.atomic.Value(bool) = .init(false);
    const ta = try std.Thread.spawn(.{}, Promotion.run, .{ io, locks, &promo_done });
    // The upload starts only once the promotion holds its first lock.
    while (park.parked.load(.acquire) == 0) try std.Thread.yield();
    const tb = try std.Thread.spawn(.{}, Upload.run, .{ io, locks, &upload_done });
    const t0 = clock.monotonicNs();
    while (!(promo_done.load(.acquire) and upload_done.load(.acquire))) {
        if (clock.monotonicNs() -| t0 > LOCK_ORDER_WALL_NS) {
            // The ABBA wedge: the promotion parked on the stripe the upload
            // holds, the upload parked on the layout lock the promotion holds.
            std.debug.print("\nlock-order wedge: promotion parked on meta stripe {d}: {d}; upload parked on layout stripe {d}: {d}; park seam fired {d}\n", .{ colliding_inos.meta_stripe, locks.stripes.waiters(io, X), colliding_inos.layout_stripe, locks.layouts.waiters(io, Y), park.parked.load(.acquire) });
            ta.detach();
            tb.detach();
            return error.LockOrderWedge;
        }
        try std.Thread.yield();
    }
    ta.join();
    tb.join();
    try testing.expectEqual(@as(u32, 1), park.parked.load(.acquire));
    try testing.expectEqual(inv0, lock_order.lock_order_inversions.load());
    try testing.expectEqual(@as(u32, 0), locks.stripes.waiters(io, X));
    try testing.expectEqual(@as(u32, 0), locks.layouts.waiters(io, X));
}

test "stats JSON names every instrument" {
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const out = w.buffered();
    try testing.expect(std.mem.startsWith(u8, out, "{\"layout_inline_writes\":"));
    try testing.expect(std.mem.indexOf(u8, out, "\"inline_max_bytes\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"packed_mapping_refusals\":") != null);
    try testing.expect(std.mem.endsWith(u8, out, "}"));
}

fn decodersTotal(bytes: []const u8) void {
    if (Layout.decode(bytes, tgeo)) |l| {
        // Whatever decodes re-encodes to the same bytes.
        var buf: [LAYOUT_HEADER_LEN + 4 + 65_536]u8 = undefined;
        const enc = l.encodeInto(buf[0..l.encodedLen()]);
        std.debug.assert(std.mem.eql(u8, enc, bytes));
        // The head decode (the read view's fill) agrees with the full one
        // on everything but the per-entry checks it leaves to each read.
        const h = decodeHead(bytes, tgeo) catch unreachable;
        std.debug.assert(h.kind == l.kind and h.flags == l.flags and h.size == l.size and h.entries.len == l.entries.len and h.data.len == l.data.len);
        for (h.entries) |*e| _ = checkEntry(e, tgeo.chunk) catch unreachable;
    } else |_| {}
    _ = decodeHead(bytes, tgeo) catch {};
    if (LayoutDelta.decode(bytes, tgeo)) |d| {
        var buf: [2 + 2 + 1 + 8 * 3 + 16 + 40 + 4 + 8 + 4 + 65_536]u8 = undefined;
        const enc = d.encodeInto(buf[0..d.encodedLen()]);
        std.debug.assert(std.mem.eql(u8, enc, bytes));
    } else |_| {}
    if (bytes.len >= MAPPING_WIRE_LEN) _ = Mapping.decode(std.mem.bytesToValue(MappingWire, bytes[0..MAPPING_WIRE_LEN]), tgeo.chunk) catch {};
    _ = XattrLayout.decode(bytes, tgeo) catch {};
}

fn decodersTotalFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [LAYOUT_HEADER_LEN + 4 + 4096 + 64]u8 = undefined;
    const len = smith.slice(&buf);
    decodersTotal(buf[0..len]);
}

test "fuzz: the layout, delta, mapping and xattr-layout decoders and decodeHead are total; whatever decodes re-encodes byte-identically and its head decode agrees" {
    try fuzzkit.fuzz({}, decodersTotalFuzz, .{
        .corpus = &.{
            // A packed header claiming body_len 56 with a garbage mapping.
            &([_]u8{ 2, 1, 1, 0, 56, 0, 0, 0 } ++ @as([24]u8, @splat(0)) ++ @as([56]u8, @splat(0xFF))),
            // A striped inline header claiming u32::MAX entries.
            &([_]u8{ 3, 1, 4, 0, 4, 0, 0, 0 } ++ @as([24]u8, @splat(0)) ++ [_]u8{ 0xFF, 0xFF, 0xFF, 0xFF }),
            // A delta with every bit set and nothing behind it.
            &[_]u8{ 0xFF, 0x03 },
        },
    });
}

test "sweep: every length 0..(32 + 4 + 4096 + margin) of a well-formed inline record and of a striped map decodes exactly once" {
    const gpa = testing.allocator;
    const data: [4096]u8 = @splat(7);
    const full = try Layout.inlineData(4096, &data).encodeAlloc(gpa);
    defer gpa.free(full);
    const padded = try gpa.alloc(u8, full.len + 64);
    defer gpa.free(padded);
    @memcpy(padded[0..full.len], full);
    @memset(padded[full.len..], 0);
    var ok: usize = 0;
    for (0..padded.len + 1) |n| {
        if (Layout.decode(padded[0..n], tgeo)) |_| ok += 1 else |_| {}
    }
    try testing.expectEqual(@as(usize, 1), ok);
    const entries = try sortedEntries(gpa, &.{ .{ .block = 0, .mapping = Mapping.wholeBlock(1, 0, 0) }, .{ .block = 9, .mapping = Mapping.tenant(1, tgeo.chunk, 0, 0, 1) } });
    defer gpa.free(entries);
    const st = try Layout.stripedInline(1, entries).encodeAlloc(gpa);
    defer gpa.free(st);
    ok = 0;
    for (0..st.len + 1) |n| {
        if (Layout.decode(st[0..n], tgeo)) |_| ok += 1 else |_| {}
    }
    try testing.expectEqual(@as(usize, 1), ok);
}
