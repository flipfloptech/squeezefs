//! Volume geometry and the format-time image plan (charter §5.1;
//! `docs/porting/kv_alloc_superblock.md` §2, §9, §10, §12): `plan` lays the
//! fixed structures, the uuid-namespaced node-seq base, and the build order
//! as a declarative plan whose node-image and ledger-record steps are typed
//! placeholders the format task fills from the node and ledger codecs.
//! Pure: no device I/O, no randomness (production randomness lives only in
//! `uuid`, `set_uuid` and `hash_seed`, all inputs here).
//!
//! Format v4 builds two classes of ONE forest (decision 0003 Q2; charter
//! §5.5): the SYMMETRIC class — the default, `SYMMETRIC_PLANE` stamped,
//! the appender directory's first link at heap extent 0 and appender 0's
//! `Free` page at the fixed ring's slot 1 — and the `--single-writer`
//! opt-out, the same control tree (kind 8: the set's control records,
//! `set_config` on member 0) and native slot tree with no appender region.
//! The class changes no byte of any tree.

const std = @import("std");
const checksum = @import("../core/checksum.zig");
const alloc_core = @import("alloc_core.zig");
const alloc_ext = @import("alloc_ext.zig");
const superblock = @import("superblock.zig");

pub const ExtentRef = superblock.ExtentRef;
pub const Superblock = superblock.Superblock;

// ---------------------------------------------------------------------------
// The journal ring clamp and the override floor.
// ---------------------------------------------------------------------------

/// Resolved OQ 1: a flat 32 MiB would be ~12 % of a 256 MiB volume; the
/// 8 MiB floor still holds the 256 KiB reserve + 128 KiB max entry many times.
pub const journal_ring_min: u64 = 8 << 20;
pub const journal_ring_max: u64 = 32 << 20;
/// The charter's whole-entry cap (`journal.MAX_ENTRY_LEN` carries the WHY).
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

/// The format class (charter §5.5): the symmetric plane by default;
/// `--single-writer` is the one flat opt-out.
pub const Class = enum { symmetric, single_writer };

/// Appender 0's four page slots are the fixed journal extent's first four
/// pages (charter §5.5); the ring proper begins behind them. The journal
/// extent must hold them AND the override floor's ring.
pub const appender0_reserved_bytes: u64 = 4 * journal_page_len;

pub const PlanRefusal = union(enum) {
    invalid_node_size: u64,
    invalid_journal_override: struct { len: u64, floor: u64 },
    volume_too_small: struct { volume_len: u64, total: u64, node_size: u64, journal_len: u64, heap_start: u64, min: u64 },

    pub fn write(self: PlanRefusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .invalid_node_size => |n| try w.print("invalid node_size {d}: must be a 4 KiB multiple in [65536, 1048576]", .{n}),
            .invalid_journal_override => |o| try w.print("journal ring override {d} bytes is invalid: must be a 4 KiB multiple of at least {d} bytes (reserve + one max entry{s})", .{ o.len, o.floor, if (o.floor != journal_override_floor) " + appender 0's four page slots" else "" }),
            .volume_too_small => |v| try w.print("metadata volume too small for format v{d}: {d} bytes leaves {d} heap extents of {d} bytes after the fixed structures (superblock + ledger + {d}-byte journal + bitmap end at {d}); at least {d} extents are required — grow the volume or pass a smaller --meta-journal-mb / --meta-node-kib", .{ superblock.version_v5, v.volume_len, v.total, v.node_size, v.journal_len, v.heap_start, v.min }),
        }
    }
};

/// The set-wide identity a format receives: the set uuid and the hash seed
/// — an INDEPENDENT random u64 minted at set creation (`Io.randomSecure`,
/// i.e. `getrandom(2)`), never derived from any identifier a URI or `.stats`
/// prints (charter §5.1 "random `hash_seed`"; decision 0002 row 42). Every
/// member carries the same pair.
pub const SetIdentity = struct {
    set_uuid: [16]u8,
    hash_seed: u64,
};

