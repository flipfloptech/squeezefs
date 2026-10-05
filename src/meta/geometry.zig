//! Volume geometry and the format-time image plan (charter §5.1;
//! `docs/porting/kv_alloc_superblock.md` §2, §9, §10, §12): `plan` lays the
//! fixed structures exactly as the reference, the feature word per format
//! class, the one-member membership stamp, the set-wide `hash_seed`, the
//! uuid-namespaced node-seq base, the byte-exact format-time appender-0
//! `Free` page and `KVAD` directory header, and the 13-step build order as a
//! declarative plan whose node-image and ledger-record steps are typed
//! placeholders the format task (K6) fills from the K2/K3 codecs. Pure: no
//! device I/O, no randomness (production randomness lives only in `uuid`
//! and `set_uuid`, both inputs here).

const std = @import("std");
const checksum = @import("../core/checksum.zig");
const alloc_core = @import("alloc_core.zig");
const alloc_ext = @import("alloc_ext.zig");
const superblock = @import("superblock.zig");

pub const ExtentRef = superblock.ExtentRef;
pub const SuperblockV3 = superblock.SuperblockV3;

// ---------------------------------------------------------------------------
// The journal ring clamp and the override floor.
// ---------------------------------------------------------------------------

/// Resolved OQ 1: a flat 32 MiB would be ~12 % of a 256 MiB volume; the
/// 8 MiB floor still holds the 256 KiB reserve + 128 KiB max entry many times.
pub const journal_ring_min: u64 = 8 << 20;
pub const journal_ring_max: u64 = 32 << 20;
/// 2× the largest legal record (a 64 KiB value) plus headers.
pub const max_entry_len: u64 = 131_072;
/// The checkpoint-task reserve carve-out `max(256 KiB, ring/64)` plus one
/// max entry must always be admissible, or a legal tx could never commit.
pub const journal_override_floor: u64 = 256 * 1024 + max_entry_len;
pub const journal_page_len: u64 = superblock.journal_page_len;

/// `clamp(volume_len / 64, 8 MiB, 32 MiB)` rounded down to whole pages (the
/// bounds already are, so the alignment never dips below the floor).
pub fn journalRingLen(volume_len: u64) u64 {
    const clamped = @min(@max(volume_len / 64, journal_ring_min), journal_ring_max);
    return clamped / journal_page_len * journal_page_len;
}

// ---------------------------------------------------------------------------
// plan()
// ---------------------------------------------------------------------------

pub const PlanError = error{ InvalidNodeSize, InvalidJournalOverride, VolumeTooSmall };

pub const PlanRefusal = union(enum) {
    invalid_node_size: u64,
    invalid_journal_override: u64,
    volume_too_small: struct { volume_len: u64, total: u64, node_size: u64, journal_len: u64, heap_start: u64, min: u64 },

    pub fn write(self: PlanRefusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .invalid_node_size => |n| try w.print("invalid node_size {d}: must be a 4 KiB multiple in [65536, 1048576]", .{n}),
            .invalid_journal_override => |len| try w.print("journal ring override {d} bytes is invalid: must be a 4 KiB multiple of at least {d} bytes (reserve + one max entry)", .{ len, journal_override_floor }),
            .volume_too_small => |v| try w.print("metadata volume too small for format v3: {d} bytes leaves {d} heap extents of {d} bytes after the fixed structures (superblock + ledger + {d}-byte journal + bitmap end at {d}); at least {d} extents are required — grow the volume or pass a smaller --meta-journal-mb / --meta-node-kib", .{ v.volume_len, v.total, v.node_size, v.journal_len, v.heap_start, v.min }),
        }
    }
};

/// `SuperblockV3::plan`: ledger at 4096 (32 × 4 KiB), journal per the clamp
/// or the override, bitmap over-reserved for `volume_len / node_size`
/// extents (deliberate: it breaks the bitmap↔heap circularity), heap start
/// aligned to `node_size`, the LAST sector held back for the DUR-5 copy
/// (`usable = volume_len − 4096`), and the floor `reserve + 4` (three roots
/// and growth).
pub fn plan(volume_len: u64, node_size: u64, journal_len_override: ?u64, uuid: [16]u8, hash_seed: u64, diag: ?*PlanRefusal) PlanError!SuperblockV3 {
    if (!superblock.nodeSizeValid(node_size)) {
        if (diag) |d| d.* = .{ .invalid_node_size = node_size };
        return error.InvalidNodeSize;
    }
    const root_ledger: ExtentRef = .{ .start = superblock.sector_len, .len = superblock.root_ledger_len };
    const journal_len = if (journal_len_override) |len| blk: {
        if (len % journal_page_len != 0 or len < journal_override_floor) {
            if (diag) |d| d.* = .{ .invalid_journal_override = len };
            return error.InvalidJournalOverride;
        }
        break :blk len;
    } else journalRingLen(volume_len);
    const journal: ExtentRef = .{ .start = root_ledger.end(), .len = journal_len };
    const upper_extents = volume_len / node_size;
    const alloc_bitmap: ExtentRef = .{ .start = journal.end(), .len = alloc_ext.bitmapRegionLen(upper_extents) };
    const heap_start = ((alloc_bitmap.end() + node_size - 1) / node_size) * node_size;
    const usable = volume_len -| superblock.sector_len;
    const total = (usable -| heap_start) / node_size;
    const min_extents = alloc_core.compactionReserveExtents(total) + 4;
    if (total < min_extents) {
        if (diag) |d| d.* = .{ .volume_too_small = .{ .volume_len = volume_len, .total = total, .node_size = node_size, .journal_len = journal_len, .heap_start = heap_start, .min = min_extents } };
        return error.VolumeTooSmall;
    }
    return .{
        .node_size = @intCast(node_size),
        .features_incompat = superblock.plan_features_incompat,
        .features_ro = 0,
        .root_ledger = root_ledger,
        .journal = journal,
        .alloc_bitmap = alloc_bitmap,
        .heap = .{ .start = heap_start, .len = total * node_size },
        .uuid = uuid,
        .hash_seed = hash_seed,
        .appender_dir = .none,
    };
}

// ---------------------------------------------------------------------------
// Format classes and their feature words.
// ---------------------------------------------------------------------------

pub const FormatClass = enum {
    /// The default `format`: nine mw bits + bit 17 + the appender region.
    symmetric,
    /// The seam-blind explicit form (never turned flat by FORMAT_FLAT).
    symmetric_explicit,
    /// `format --single-writer`: the stamp still rides the ledger; only the
    /// mw bits, bit 17 and the appender region are withheld.
    single_writer,
    /// Nine bits, no forest — the pre-flip class (tests only); a writable
    /// mount refuses it since PR 14.
    multi_writer_flat,
};

/// Test seams the builder honours (read by the format task through the knob
/// readers; here they are plain inputs).
pub const Seams = struct {
    /// `SQUEEZEFS_TEST_STAMP_BLOCK_REFS=1`: bit 9 alone.
    stamp_block_refs: bool = false,
    /// `SQUEEZEFS_TEST_STAMP_WRITER_SCOPE=1`: bit 10 alone.
    stamp_writer_scope: bool = false,
    /// `SQUEEZEFS_TEST_FORMAT_FLAT=1`: a DEFAULT-class request builds the
    /// single-writer volume (never the explicit forms).
    format_flat: bool = false,
};

pub fn effectiveClass(class: FormatClass, seams: Seams) FormatClass {
    return if (class == .symmetric and seams.format_flat) .single_writer else class;
}

/// `plan`'s word (bits 0,1,6,7) + bits 2|4 (every public format is a
/// stamped set member) + the class bits + the seams.
pub fn featureWord(class: FormatClass, seams: Seams) u64 {
    var word = superblock.plan_features_incompat | superblock.kv_guest_slots | superblock.kv_slot_migration;
    switch (effectiveClass(class, seams)) {
        .symmetric, .symmetric_explicit => word |= superblock.multi_writer_format_bits | superblock.kv_symmetric_forest,
        .multi_writer_flat => word |= superblock.multi_writer_format_bits,
        .single_writer => {},
    }
    if (seams.stamp_block_refs) word |= superblock.kv_block_refcounts;
    if (seams.stamp_writer_scope) word |= superblock.kv_writer_scoped_staging;
    return word;
}

/// bits {0,1,2,4,6,7,8..15,17}. (The spec's `0x3FFD7` transcribes bit 16
/// in; the builder never stamps bit 16 at format — `builder.rs:496-560`.)
pub const default_format_word: u64 = 0x2FFD7;
/// bits {0,1,2,4,6,7}.
pub const single_writer_word: u64 = 0xD7;
/// The default word without bit 17.
pub const multi_writer_flat_word: u64 = 0xFFD7;

// ---------------------------------------------------------------------------
// --meta-node-kib
// ---------------------------------------------------------------------------

/// `min(65536, node_size/4) + 256`: Linux `XATTR_SIZE_MAX` at the default
/// node; `/4` keeps ≥ 4 records per leaf; +256 = 1-byte name_len + 255-byte
/// name envelope.
pub fn recordValueCap(node_size: u64) u64 {
    return @min(65_536, node_size / 4) + 256;
}

pub const allowed_node_kib = [_]u32{ 64, 128, 256, 512, 1024 };

pub const NodeKib = struct { bytes: u64, warning: ?[]const u8 };

/// The CLI accepts ONLY the five powers of two; below 256 KiB returns a
/// warning naming the reduced per-volume record-value cap.
pub fn validateNodeKib(kib: u32, buf: []u8) error{UnsupportedNodeKib}!NodeKib {
    if (std.mem.indexOfScalar(u32, &allowed_node_kib, kib) == null) {
        return error.UnsupportedNodeKib;
    }
    const bytes = @as(u64, kib) * 1024;
    if (kib >= 256) return .{ .bytes = bytes, .warning = null };
    const warning = std.fmt.bufPrint(buf, "--meta-node-kib {d} trades xattr/layout headroom for cold-read latency: the per-volume record-value cap becomes {d} bytes (node_size/4, design §4.2) — values above it are rejected and layout maps spill to the indirect mechanism sooner", .{ kib, recordValueCap(bytes) }) catch buf[0..0];
    return .{ .bytes = bytes, .warning = warning };
}

pub fn writeNodeKibRefusal(w: *std.Io.Writer, kib: u32) std.Io.Writer.Error!void {
    try w.print("--meta-node-kib {d} is not supported: allowed values are 64|128|256|512|1024 (64 KiB floor, 1 MiB ceiling — design §5.1)", .{kib});
}