/// Mint a new set's identity from the OS entropy source: two independent
/// draws, so the seed is never a function of the uuid.
pub fn mintSetIdentity(io: std.Io) std.Io.RandomSecureError!SetIdentity {
    var set: SetIdentity = undefined;
    try io.randomSecure(&set.set_uuid);
    try io.randomSecure(std.mem.asBytes(&set.hash_seed));
    return set;
}

/// `Superblock::plan`: ledger at 4096 (32 × 4 KiB), journal per the clamp
/// or the override, bitmap over-reserved for `volume_len / node_size`
/// extents (deliberate: it breaks the bitmap↔heap circularity), heap start
/// aligned to `node_size`, the LAST sector held back for the DUR-5 copy
/// (`usable = volume_len − 4096`), and the floor `reserve + 4` (two roots
/// and growth) — plus one extent for the appender directory on the
/// symmetric class, which also stamps `SYMMETRIC_PLANE` and names the
/// directory at heap extent 0. v4's always-on mechanisms are the version.
pub fn plan(volume_len: u64, node_size: u64, journal_len_override: ?u64, uuid: [16]u8, set: SetIdentity, class: Class, diag: ?*PlanRefusal) PlanError!Superblock {
    if (!superblock.nodeSizeValid(node_size)) {
        if (diag) |d| d.* = .{ .invalid_node_size = node_size };
        return error.InvalidNodeSize;
    }
    const root_ledger: ExtentRef = .{ .start = superblock.sector_len, .len = superblock.root_ledger_len };
    // The symmetric class's ring proper sits behind appender 0's four page
    // slots, so the override must leave the floor behind them.
    const override_floor = journal_override_floor + (if (class == .symmetric) appender0_reserved_bytes else 0);
    const journal_len = if (journal_len_override) |len| blk: {
        if (len % journal_page_len != 0 or len < override_floor) {
            if (diag) |d| d.* = .{ .invalid_journal_override = .{ .len = len, .floor = override_floor } };
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
    const dir_extents: u64 = if (class == .symmetric) 1 else 0;
    const min_extents = alloc_core.compactionReserveExtents(total) + 4 + dir_extents;
    if (total < min_extents) {
        if (diag) |d| d.* = .{ .volume_too_small = .{ .volume_len = volume_len, .total = total, .node_size = node_size, .journal_len = journal_len, .heap_start = heap_start, .min = min_extents } };
        return error.VolumeTooSmall;
    }
    return .{
        .node_size = @intCast(node_size),
        .volume_len = volume_len,
        .features_incompat = if (class == .symmetric) superblock.symmetric_plane else 0,
        .features_ro = 0,
        .root_ledger = root_ledger,
        .journal = journal,
        .alloc_bitmap = alloc_bitmap,
        .heap = .{ .start = heap_start, .len = total * node_size },
        .appender_dir = if (class == .symmetric) .{ .start = heap_start, .len = node_size } else .none,
        .uuid = uuid,
        .set_uuid = set.set_uuid,
        .hash_seed = set.hash_seed,
    };
}

/// The ring proper of the fixed journal extent: the whole extent on the
/// single-writer class, the extent behind appender 0's four page slots on
/// the symmetric class.
pub fn fixedRingExtent(sb: Superblock) ExtentRef {
    if (!sb.symmetricPlaneStamped()) return sb.journal;
    return .{ .start = sb.journal.start + appender0_reserved_bytes, .len = sb.journal.len - appender0_reserved_bytes };
}

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
// The routing width and node_seq_base.
// ---------------------------------------------------------------------------

/// `W = 2^16`: the u16 slot-id wire type, derived, frozen forever.
pub const routing_width: u32 = 65_536;

/// `u64 LE of uuid[0..8] & (u64::MAX >> 1)`: quick format leaves heap frames
/// behind, and uuid-namespaced seqs make a cross-generation chain match as
/// improbable as a checksum collision; the clear top bit is 2^63 of headroom.
pub fn nodeSeqBase(uuid: [16]u8) u64 {
    return std.mem.readInt(u64, uuid[0..8], .little) & (std.math.maxInt(u64) >> 1);
}

// ---------------------------------------------------------------------------
// The build order as a declarative plan.
// ---------------------------------------------------------------------------

/// What a tree's records are at format time.
pub const TreeRecords = enum {
    /// Tree 0 (`TREE_CONTROL = 8`): the format-time control records — the
    /// `set_config` record on the set's member 0 (charter §8; rung 4),
    /// nothing on any other member — still one real leaf when empty.
    control,
    /// The native slot tree: every record of the description (kinds 1, 2,
    /// 3), memcmp-sorted — one mixed-kind tree.
    native,
};

pub const TreeSpec = struct { records: TreeRecords };

/// The two trees of the one-member forest, in write order (the control
/// tree first: its root is the ledger's first).
pub const forest_trees = [_]TreeSpec{ .{ .records = .control }, .{ .records = .native } };

/// The bootstrap ledger record is `seq 1` ⇒ slot `1 % 32 = 1`, offset
/// `root_ledger.start + 4096` — NOT slot 0.
pub const bootstrap_ledger_seq: u64 = 1;
pub const bitmap_format_generation: u64 = 1;

pub const BuildStep = union(enum) {
    /// 1. O_DIRECT from the first byte.
    register_device,
    /// 2. Quick-format hygiene: SB + ledger + ring + bitmap; the heap is NOT
    /// wiped (dead frames fail the uuid-namespaced seq chain).
    zero_range: struct { offset: u64, len: u64 },
    /// 3. One tree in the fixed order: the format task packs its records
    /// bottom-up (¾ fill), claiming one extent per node lowest-first
    /// (`claim_internal`) and minting seqs `base+1, base+2, …`. The node
    /// images are the node codec's.
    tree_image: TreeSpec,
    /// 4. Every page dirty ⇒ the whole bitmap at generation 1 into slot A
    /// (`base + i × 8192`), bits = every claimed extent.
    write_bitmap_pages: struct { base: u64, generation: u64 },
    /// 5. `{seq 1, roots, journal_tail_seq 0, next_ino, alloc_bitmap_generation
    /// 1, node_seq_watermark = last minted seq, stamp}` — the ledger slot
    /// codec — at the slot offset below.
    write_ledger_record: struct { offset: u64, seq: u64, journal_tail_seq: u64, alloc_bitmap_generation: u64 },
    /// 6, 8. `fdatasync`.
    barrier,
    /// 7. `superblock.planWrite(sb, nextSuperblockGeneration(primary,
    /// tail))`: tail copy, barrier, sector 0, barrier.
    write_superblock,
    /// Symmetric class, after the zero of the fixed region: the directory
    /// extent (heap extent 0, claimed before any tree) zeroed and its
    /// `KVAD` header page written FIRST at the extent's start.
    write_dir_header: struct { extent: ExtentRef },
    /// Symmetric class: appender 0's `Free` page at the fixed ring's slot
    /// `pageSlotFor(generation 1) = 1` (`journal.start + 4096`), naming the
    /// ring proper as its one segment; the other three slots stay zero.
    write_appender0_page: struct { offset: u64, ring: ExtentRef },
};

pub const BuildPlan = struct {
    steps: [11]BuildStep,
    len: u8,

    pub fn slice(self: *const BuildPlan) []const BuildStep {
        return self.steps[0..self.len];
    }

    fn push(self: *BuildPlan, step: BuildStep) void {
        self.steps[self.len] = step;
        self.len += 1;
    }
};

/// The image builder's write/barrier order for a planned superblock. The
/// in-memory steps (plan, allocator format, layout, writer) have no entry:
/// they are the format task's setup, not device order. Atomicity: sector 0
/// is zeroed at step 2 and rewritten LAST, so no crash prefix is mountable
/// (it reads Blank) and a re-run needs no `--force`.
pub fn buildPlan(sb: Superblock) BuildPlan {
    var p: BuildPlan = .{ .steps = undefined, .len = 0 };
    p.push(.register_device);
    p.push(.{ .zero_range = .{ .offset = 0, .len = sb.heap.start } });
    if (sb.symmetricPlaneStamped()) {
        p.push(.{ .write_dir_header = .{ .extent = sb.appender_dir } });
        p.push(.{ .write_appender0_page = .{ .offset = sb.journal.start + journal_page_len, .ring = fixedRingExtent(sb) } });
    }
    for (forest_trees) |t| p.push(.{ .tree_image = t });
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
const test_set: SetIdentity = .{ .set_uuid = "set-uuid-fixture".*, .hash_seed = 0x5EED_CAFE_F00D_D00D };

fn planMsg(buf: []u8, r: PlanRefusal) []const u8 {
    var w: std.Io.Writer = .fixed(buf);
    r.write(&w) catch unreachable;
    return w.buffered();
}

test "mintSetIdentity: two mints differ in both fields and the seed is not hash64(set_uuid) (independent draws)" {
    const a = try mintSetIdentity(testing.io);
    const b = try mintSetIdentity(testing.io);
    try testing.expect(!std.mem.eql(u8, &a.set_uuid, &b.set_uuid));
    try testing.expect(a.hash_seed != b.hash_seed);
    try testing.expect(a.hash_seed != checksum.hash64(&a.set_uuid));
    try testing.expect(a.hash_seed != std.mem.readInt(u64, a.set_uuid[0..8], .little));
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

test "plan (single-writer class): the four worked examples tie (64 MiB/64 KiB/1 MiB override, 256 MiB, 1 GiB, 1 TiB); volume_len stored; no capability bit" {
    const a = try plan(64 * MiB, 65536, MiB, test_uuid, test_set, .single_writer, null);
    try testing.expectEqual(ExtentRef{ .start = 4096, .len = 131072 }, a.root_ledger);
    try testing.expectEqual(ExtentRef{ .start = 135168, .len = 1048576 }, a.journal);
    try testing.expectEqual(ExtentRef{ .start = 1183744, .len = 8192 }, a.alloc_bitmap);
    try testing.expectEqual(ExtentRef{ .start = 1245184, .len = 1004 * 65536 }, a.heap);
    try testing.expectEqual(@as(u64, 1004), a.totalExtents());
    try testing.expectEqual(@as(u64, 20), alloc_core.compactionReserveExtents(a.totalExtents()));
    try testing.expectEqual(64 * MiB, a.volume_len);
    try testing.expectEqual(@as(u64, 67104768), a.backupOffset());
    try testing.expectEqual(@as(u64, 0), a.features_incompat);
    try testing.expectEqual(@as(u64, 0), a.features_ro);
    try testing.expect(a.appender_dir.isNone());
    try testing.expectEqual(test_uuid, a.uuid);
    try testing.expectEqual(test_set.set_uuid, a.set_uuid);
    try testing.expectEqual(test_set.hash_seed, a.hash_seed);
    try a.validateGeometry(null);

    const b = try plan(256 * MiB, 262144, null, test_uuid, test_set, .single_writer, null);
    try testing.expectEqual(ExtentRef{ .start = 135168, .len = 8388608 }, b.journal);
    try testing.expectEqual(ExtentRef{ .start = 8523776, .len = 8192 }, b.alloc_bitmap);
    try testing.expectEqual(ExtentRef{ .start = 8650752, .len = 990 * 262144 }, b.heap);
    try testing.expectEqual(@as(u64, 19), alloc_core.compactionReserveExtents(b.totalExtents()));
    try testing.expectEqual(@as(u64, 268431360), b.backupOffset());

    const c = try plan(GiB, 262144, null, test_uuid, test_set, .single_writer, null);
    try testing.expectEqual(ExtentRef{ .start = 135168, .len = 16777216 }, c.journal);
    try testing.expectEqual(ExtentRef{ .start = 16912384, .len = 8192 }, c.alloc_bitmap);
    try testing.expectEqual(ExtentRef{ .start = 17039360, .len = 4030 * 262144 }, c.heap);
    try testing.expectEqual(@as(u64, 80), alloc_core.compactionReserveExtents(c.totalExtents()));

    const d = try plan(TiB, 262144, null, test_uuid, test_set, .single_writer, null);
    try testing.expectEqual(ExtentRef{ .start = 135168, .len = 33554432 }, d.journal);
    try testing.expectEqual(ExtentRef{ .start = 33689600, .len = 1056768 }, d.alloc_bitmap);
    try testing.expectEqual(ExtentRef{ .start = 34865152, .len = 4194170 * 262144 }, d.heap);
    try testing.expectEqual(@as(u64, 83883), alloc_core.compactionReserveExtents(d.totalExtents()));
    try d.validateGeometry(null);
    // The heap start is aligned to node_size (the validator demands it);
    // the bitmap is over-reserved for volume/node_size.
    try testing.expectEqual(@as(u64, 0), d.heap.start % 262144);
    try testing.expect(d.alloc_bitmap.len >= alloc_ext.bitmapRegionLen(d.totalExtents()));
}

test "plan: default = the clamp (8 MiB on 64 MiB); an override is honoured; SB|ledger|ring|bitmap|heap ascend; a 4 MiB volume fails with the exact message" {
    const def = try plan(64 * MiB, 65536, null, test_uuid, test_set, .single_writer, null);
    try testing.expectEqual(8 * MiB, def.journal.len);
    const ov = try plan(64 * MiB, 65536, 2 * MiB, test_uuid, test_set, .single_writer, null);
    try testing.expectEqual(2 * MiB, ov.journal.len);
    try testing.expect(ov.root_ledger.start >= 4096);
    try testing.expect(ov.journal.start >= ov.root_ledger.end());
    try testing.expect(ov.alloc_bitmap.start >= ov.journal.end());
    try testing.expect(ov.heap.start >= ov.alloc_bitmap.end());
    var diag: PlanRefusal = undefined;
    var buf: [512]u8 = undefined;
    try testing.expectError(error.VolumeTooSmall, plan(4 * MiB, 65536, null, test_uuid, test_set, .single_writer, &diag));
    // 4 MiB: journal 8 MiB already exceeds the volume ⇒ 0 extents, min 12.
    try testing.expectEqualStrings("metadata volume too small for format v5: 4194304 bytes leaves 0 heap extents of 65536 bytes after the fixed structures (superblock + ledger + 8388608-byte journal + bitmap end at 8585216); at least 12 extents are required — grow the volume or pass a smaller --meta-journal-mb / --meta-node-kib", planMsg(&buf, diag));
    // The override floor and alignment.
    _ = try plan(64 * MiB, 65536, journal_override_floor, test_uuid, test_set, .single_writer, &diag);
    _ = try plan(64 * MiB, 65536, journal_override_floor + appender0_reserved_bytes, test_uuid, test_set, .symmetric, &diag);
    try testing.expectError(error.InvalidJournalOverride, plan(64 * MiB, 65536, journal_override_floor, test_uuid, test_set, .symmetric, &diag));
    try testing.expectEqualStrings("journal ring override 393216 bytes is invalid: must be a 4 KiB multiple of at least 409600 bytes (reserve + one max entry + appender 0's four page slots)", planMsg(&buf, diag));
    try testing.expectError(error.InvalidJournalOverride, plan(64 * MiB, 65536, journal_override_floor - 4096, test_uuid, test_set, .single_writer, &diag));
    try testing.expectEqualStrings("journal ring override 389120 bytes is invalid: must be a 4 KiB multiple of at least 393216 bytes (reserve + one max entry)", planMsg(&buf, diag));
    try testing.expectError(error.InvalidJournalOverride, plan(64 * MiB, 65536, journal_override_floor + 1, test_uuid, test_set, .single_writer, &diag));
    try testing.expectError(error.InvalidNodeSize, plan(64 * MiB, 4096, null, test_uuid, test_set, .single_writer, &diag));
    try testing.expectEqualStrings("invalid node_size 4096: must be a 4 KiB multiple in [65536, 1048576]", planMsg(&buf, diag));
    // The DUR-5 tail: the last sector is never inside the heap.
    try testing.expect(def.heap.end() <= 64 * MiB - 4096);
    // Exactly the floor: reserve 8 + 4 = 12 extents on a 64 KiB node.
    const tiny_len: u64 = 1245184 + 12 * 65536 + 4096;
    const tiny = try plan(tiny_len, 65536, MiB, test_uuid, test_set, .single_writer, null);
    try testing.expectEqual(@as(u64, 12), tiny.totalExtents());
    try testing.expectError(error.VolumeTooSmall, plan(tiny_len - 1, 65536, MiB, test_uuid, test_set, .single_writer, null));
    // The symmetric class needs ONE more extent (the directory) and refuses
    // one short; it stamps the bit and names heap extent 0.
    try testing.expectError(error.VolumeTooSmall, plan(tiny_len, 65536, MiB, test_uuid, test_set, .symmetric, &diag));
    try testing.expectEqual(@as(u64, 13), diag.volume_too_small.min);
    const sym = try plan(tiny_len + 65536, 65536, MiB, test_uuid, test_set, .symmetric, null);
    try testing.expect(sym.symmetricPlaneStamped());
    try testing.expectEqual(ExtentRef{ .start = sym.heap.start, .len = 65536 }, sym.appender_dir);
    try sym.validateGeometry(null);
    try testing.expectEqual(ExtentRef{ .start = sym.journal.start + 16_384, .len = MiB - 16_384 }, fixedRingExtent(sym));
    try testing.expectEqual(def.journal, fixedRingExtent(def));
    // The planned superblock decodes through its own gate.
    var img: [4096]u8 = undefined;
    try def.encodeSector(0, &img, null);
    const back = try Superblock.decodeSector(&img, null);
    try testing.expectEqual(def, back);
    try testing.expect(!back.symmetricPlaneStamped());
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

test "node_seq_base = uuid[0..8] LE with the top bit clear; W = 2^16" {
    try testing.expectEqual(std.mem.readInt(u64, "kv-backe", .little), nodeSeqBase(test_uuid));
    const ones: [16]u8 = @splat(0xFF);
    try testing.expectEqual(std.math.maxInt(u64) >> 1, nodeSeqBase(ones));
    try testing.expectEqual(@as(u64, 0), nodeSeqBase(@splat(0)));
    try testing.expectEqual(@as(u32, 65536), routing_width);
}

test "build plan: the single-writer class's 9 steps — zero the fixed region, tree 0 (the control records) then the native slot tree, the bitmap, the ledger record in slot 1, barrier, superblock, barrier; the symmetric class adds the directory header and appender 0's Free page right after the zero" {
    const sym = try plan(64 * MiB, 65536, MiB, test_uuid, test_set, .symmetric, null);
    const sbp = buildPlan(sym);
    const ss = sbp.slice();
    try testing.expectEqual(@as(usize, 11), ss.len);
    try testing.expectEqual(sym.appender_dir, ss[2].write_dir_header.extent);
    try testing.expectEqual(sym.journal.start + 4096, ss[3].write_appender0_page.offset);
    try testing.expectEqual(fixedRingExtent(sym), ss[3].write_appender0_page.ring);
    try testing.expectEqual(TreeSpec{ .records = .control }, ss[4].tree_image);
    const sb = try plan(64 * MiB, 65536, MiB, test_uuid, test_set, .single_writer, null);
    const bp = buildPlan(sb);
    const s = bp.slice();
    try testing.expectEqual(@as(usize, 9), s.len);
    try testing.expect(s[0] == .register_device);
    try testing.expectEqual(@as(u64, 0), s[1].zero_range.offset);
    try testing.expectEqual(sb.heap.start, s[1].zero_range.len);
    try testing.expectEqual(TreeSpec{ .records = .control }, s[2].tree_image);
    try testing.expectEqual(TreeSpec{ .records = .native }, s[3].tree_image);
    try testing.expectEqual(sb.alloc_bitmap.start, s[4].write_bitmap_pages.base);
    try testing.expectEqual(@as(u64, 1), s[4].write_bitmap_pages.generation);
    try testing.expectEqual(@as(u64, 4096 + 4096), s[5].write_ledger_record.offset);
    try testing.expectEqual(@as(u64, 1), s[5].write_ledger_record.seq);
    try testing.expectEqual(@as(u64, 0), s[5].write_ledger_record.journal_tail_seq);
    try testing.expectEqual(@as(u64, 1), s[5].write_ledger_record.alloc_bitmap_generation);
    try testing.expect(s[6] == .barrier);
    try testing.expect(s[7] == .write_superblock);
    try testing.expect(s[8] == .barrier);
}