// ---------------------------------------------------------------------------
// The membership stamp, hash_seed and node_seq_base.
// ---------------------------------------------------------------------------

/// `W = 2^16`: the u16 slot-id wire type, derived, frozen forever.
pub const routing_width: u32 = 65_536;

pub const SlotRun = struct { start: u16, stride: u16, count: u32 };

/// `plan_meta_slot_set(n).stamps[pos]`: member `pos` of `n` hosts the stride
/// run `{start: pos, stride: n (1 if n == 1), count: ceil((W − pos)/n)}`,
/// `native_slot = pos`, no cursors, epoch 1.
pub const MembershipStamp = struct {
    set_uuid: [16]u8,
    set_epoch: u64,
    member_position: u16,
    member_count: u16,
    routing_width: u32,
    slots_hosted: SlotRun,
    native_slot: ?u16,

    pub fn member(set_uuid: [16]u8, position: u16, count: u16) MembershipStamp {
        std.debug.assert(count >= 1 and position < count);
        const stride: u16 = if (count == 1) 1 else count;
        return .{
            .set_uuid = set_uuid,
            .set_epoch = 1,
            .member_position = position,
            .member_count = count,
            .routing_width = routing_width,
            .slots_hosted = .{ .start = position, .stride = stride, .count = @intCast((routing_width - position + count - 1) / count) },
            .native_slot = position,
        };
    }

    /// The default `format`'s stamp: one member hosting every slot.
    pub fn oneMember(set_uuid: [16]u8) MembershipStamp {
        return member(set_uuid, 0, 1);
    }
};

/// Set-wide: a migrated slot's seeded dentry hashes must resolve on its new
/// volume; the same secrecy class as a per-volume seed (plaintext in sector 0).
pub fn hashSeed(set_uuid: [16]u8) u64 {
    return checksum.xxh3(&set_uuid);
}

/// `u64 LE of uuid[0..8] & (u64::MAX >> 1)`: quick format leaves heap frames
/// behind, and uuid-namespaced seqs make a cross-generation chain match as
/// improbable as a checksum collision; the clear top bit is 2^63 of headroom.
pub fn nodeSeqBase(uuid: [16]u8) u64 {
    return std.mem.readInt(u64, uuid[0..8], .little) & (std.math.maxInt(u64) >> 1);
}

// ---------------------------------------------------------------------------
// The forest's format-time appender-0 page and directory header.
// ---------------------------------------------------------------------------

pub const appender_page_len: usize = 4096;
/// "KVAP".
pub const appender_page_magic: u32 = 0x4B56_4150;
/// "KVAD".
pub const appender_dir_magic: u32 = 0x4B56_4144;
/// A verified page with another value is ForeignLayout ⇒ REFUSE, never fallback.
pub const appender_page_layout_version: u8 = 1;
/// `[A, B, R0, R1]`: appender 0's page slots are the fixed ring's first four
/// pages (16 KiB); its ring proper begins after them.
pub const appender_page_slots: usize = 4;
pub const appender0_reserved_pages: u64 = appender_page_slots;
/// `(4096 − 314) / 35` slot entries per page.
pub const slot_page_budget: usize = (appender_page_len - 314) / 35;

pub fn appender0PageOffsets(journal: ExtentRef) [appender_page_slots]u64 {
    const p: u64 = appender_page_len;
    return .{ journal.start, journal.start + p, journal.start + 2 * p, journal.start + 3 * p };
}

pub fn appender0RingExtent(journal: ExtentRef) ExtentRef {
    const reserved = appender0_reserved_pages * appender_page_len;
    return .{ .start = journal.start + reserved, .len = journal.len -| reserved };
}

/// The slot a write at `generation` lands in.
pub fn pageSlotFor(generation: u64) usize {
    return @intCast(generation % appender_page_slots);
}

/// The format-time generation of appender 0's page.
pub const appender0_format_generation: u64 = 1;

/// Where format writes the page: `page_slot_for(1) = 1` ⇒ slot B.
pub fn appender0FormatPageOffset(journal: ExtentRef) u64 {
    return appender0PageOffsets(journal)[pageSlotFor(appender0_format_generation)];
}

/// `AppenderPage::free(0, 1)` with one segment = the ring proper, byte-exact
/// per spec §9. Identity zeros, state Free, layout version 1, everything
/// else zero.
pub fn appender0FreePage(journal: ExtentRef, out: *[appender_page_len]u8) void {
    @memset(out, 0);
    std.mem.writeInt(u32, out[0..4], appender_page_magic, .little);
    std.mem.writeInt(u32, out[4..8], 0, .little);
    std.mem.writeInt(u64, out[8..16], appender0_format_generation, .little);
    out[55] = appender_page_layout_version;
    std.mem.writeInt(u16, out[72..74], 1, .little);
    const ring = appender0RingExtent(journal);
    std.mem.writeInt(u64, out[80..88], ring.start, .little);
    std.mem.writeInt(u64, out[88..96], ring.len, .little);
    checksum.stamp(out, 16);
}

/// Page pairs one directory extent holds: every page but the header, in
/// pairs (256 KiB ⇒ 31).
pub fn dirPairsPerExtent(node_size: u64) u64 {
    return (node_size / appender_page_len -| 1) / 2;
}

/// The header page is the extent's LAST page.
pub fn dirHeaderOffset(extent: ExtentRef) u64 {
    return extent.end() - appender_page_len;
}

/// `DirHeader{chain_index: 0, next: none, pairs}`: `magic u32 ‖ chain_index
/// u32 ‖ xxh3 ‖ next.start u64 ‖ next.len u64 ‖ pairs u16`, zero-padded.
pub fn dirHeaderPage(node_size: u64, out: *[appender_page_len]u8) void {
    @memset(out, 0);
    std.mem.writeInt(u32, out[0..4], appender_dir_magic, .little);
    std.mem.writeInt(u32, out[4..8], 0, .little);
    std.mem.writeInt(u64, out[16..24], 0, .little);
    std.mem.writeInt(u64, out[24..32], 0, .little);
    std.mem.writeInt(u16, out[32..34], @intCast(dirPairsPerExtent(node_size)), .little);
    checksum.stamp(out, 8);
}

// ---------------------------------------------------------------------------
// The 13-step build order as a declarative plan.
// ---------------------------------------------------------------------------

/// What a tree's records are at format time.
pub const TreeRecords = enum {
    /// Tree 0 (`TREE_CONTROL = 8`): no records — still one real empty leaf.
    control_empty,
    /// The forest's native slot tree: every record re-keyed by
    /// `forest_key(kind, legacy_key)`, sorted.
    native,
    inodes,
    dentries,
    xattrs,
    /// Kind 6, empty, iff bit 9 is set.
    block_refs_empty,
};

pub const TreeSpec = struct { tree_id: u8, records: TreeRecords };

pub const forest_trees = [_]TreeSpec{ .{ .tree_id = 8, .records = .control_empty }, .{ .tree_id = 0, .records = .native } };
pub const flat_trees = [_]TreeSpec{ .{ .tree_id = 1, .records = .inodes }, .{ .tree_id = 2, .records = .dentries }, .{ .tree_id = 3, .records = .xattrs } };
pub const flat_block_refs_tree = TreeSpec{ .tree_id = 6, .records = .block_refs_empty };

/// The bootstrap ledger record is `seq 1` ⇒ slot `1 % 32 = 1`, offset
/// `root_ledger.start + 4096` — NOT slot 0 (the spec's correction).
pub const bootstrap_ledger_seq: u64 = 1;
pub const bitmap_format_generation: u64 = 1;

pub const BuildStep = union(enum) {
    /// 1. O_DIRECT from the first byte.
    register_device,
    /// 3. Quick-format hygiene: SB + ledger + ring + bitmap; the heap is NOT
    /// wiped (dead frames fail the uuid-namespaced seq chain). Also 8's
    /// directory zero.
    zero_range: struct { offset: u64, len: u64 },
    /// 7. One tree in the fixed order: the format task packs its records
    /// bottom-up (¾ fill), claiming one extent per node lowest-first
    /// (`claim_internal`) and minting seqs `base+1, base+2, …`. The node
    /// images are the K2 codec's.
    tree_image: TreeSpec,
    /// 8 (forest only). The directory extent is the NEXT lowest free extent
    /// after the nodes; `sb.appender_dir = {heap.start + extent × node_size,
    /// node_size}`.
    claim_dir_extent,
    /// 8. `dirHeaderPage` at `dir.end − 4096`.
    write_dir_header,
    /// 8. `appender0FreePage` at `journal.start + 4096` (slot B).
    write_appender_page: struct { offset: u64 },
    /// 9. Every page dirty ⇒ the whole bitmap at generation 1 into slot A
    /// (`base + i × 8192`), bits = every claimed extent (nodes + dir).
    write_bitmap_pages: struct { base: u64, generation: u64 },
    /// 10. `{seq 1, roots, journal_tail_seq 0, next_ino, alloc_bitmap_generation
    /// 1, node_seq_watermark = last minted seq, stamp, no partition suffix}`
    /// — the K3 slot codec — at the slot offset below.
    write_ledger_record: struct { offset: u64, seq: u64, journal_tail_seq: u64, alloc_bitmap_generation: u64 },
    /// 11, 13. `fdatasync`.
    barrier,
    /// 12. `superblock.planWrite(sb, len, nextSuperblockGeneration(primary,
    /// tail))`: tail copy, barrier, sector 0, barrier.
    write_superblock,
};

pub const BuildPlan = struct {
    steps: [16]BuildStep,
    len: u8,

    pub fn slice(self: *const BuildPlan) []const BuildStep {
        return self.steps[0..self.len];
    }

    fn push(self: *BuildPlan, step: BuildStep) void {
        self.steps[self.len] = step;
        self.len += 1;
    }
};

/// `ImageBuilder::build`'s write/barrier order for a planned superblock and
/// its (effective) class. The reference's in-memory steps (2 plan + bits,
/// 4 allocator format, 5 layout, 6 writer) have no entry: they are the
/// format task's setup, not device order. Atomicity: sector 0 is zeroed at
/// step 3 and rewritten LAST, so no crash prefix is mountable (it reads
/// Blank) and a re-run needs no `--force`.
pub fn buildPlan(sb: SuperblockV3, class: FormatClass, word: u64) BuildPlan {
    var p: BuildPlan = .{ .steps = undefined, .len = 0 };
    const forest = class == .symmetric or class == .symmetric_explicit;
    p.push(.register_device);
    p.push(.{ .zero_range = .{ .offset = 0, .len = sb.heap.start } });
    if (forest) {
        for (forest_trees) |t| p.push(.{ .tree_image = t });
    } else {
        for (flat_trees) |t| p.push(.{ .tree_image = t });
        if (word & superblock.kv_block_refcounts != 0) p.push(.{ .tree_image = flat_block_refs_tree });
    }
    if (forest) {
        p.push(.claim_dir_extent);
        p.push(.write_dir_header);
        p.push(.{ .write_appender_page = .{ .offset = appender0FormatPageOffset(sb.journal) } });
    }
    p.push(.{ .write_bitmap_pages = .{ .base = sb.alloc_bitmap.start, .generation = bitmap_format_generation } });
    p.push(.{ .write_ledger_record = .{
        .offset = sb.root_ledger.start + (bootstrap_ledger_seq % 32) * 4096,
        .seq = bootstrap_ledger_seq,
        .journal_tail_seq = 0,
        .alloc_bitmap_generation = bitmap_format_generation,
    } });
    p.push(.barrier);
    p.push(.write_superblock);
    p.push(.barrier);
    return p;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const MiB: u64 = 1 << 20;
const GiB: u64 = 1 << 30;
const TiB: u64 = 1 << 40;
const test_uuid: [16]u8 = "kv-backend-test!".*;
const test_seed: u64 = 0x5EED_CAFE_F00D_D00D;

fn planMsg(buf: []u8, r: PlanRefusal) []const u8 {
    var w: std.Io.Writer = .fixed(buf);
    r.write(&w) catch unreachable;
    return w.buffered();
}

test "journal ring clamp math: 256 MiB → 8 MiB, 1 GiB → 16 MiB, 4 GiB → 32 MiB, 100 TiB → 32 MiB; page-aligned; the override floor is 393216" {
    try testing.expectEqual(8 * MiB, journalRingLen(256 * MiB));
    try testing.expectEqual(16 * MiB, journalRingLen(GiB));
    try testing.expectEqual(32 * MiB, journalRingLen(4 * GiB));
    try testing.expectEqual(32 * MiB, journalRingLen(100 * TiB));
    try testing.expectEqual(8 * MiB, journalRingLen(64 * MiB));
    try testing.expectEqual(@as(u64, 0), journalRingLen(700 * MiB) % 4096);
    // Inside the clamp an unaligned `v / 64` rounds DOWN to whole pages.
    try testing.expectEqual(10 * MiB, journalRingLen(64 * (10 * MiB + 1000)));
    try testing.expectEqual(@as(u64, 393_216), journal_override_floor);
}

test "plan: the four worked examples tie (64 MiB/64 KiB/1 MiB override, 256 MiB, 1 GiB, 1 TiB)" {
    const a = try plan(64 * MiB, 65536, MiB, test_uuid, test_seed, null);
    try testing.expectEqual(ExtentRef{ .start = 4096, .len = 131072 }, a.root_ledger);
    try testing.expectEqual(ExtentRef{ .start = 135168, .len = 1048576 }, a.journal);
    try testing.expectEqual(ExtentRef{ .start = 1183744, .len = 8192 }, a.alloc_bitmap);
    try testing.expectEqual(ExtentRef{ .start = 1245184, .len = 1004 * 65536 }, a.heap);
    try testing.expectEqual(@as(u64, 1004), a.totalExtents());
    try testing.expectEqual(@as(u64, 20), alloc_core.compactionReserveExtents(a.totalExtents()));
    try testing.expectEqual(@as(?u64, 67104768), superblock.backupSlotFor(a, 64 * MiB));
    try testing.expectEqual(superblock.plan_features_incompat, a.features_incompat);
    try testing.expectEqual(@as(u64, 0), a.features_ro);
    try testing.expect(a.appender_dir.isNone());
    try testing.expectEqual(test_uuid, a.uuid);
    try testing.expectEqual(test_seed, a.hash_seed);
    try a.validateGeometry(null);

    const b = try plan(256 * MiB, 262144, null, test_uuid, test_seed, null);
    try testing.expectEqual(ExtentRef{ .start = 135168, .len = 8388608 }, b.journal);
    try testing.expectEqual(ExtentRef{ .start = 8523776, .len = 8192 }, b.alloc_bitmap);
    try testing.expectEqual(ExtentRef{ .start = 8650752, .len = 990 * 262144 }, b.heap);
    try testing.expectEqual(@as(u64, 19), alloc_core.compactionReserveExtents(b.totalExtents()));
    try testing.expectEqual(@as(?u64, 268431360), superblock.backupSlotFor(b, 256 * MiB));

    const c = try plan(GiB, 262144, null, test_uuid, test_seed, null);
    try testing.expectEqual(ExtentRef{ .start = 135168, .len = 16777216 }, c.journal);
    try testing.expectEqual(ExtentRef{ .start = 16912384, .len = 8192 }, c.alloc_bitmap);
    try testing.expectEqual(ExtentRef{ .start = 17039360, .len = 4030 * 262144 }, c.heap);
    try testing.expectEqual(@as(u64, 80), alloc_core.compactionReserveExtents(c.totalExtents()));

    const d = try plan(TiB, 262144, null, test_uuid, test_seed, null);
    try testing.expectEqual(ExtentRef{ .start = 135168, .len = 33554432 }, d.journal);
    try testing.expectEqual(ExtentRef{ .start = 33689600, .len = 1056768 }, d.alloc_bitmap);
    try testing.expectEqual(ExtentRef{ .start = 34865152, .len = 4194170 * 262144 }, d.heap);
    try testing.expectEqual(@as(u64, 83883), alloc_core.compactionReserveExtents(d.totalExtents()));
    try d.validateGeometry(null);
    // The heap start is aligned to node_size, stronger than the 4 KiB the
    // validator demands; the bitmap is over-reserved for volume/node_size.
    try testing.expectEqual(@as(u64, 0), d.heap.start % 262144);
    try testing.expect(d.alloc_bitmap.len >= alloc_ext.bitmapRegionLen(d.totalExtents()));
}

test "plan: default = the clamp (8 MiB on 64 MiB); an override is honoured; SB|ledger|ring|bitmap|heap ascend; a 4 MiB volume fails with the exact message" {
    const def = try plan(64 * MiB, 65536, null, test_uuid, test_seed, null);
    try testing.expectEqual(8 * MiB, def.journal.len);
    const ov = try plan(64 * MiB, 65536, 2 * MiB, test_uuid, test_seed, null);
    try testing.expectEqual(2 * MiB, ov.journal.len);
    try testing.expect(ov.root_ledger.start >= 4096);
    try testing.expect(ov.journal.start >= ov.root_ledger.end());
    try testing.expect(ov.alloc_bitmap.start >= ov.journal.end());
    try testing.expect(ov.heap.start >= ov.alloc_bitmap.end());
    var diag: PlanRefusal = undefined;
    var buf: [512]u8 = undefined;
    try testing.expectError(error.VolumeTooSmall, plan(4 * MiB, 65536, null, test_uuid, test_seed, &diag));
    // 4 MiB: journal 8 MiB already exceeds the volume ⇒ 0 extents, min 12.
    try testing.expectEqualStrings("metadata volume too small for format v3: 4194304 bytes leaves 0 heap extents of 65536 bytes after the fixed structures (superblock + ledger + 8388608-byte journal + bitmap end at 8585216); at least 12 extents are required — grow the volume or pass a smaller --meta-journal-mb / --meta-node-kib", planMsg(&buf, diag));
    // The override floor and alignment.
    _ = try plan(64 * MiB, 65536, journal_override_floor, test_uuid, test_seed, &diag);
    try testing.expectError(error.InvalidJournalOverride, plan(64 * MiB, 65536, journal_override_floor - 4096, test_uuid, test_seed, &diag));
    try testing.expectEqualStrings("journal ring override 389120 bytes is invalid: must be a 4 KiB multiple of at least 393216 bytes (reserve + one max entry)", planMsg(&buf, diag));
    try testing.expectError(error.InvalidJournalOverride, plan(64 * MiB, 65536, journal_override_floor + 1, test_uuid, test_seed, &diag));
    try testing.expectError(error.InvalidNodeSize, plan(64 * MiB, 4096, null, test_uuid, test_seed, &diag));
    try testing.expectEqualStrings("invalid node_size 4096: must be a 4 KiB multiple in [65536, 1048576]", planMsg(&buf, diag));
    // The DUR-5 tail: the last sector is never inside the heap.
    try testing.expect(def.heap.end() <= 64 * MiB - 4096);
    // Exactly the floor: reserve 8 + 4 = 12 extents on a 64 KiB node.
    const tiny_len: u64 = 1245184 + 12 * 65536 + 4096;
    const tiny = try plan(tiny_len, 65536, MiB, test_uuid, test_seed, null);
    try testing.expectEqual(@as(u64, 12), tiny.totalExtents());
    try testing.expectError(error.VolumeTooSmall, plan(tiny_len - 1, 65536, MiB, test_uuid, test_seed, null));
}

test "feature words per class: default 0x2FFD7, single-writer 0xD7, multi-writer-flat 0xFFD7; the seams add bit 9 / bit 10; FORMAT_FLAT turns only the default class flat" {
    try testing.expectEqual(default_format_word, featureWord(.symmetric, .{}));
    try testing.expectEqual(default_format_word, featureWord(.symmetric_explicit, .{}));
    try testing.expectEqual(single_writer_word, featureWord(.single_writer, .{}));
    try testing.expectEqual(multi_writer_flat_word, featureWord(.multi_writer_flat, .{}));
    try testing.expectEqual(single_writer_word | superblock.kv_block_refcounts, featureWord(.single_writer, .{ .stamp_block_refs = true }));
    try testing.expectEqual(single_writer_word | superblock.kv_writer_scoped_staging, featureWord(.single_writer, .{ .stamp_writer_scope = true }));
    try testing.expectEqual(single_writer_word, featureWord(.symmetric, .{ .format_flat = true }));
    try testing.expectEqual(default_format_word, featureWord(.symmetric_explicit, .{ .format_flat = true }));
    try testing.expectEqual(FormatClass.single_writer, effectiveClass(.symmetric, .{ .format_flat = true }));
    try testing.expectEqual(FormatClass.multi_writer_flat, effectiveClass(.multi_writer_flat, .{ .format_flat = true }));
    // Every class word decodes through the gate.
    for ([_]FormatClass{ .symmetric, .single_writer, .multi_writer_flat }) |class| {
        var sb = superblock.testFixture();
        sb.features_incompat = featureWord(class, .{});
        if (class != .symmetric) sb.appender_dir = .none;
        var img: [4096]u8 = undefined;
        try sb.encodeSector(0, &img, null);
        const back = try superblock.SuperblockV3.decodeSector(&img, null);
        try testing.expectEqual(class == .symmetric, back.symmetricForestStamped());
        try testing.expectEqual(class != .single_writer, back.multiWriterClass());
    }
    // Bit 16 is never stamped at format; bit 11 ⇒ all nine.
    try testing.expectEqual(@as(u64, 0), default_format_word & superblock.kv_block_map_tree);
    try testing.expectEqual(superblock.multi_writer_format_bits, default_format_word & superblock.multi_writer_format_bits);
}

test "--meta-node-kib: allowed {64,128,256,512,1024}; < 256 warns naming record_value_cap; 32/0/100/2048 refused" {
    var buf: [512]u8 = undefined;
    for (allowed_node_kib) |kib| {
        const r = try validateNodeKib(kib, &buf);
        try testing.expectEqual(@as(u64, kib) * 1024, r.bytes);
        try testing.expectEqual(kib < 256, r.warning != null);
    }
    const r64 = try validateNodeKib(64, &buf);
    try testing.expectEqualStrings("--meta-node-kib 64 trades xattr/layout headroom for cold-read latency: the per-volume record-value cap becomes 16640 bytes (node_size/4, design §4.2) — values above it are rejected and layout maps spill to the indirect mechanism sooner", r64.warning.?);
    try testing.expectEqual(@as(u64, 65_792), recordValueCap(262144));
    try testing.expectEqual(@as(u64, 65_792), recordValueCap(1048576));
    try testing.expectEqual(@as(u64, 33_024), recordValueCap(131072));
    for ([_]u32{ 32, 0, 100, 2048 }) |kib| try testing.expectError(error.UnsupportedNodeKib, validateNodeKib(kib, &buf));
    var w: std.Io.Writer = .fixed(&buf);
    try writeNodeKibRefusal(&w, 2048);
    try testing.expectEqualStrings("--meta-node-kib 2048 is not supported: allowed values are 64|128|256|512|1024 (64 KiB floor, 1 MiB ceiling — design §5.1)", w.buffered());
}

test "membership stamp: one member hosts {0, 1, 65536} natively at epoch 1; member pos of n hosts {pos, n, ceil((W−pos)/n)}" {
    const one = MembershipStamp.oneMember(test_uuid);
    try testing.expectEqual(test_uuid, one.set_uuid);
    try testing.expectEqual(@as(u64, 1), one.set_epoch);
    try testing.expectEqual(@as(u16, 0), one.member_position);
    try testing.expectEqual(@as(u16, 1), one.member_count);
    try testing.expectEqual(@as(u32, 65536), one.routing_width);
    try testing.expectEqual(SlotRun{ .start = 0, .stride = 1, .count = 65536 }, one.slots_hosted);
    try testing.expectEqual(@as(?u16, 0), one.native_slot);
    const m = MembershipStamp.member(test_uuid, 1, 4);
    try testing.expectEqual(SlotRun{ .start = 1, .stride = 4, .count = 16384 }, m.slots_hosted);
    try testing.expectEqual(@as(?u16, 1), m.native_slot);
    const last = MembershipStamp.member(test_uuid, 2, 3);
    try testing.expectEqual(@as(u32, 21845), last.slots_hosted.count);
}

test "hash_seed = xxh3(set_uuid); node_seq_base = uuid[0..8] LE with the top bit clear" {
    try testing.expectEqual(checksum.xxh3("kv-backend-test!"), hashSeed(test_uuid));
    try testing.expect(hashSeed(test_uuid) != hashSeed(@splat(0)));
    try testing.expectEqual(std.mem.readInt(u64, "kv-backe", .little), nodeSeqBase(test_uuid));
    const ones: [16]u8 = @splat(0xFF);
    try testing.expectEqual(std.math.maxInt(u64) >> 1, nodeSeqBase(ones));
    try testing.expectEqual(@as(u64, 0), nodeSeqBase(@splat(0)));
}

test "appender-0 Free page: byte-exact per §9 at journal.start + 4096 (slot B); the other three slots stay zero; ring proper at +16384" {
    const journal = ExtentRef{ .start = 135168, .len = 1048576 };
    try testing.expectEqual([4]u64{ 135168, 139264, 143360, 147456 }, appender0PageOffsets(journal));
    try testing.expectEqual(@as(usize, 1), pageSlotFor(1));
    try testing.expectEqual(@as(usize, 0), pageSlotFor(4));
    try testing.expectEqual(@as(u64, 139264), appender0FormatPageOffset(journal));
    try testing.expectEqual(ExtentRef{ .start = 151552, .len = 1032192 }, appender0RingExtent(journal));
    var page: [4096]u8 = undefined;
    appender0FreePage(journal, &page);
    try testing.expectEqual(appender_page_magic, std.mem.readInt(u32, page[0..4], .little));
    try testing.expectEqual(@as(u32, 0), std.mem.readInt(u32, page[4..8], .little));
    try testing.expectEqual(@as(u64, 1), std.mem.readInt(u64, page[8..16], .little));
    try testing.expect(checksum.verify(&page, 16));
    try testing.expect(std.mem.allEqual(u8, page[24..55], 0));
    try testing.expectEqual(@as(u8, 1), page[55]);
    try testing.expect(std.mem.allEqual(u8, page[56..72], 0));
    try testing.expectEqual(@as(u16, 1), std.mem.readInt(u16, page[72..74], .little));
    try testing.expect(std.mem.allEqual(u8, page[74..80], 0));
    try testing.expectEqual(@as(u64, 151552), std.mem.readInt(u64, page[80..88], .little));
    try testing.expectEqual(@as(u64, 1032192), std.mem.readInt(u64, page[88..96], .little));
    try testing.expect(std.mem.allEqual(u8, page[96..], 0));
    try testing.expectEqual(@as(usize, 108), slot_page_budget);
}

test "KVAD directory header: magic, chain 0, checksum at 8, next none, pairs (node_size/4096 − 1)/2 at 32; header at the extent's last page" {
    var page: [4096]u8 = undefined;
    dirHeaderPage(262144, &page);
    try testing.expectEqual(appender_dir_magic, std.mem.readInt(u32, page[0..4], .little));
    try testing.expectEqual(@as(u32, 0), std.mem.readInt(u32, page[4..8], .little));
    try testing.expect(checksum.verify(&page, 8));
    try testing.expect(std.mem.allEqual(u8, page[16..32], 0));
    try testing.expectEqual(@as(u16, 31), std.mem.readInt(u16, page[32..34], .little));
    try testing.expect(std.mem.allEqual(u8, page[34..], 0));
    try testing.expectEqual(@as(u64, 7), dirPairsPerExtent(65536));
    try testing.expectEqual(@as(u64, 127), dirPairsPerExtent(1048576));
    try testing.expectEqual(@as(u64, 1245184 + 65536 - 4096), dirHeaderOffset(.{ .start = 1245184, .len = 65536 }));
}

test "build plan: the 13-step order; forest claims the dir, writes KVAD and the appender page; flat adds tree 6 only under bit 9; ledger seq 1 lands in slot 1" {
    const sb = try plan(64 * MiB, 65536, MiB, test_uuid, test_seed, null);
    const forest = buildPlan(sb, .symmetric, default_format_word);
    const fs = forest.slice();
    try testing.expectEqual(@as(usize, 12), fs.len);
    try testing.expect(fs[0] == .register_device);
    try testing.expectEqual(@as(u64, 0), fs[1].zero_range.offset);
    try testing.expectEqual(sb.heap.start, fs[1].zero_range.len);
    try testing.expectEqual(TreeSpec{ .tree_id = 8, .records = .control_empty }, fs[2].tree_image);
    try testing.expectEqual(TreeSpec{ .tree_id = 0, .records = .native }, fs[3].tree_image);
    try testing.expect(fs[4] == .claim_dir_extent);
    try testing.expect(fs[5] == .write_dir_header);
    try testing.expectEqual(@as(u64, 139264), fs[6].write_appender_page.offset);
    try testing.expectEqual(sb.alloc_bitmap.start, fs[7].write_bitmap_pages.base);
    try testing.expectEqual(@as(u64, 1), fs[7].write_bitmap_pages.generation);
    try testing.expectEqual(@as(u64, 4096 + 4096), fs[8].write_ledger_record.offset);
    try testing.expectEqual(@as(u64, 1), fs[8].write_ledger_record.seq);
    try testing.expectEqual(@as(u64, 0), fs[8].write_ledger_record.journal_tail_seq);
    try testing.expectEqual(@as(u64, 1), fs[8].write_ledger_record.alloc_bitmap_generation);
    try testing.expect(fs[9] == .barrier);
    try testing.expect(fs[10] == .write_superblock);
    try testing.expect(fs[11] == .barrier);

    const flat = buildPlan(sb, .single_writer, single_writer_word);
    const fl = flat.slice();
    try testing.expectEqual(@as(usize, 10), fl.len);
    try testing.expectEqual(@as(u8, 1), fl[2].tree_image.tree_id);
    try testing.expectEqual(@as(u8, 2), fl[3].tree_image.tree_id);
    try testing.expectEqual(@as(u8, 3), fl[4].tree_image.tree_id);
    try testing.expect(fl[5] == .write_bitmap_pages);
    for (fl) |s| try testing.expect(s != .claim_dir_extent and s != .write_dir_header and s != .write_appender_page);

    const refs = buildPlan(sb, .single_writer, featureWord(.single_writer, .{ .stamp_block_refs = true }));
    try testing.expectEqual(@as(usize, 11), refs.slice().len);
    try testing.expectEqual(TreeSpec{ .tree_id = 6, .records = .block_refs_empty }, refs.slice()[5].tree_image);
    // The pre-flip class: nine bits, flat trees, no appender region.
    const mwf = buildPlan(sb, .multi_writer_flat, multi_writer_flat_word);
    try testing.expectEqual(@as(usize, 11), mwf.slice().len);
    for (mwf.slice()) |s| try testing.expect(s != .claim_dir_extent);
}
