//! `squeezefs format`'s pure half (charter §5.1; `docs/porting/kv_alloc_superblock.md`
//! §2.2–§2.3, §9, §12): the complete, valid, checkpointed v4 image of one
//! metadata volume as an ordered list of `(offset, bytes, barrier_after)`
//! steps in the builder's write/barrier order, plus `formatImage`, which
//! applies them to an in-RAM volume. The io binding registers the device
//! (O_DIRECT from the first byte) and executes the steps; nothing here
//! touches a device or randomness (`uuid` and the set identity are inputs).
//!
//! Every volume is a one-member forest (decision 0003 Q2): the control tree
//! (kind 8: the caller's format-time control records — the `set_config`
//! record on the set's member 0, empty elsewhere) and the native slot tree
//! holding the description's records under their kind-carrying keys. The
//! SYMMETRIC class (the default, charter §5.5) also stamps `SYMMETRIC_PLANE`
//! and writes the appender directory's first link (heap extent 0, zeroed,
//! its `KVAD` header FIRST) and appender 0's `Free` page at the fixed ring's
//! slot 1; `--single-writer` is the flat opt-out with no appender region.
//!
//! The order is `geometry.buildPlan`'s: zero `[0, heap.start)` (quick-format
//! hygiene — the heap is NOT wiped; dead frames fail the uuid-namespaced seq
//! chain) → [the directory link + appender 0's page] → every tree root as a
//! REAL leaf (an empty tree is still one header-only node) → every bitmap
//! page at generation 1 → the bootstrap ledger record (seq 1 ⇒ slot 1) →
//! barrier → the superblock's tail copy, barrier, sector 0, barrier. Sector
//! 0 is zeroed first and rewritten LAST, so no crash prefix is mountable
//! (it reads Blank).

const std = @import("std");
const key = @import("key.zig");
const rec = @import("record.zig");
const bset = @import("bset.zig");
const node = @import("node.zig");
const node_seq = @import("node_seq.zig");
const alloc_core = @import("alloc_core.zig");
const alloc_ext = @import("alloc_ext.zig");
const superblock = @import("superblock.zig");
const geometry = @import("geometry.zig");
const ledger = @import("ledger.zig");
const builder = @import("builder.zig");
const appender_page = @import("sym/appender_page.zig");
const appender_dir = @import("sym/appender_dir.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
const Superblock = superblock.Superblock;
const ExtentRef = superblock.ExtentRef;

/// The wipe chunk.
pub const zero_chunk: usize = 1 << 20;

/// The format options plus the identities the CLI mints (random in
/// production; fixed for deterministic images).
pub const FormatOptions = struct {
    /// The symmetric plane by default; `--single-writer` is the opt-out.
    class: geometry.Class = .symmetric,
    node_size: u64 = superblock.default_node_size,
    /// `--meta-journal-mb` in bytes; null = the clamp.
    journal_len_override: ?u64 = null,
    /// Per-volume identity: the node-seq namespace and the superblock's.
    uuid: [16]u8,
    /// The set's identity: the membership stamp's `set_uuid` and the
    /// independent random `hash_seed` (decision 0002 row 42).
    set: geometry.SetIdentity,
    /// This member's place in the set (`plan_meta_slot_set(n).stamps[pos]`).
    member_position: u16 = 0,
    member_count: u16 = 1,
    /// An explicit stamp instead of the fresh member's derivation: the
    /// `add-meta` joiner's HOSTLESS transient stamp (its claim lands after
    /// the copy, per the write-0-before-claim order). Borrows the caller's
    /// runs for the plan's life.
    stamp_override: ?ledger.MembershipStamp = null,
    /// The root directory's owner: the invoking user (`SUDO_UID:SUDO_GID`
    /// under sudo); the builder default is 0:0.
    root_uid: u32 = 0,
    root_gid: u32 = 0,
    /// `nextSuperblockGeneration(primary, tail)` as read BEFORE the wipe:
    /// 1 on a blank volume; a reformat over a valid tail copy continues
    /// its generation so DUR-5's newest-valid-wins never picks the stale
    /// copy over the fresh primary.
    sb_generation: u64 = 1,
    /// Tree 0's format-time records, key-sorted (the `set_config` record on
    /// the set's member 0 — `data/set_config.zig`; charter §8); empty on
    /// every other member. Seq 0 like every format-time record.
    control_records: []const rec.RecordRef = &.{},
};

/// One device write; `barrier_after` is an `fdatasync` once it landed.
pub const Step = struct {
    offset: u64,
    bytes: []const u8,
    barrier_after: bool,
};

pub const FormatError = geometry.PlanError || builder.DescribeError || builder.WriteError || superblock.DecodeError || ledger.Error;

/// The planned image: every step's bytes live in the plan's arena.
pub const FormatPlan = struct {
    arena: std.heap.ArenaAllocator,
    steps: []Step,
    superblock: Superblock,
    ledger_seq: u64,
    next_ino: u64,
    /// Btree nodes written (both trees, all levels) — one heap extent each.
    nodes_written: u64,
    /// `nodes_written` plus the directory link on the symmetric class.
    extents_allocated: u64,
    node_seq_watermark: u64,
    /// `[control, native_slot]` in write order.
    tree_roots: []ledger.TreeRoot,

    pub fn deinit(self: *FormatPlan) void {
        self.arena.deinit();
        self.* = undefined;
    }

    /// Apply every step to an in-RAM volume of the planned length.
    pub fn apply(self: *const FormatPlan, image: []u8) void {
        for (self.steps) |s| @memcpy(image[@intCast(s.offset)..][0..s.bytes.len], s.bytes);
    }

    pub fn barrierCount(self: *const FormatPlan) usize {
        var n: usize = 0;
        for (self.steps) |s| n += @intFromBool(s.barrier_after);
        return n;
    }
};

/// The root-only description (owner stamped), member `position` of
/// `count`'s membership stamp — then `build`.
pub fn planFormat(gpa: Allocator, volume_len: u64, opts: FormatOptions, diag: ?*geometry.PlanRefusal) FormatError!FormatPlan {
    var desc = try builder.ImageBuilder.init(gpa, .{
        .node_size = @intCast(opts.node_size),
        .journal_len_override = opts.journal_len_override,
        .hash_seed = opts.set.hash_seed,
        .uuid = opts.uuid,
    });
    defer desc.deinit();
    try desc.setOwner(builder.ROOT_INO, opts.root_uid, opts.root_gid);
    return planFormatWith(gpa, volume_len, opts, &desc, diag);
}

/// `ImageBuilder::build` over an arbitrary description (tests build fixture
/// volumes through it).
pub fn planFormatWith(gpa: Allocator, volume_len: u64, opts: FormatOptions, desc: *const builder.ImageBuilder, diag: ?*geometry.PlanRefusal) FormatError!FormatPlan {
    assert(desc.cfg.hash_seed == opts.set.hash_seed);
    var arena = std.heap.ArenaAllocator.init(gpa);
    errdefer arena.deinit();
    const a = arena.allocator();

    const sb = try geometry.plan(volume_len, opts.node_size, opts.journal_len_override, opts.uuid, opts.set, opts.class, diag);
    const build_plan = geometry.buildPlan(sb);

    // The allocator over the planned heap; the layout (frames stamped
    // `(0, 0)`: the one-member forest's stamp); the writer from the uuid
    // base.
    const total = sb.totalExtents();
    var alloc = try alloc_ext.ExtentAllocator.format(gpa, total, alloc_core.compactionReserveExtents(total), alloc_ext.builder_pending_cap);
    defer alloc.deinit();
    // The directory link is heap extent 0, claimed BEFORE any tree so the
    // trees pack behind it.
    var dir_extents: u64 = 0;
    if (sb.symmetricPlaneStamped()) {
        const e = alloc.claimInternal() catch return error.VolumeTooSmall;
        assert(e == 0 and sb.appender_dir.start == sb.heap.start);
        dir_extents = 1;
    }
    const layout = try node.NodeLayout.new(@intCast(opts.node_size), null);
    var images: std.ArrayList(node.WrittenNode) = .empty;
    defer images.deinit(a);
    var writer = builder.TreeWriter.init(a, layout, sb.heap.start, &alloc, node_seq.nodeSeqBase(opts.uuid), &images);

    var steps: std.ArrayList(Step) = .empty;
    var roots: std.ArrayList(ledger.TreeRoot) = .empty;
    const zeros = try a.alloc(u8, zero_chunk);
    @memset(zeros, 0);

    for (build_plan.slice()) |step| switch (step) {
        // The io binding's precondition, not a write.
        .register_device => {},
        .zero_range => |z| try pushZero(a, &steps, zeros, z.offset, z.len),
        .tree_image => |t| {
            const id: ledger.RootId = switch (t.records) {
                .control => .control,
                .native => .native_slot,
            };
            const refs: []const rec.RecordRef = switch (t.records) {
                .control => opts.control_records,
                // The native keyspace belongs to slot 0's host (member 0,
                // `ledger.nativeSlotOf`): every other member's native tree
                // is born EMPTY — it holds the volume's own control records
                // (the writer claim at local ino 1) and never a `/`.
                .native => if (opts.member_position != 0) &.{} else blk: {
                    const records = try desc.nativeRecords(a);
                    const out = try a.alloc(rec.RecordRef, records.len);
                    for (records, out) |*r, *o| o.* = r.ref();
                    break :blk out;
                },
            };
            const first_image = images.items.len;
            const root = try writer.writeTree(id.headerTreeId(), refs);
            for (images.items[first_image..]) |*w| try steps.append(a, .{ .offset = w.node_addr, .bytes = w.image, .barrier_after = false });
            try roots.append(a, .{ .id = id, .node_addr = root.addr, .node_seq = root.seq });
        },
        .write_bitmap_pages => |b| {
            var set = try alloc.beginDirtyPageWrite(b.generation);
            defer set.deinit();
            for (set.writes) |*w| {
                const page = try a.dupe(u8, &w.image);
                try steps.append(a, .{ .offset = b.base + w.offset, .bytes = page, .barrier_after = false });
            }
            set.commit(&alloc);
        },
        .write_ledger_record => |l| {
            const run = try a.create([1]ledger.SlotRun);
            const stamp = opts.stamp_override orelse ledger.MembershipStamp.member(opts.set.set_uuid, opts.member_position, opts.member_count, run);
            const record = ledger.bootstrapRecord(roots.items, desc.nextIno(), writer.nodeSeqWatermark(), stamp);
            assert(record.seq == l.seq and record.journal_tail_seq == l.journal_tail_seq and record.alloc_bitmap_generation == l.alloc_bitmap_generation);
            assert(ledger.slotOffset(sb.root_ledger.start, record.slotIndex()) == l.offset);
            const slot = try a.create([ledger.ROOT_LEDGER_SLOT_LEN]u8);
            slot.* = try record.encodeSlot();
            try steps.append(a, .{ .offset = l.offset, .bytes = slot, .barrier_after = false });
        },
        .barrier => {
            // `fdatasync` after everything so far; a barrier right after the
            // superblock's own trailing barrier is the same barrier.
            if (steps.items.len > 0) steps.items[steps.items.len - 1].barrier_after = true;
        },
        .write_superblock => {
            const plan = try superblock.planWrite(sb, opts.sb_generation, null);
            const image = try a.dupe(u8, &plan.image);
            for (plan.slice()) |w| try steps.append(a, .{ .offset = w.offset, .bytes = image, .barrier_after = w.barrier_after });
        },
        .write_dir_header => |d| {
            // The whole link zeroed (a stale pair image there would be a
            // phantom appender), then the header page first.
            try pushZero(a, &steps, zeros, d.extent.start, d.extent.len);
            const hdr = try a.create([appender_dir.PAGE_LEN]u8);
            appender_dir.formatHeader(opts.node_size, hdr);
            try steps.append(a, .{ .offset = d.extent.start, .bytes = hdr, .barrier_after = false });
        },
        .write_appender0_page => |p| {
            const img = try a.create([appender_page.PAGE_LEN]u8);
            const free = appender_page.Page.free(0, 1, appender_page.SegmentTable.one(.{ .start = sb.journal.start, .len = sb.journal.len }));
            assert(appender_page.pageSlotFor(free.generation) == 1 and p.ring.start == sb.journal.start + geometry.appender0_reserved_bytes);
            free.encode(img) catch unreachable;
            try steps.append(a, .{ .offset = p.offset, .bytes = img, .barrier_after = false });
        },
    };

    assert(alloc.freeExtents() == total - writer.nodesWritten() - dir_extents);
    return .{
        .arena = arena,
        .steps = try steps.toOwnedSlice(a),
        .superblock = sb,
        .ledger_seq = geometry.bootstrap_ledger_seq,
        .next_ino = desc.nextIno(),
        .nodes_written = writer.nodesWritten(),
        .extents_allocated = writer.nodesWritten() + dir_extents,
        .node_seq_watermark = writer.nodeSeqWatermark(),
        .tree_roots = try roots.toOwnedSlice(a),
    };
}

/// `zero_range` in ≤ 1 MiB chunks, every chunk a view of ONE zero buffer
/// (the format's and `enable-symmetric`'s).
pub fn pushZero(a: Allocator, steps: *std.ArrayList(Step), zeros: []const u8, start: u64, len: u64) Allocator.Error!void {
    var off = start;
    const end = start + len;
    while (off < end) {
        const n: usize = @intCast(@min(end - off, zeros.len));
        try steps.append(a, .{ .offset = off, .bytes = zeros[0..n], .barrier_after = false });
        off += n;
    }
}

/// The whole volume as one in-RAM image: a zeroed `volume_len` buffer with
/// every step applied (caller frees).
pub fn formatImage(gpa: Allocator, volume_len: u64, opts: FormatOptions) FormatError![]u8 {
    var plan = try planFormat(gpa, volume_len, opts, null);
    defer plan.deinit();
    const image = try gpa.alloc(u8, @intCast(volume_len));
    errdefer gpa.free(image);
    @memset(image, 0);
    plan.apply(image);
    return image;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const MiB: u64 = 1 << 20;
const test_uuid: [16]u8 = "kv-backend-test!".*;
const test_set: geometry.SetIdentity = .{ .set_uuid = "set-uuid-fixture".*, .hash_seed = 0x5EED_CAFE_F00D_D00D };

fn testOpts() FormatOptions {
    return .{ .class = .single_writer, .node_size = 65536, .journal_len_override = MiB, .uuid = test_uuid, .set = test_set };
}

fn symOpts() FormatOptions {
    return .{ .node_size = 65536, .journal_len_override = MiB, .uuid = test_uuid, .set = test_set };
}

/// Mount-side wiring over an image: SB → newest ledger → both roots load.
fn expectMounts(gpa: Allocator, image: []const u8, volume_len: u64, extents_allocated: u64) !void {
    const fmt = try superblock.classifyVolumeImages(image[0..4096], image[@intCast(superblock.backupOffsetFor(volume_len).?)..][0..4096], volume_len, null);
    try testing.expectEqual(superblock.SuperblockSlot.primary, fmt.slot);
    const sb = fmt.format.v5;
    try testing.expectEqual(sb.symmetricPlaneStamped(), !sb.appender_dir.isNone());
    try testing.expectEqual(sb.features_incompat & ~superblock.symmetric_plane, 0);
    const layout = try node.NodeLayout.new(sb.node_size, null);
    var record = (try ledger.newestValid(gpa, image[@intCast(sb.root_ledger.start)..][0..@intCast(sb.root_ledger.len)])) orelse return error.TestUnexpectedResult;
    defer record.deinit(gpa);
    try testing.expectEqual(@as(u64, 1), record.seq);
    try testing.expectEqual(@as(usize, 2), record.tree_roots.len);
    try testing.expectEqualSlices(u8, &test_set.set_uuid, &record.membership_stamp.set_uuid);
    try testing.expectEqual(@as(?u16, 0), record.membership_stamp.native_slot);
    for (record.tree_roots, [_]ledger.RootId{ .control, .native_slot }) |root, id| {
        try testing.expectEqual(id, root.id);
        var n = try node.verifyNodeExtent(gpa, image[@intCast(root.node_addr)..][0..sb.node_size], layout, root.node_addr, record.journal_tail_seq, null);
        defer n.deinit(gpa);
        try testing.expectEqual(root.node_seq, n.header.node_seq);
        try testing.expectEqual(id.headerTreeId(), n.header.tree_id);
        try testing.expectEqual(@as(u64, 0), n.dropped_tail_bsets);
    }
    // The bitmap: the newest page claims exactly the ledger's extents.
    var loaded = try alloc_ext.ExtentAllocator.load(gpa, image[@intCast(sb.alloc_bitmap.start)..][0..@intCast(sb.alloc_bitmap.len)], sb.totalExtents(), alloc_core.compactionReserveExtents(sb.totalExtents()), 4, 0, &.{}, null);
    defer loaded.deinit();
    try testing.expectEqual(@as(u64, 1), loaded.resumeGeneration());
    try testing.expectEqual(sb.totalExtents() - extents_allocated, loaded.freeExtents());
    for (record.tree_roots) |root| try testing.expect(loaded.isAllocated((root.node_addr - sb.heap.start) / sb.node_size));
    // The ring proper is all zero: nothing to replay.
    const ring = geometry.fixedRingExtent(sb);
    try testing.expect(std.mem.allEqual(u8, image[@intCast(ring.start)..][0..@intCast(ring.len)], 0));
}

test "the default format is the symmetric class: the bit stamped, the directory link at heap extent 0 zeroed with its KVAD header first, appender 0's Free page at the fixed ring's slot 1 naming the ring proper, the other three slots zero, the trees packed behind the link; mounts" {
    const gpa = testing.allocator;
    const len = 64 * MiB;
    var plan = try planFormat(gpa, len, symOpts(), null);
    defer plan.deinit();
    const sb = plan.superblock;
    try testing.expect(sb.symmetricPlaneStamped());
    try testing.expectEqual(ExtentRef{ .start = sb.heap.start, .len = 65536 }, sb.appender_dir);
    try testing.expectEqual(@as(u64, 3), plan.extents_allocated);
    try testing.expectEqual(@as(u64, 2), plan.nodes_written);
    // The trees sit behind the directory link.
    try testing.expectEqual(sb.heap.start + 65536, plan.tree_roots[0].node_addr);
    try testing.expectEqual(sb.heap.start + 2 * 65536, plan.tree_roots[1].node_addr);
    const image = try gpa.alloc(u8, len);
    defer gpa.free(image);
    @memset(image, 0xA5);
    plan.apply(image);
    // The link: header first, every pair zero.
    const hdr = try appender_dir.Header.decode(image[@intCast(sb.heap.start)..][0..4096]);
    try testing.expectEqual(@as(u32, 7), hdr.pairs);
    try testing.expect(!hdr.hasNext() and hdr.chain_index == 0);
    try testing.expect(std.mem.allEqual(u8, image[@intCast(sb.heap.start + 4096)..][0 .. 65536 - 4096], 0));
    // Appender 0's four slots: slot 1 the Free page, the rest zero.
    const offs = appender_page.appender0PageOffsets(sb.journal.start);
    const p0 = try appender_page.decode(image[@intCast(offs[1])..][0..4096]);
    try testing.expect(p0.state == .free and p0.appender_id == 0 and p0.generation == 1 and p0.identity.eql(.none));
    try testing.expectEqual(@as(u8, 1), p0.segments.len);
    try testing.expectEqual(appender_page.Extent{ .start = sb.journal.start, .len = sb.journal.len }, p0.segments.items[0]);
    for ([_]usize{ 0, 2, 3 }) |i| try testing.expect(std.mem.allEqual(u8, image[@intCast(offs[i])..][0..4096], 0));
    try testing.expectEqual(@as(?appender_page.Page, null), try appender_page.newestValid(.{ image[@intCast(offs[0])..][0..4096], null, null, null }));
    try testing.expectEqual(@as(u64, 1), (try appender_page.newestValid(.{ image[@intCast(offs[0])..][0..4096], image[@intCast(offs[1])..][0..4096], image[@intCast(offs[2])..][0..4096], image[@intCast(offs[3])..][0..4096] })).?.generation);
    // A fresh buffer: the image mounts, the bitmap claims dir + 2 nodes.
    @memset(image, 0);
    plan.apply(image);
    try expectMounts(gpa, image, len, 3);
    // Determinism holds for the class too, and the two classes differ only
    // by the plane (same set identity, same stamp).
    const again = try formatImage(gpa, len, symOpts());
    defer gpa.free(again);
    try testing.expectEqualSlices(u8, image, again);
}

test "the one-member forest plan: features 0, the control tree (empty) then the native slot tree as real leaves at base+1..2 stamped (0,0), bitmap slot A gen 1, ledger seq 1 in slot 1 with the stamp, SB tail copy then sector 0, 3 barriers; mounts" {
    const gpa = testing.allocator;
    const len = 64 * MiB;
    var opts = testOpts();
    opts.root_uid = 1000;
    opts.root_gid = 1001;
    var plan = try planFormat(gpa, len, opts, null);
    defer plan.deinit();
    try testing.expectEqual(@as(u64, 0), plan.superblock.features_incompat);
    try testing.expectEqual(@as(u64, 2), plan.nodes_written);
    try testing.expectEqual(@as(u64, 2), plan.next_ino);
    try testing.expectEqual(@as(u64, 1), plan.ledger_seq);
    const base = node_seq.nodeSeqBase(test_uuid);
    try testing.expectEqual(base + 2, plan.node_seq_watermark);
    try testing.expectEqual(@as(usize, 2), plan.tree_roots.len);
    try testing.expectEqual(ledger.RootId.control, plan.tree_roots[0].id);
    try testing.expectEqual(ledger.RootId.native_slot, plan.tree_roots[1].id);
    for (plan.tree_roots, 1..) |r, i| {
        try testing.expectEqual(base + i, r.node_seq);
        try testing.expectEqual(plan.superblock.heap.start + (i - 1) * 65536, r.node_addr);
    }
    try testing.expect(plan.superblock.appender_dir.isNone());
    // Steps: 2 zero chunks (heap.start = 1,245,184 < 2 MiB), 2 nodes, 1
    // bitmap page, 1 ledger slot (barrier), tail SB (barrier), sector 0
    // (barrier).
    const s = plan.steps;
    try testing.expectEqual(@as(usize, 2 + 2 + 1 + 1 + 2), s.len);
    try testing.expectEqual(@as(u64, 0), s[0].offset);
    try testing.expectEqual(zero_chunk, s[0].bytes.len);
    try testing.expectEqual(@as(u64, MiB), s[1].offset);
    try testing.expectEqual(@as(usize, 1_245_184 - (1 << 20)), s[1].bytes.len);
    try testing.expect(std.mem.allEqual(u8, s[1].bytes, 0));
    try testing.expectEqual(plan.superblock.heap.start, s[2].offset);
    try testing.expectEqualStrings("KVND", s[2].bytes[0..4]);
    try testing.expectEqual(@as(usize, 4096), s[2].bytes.len); // the empty control tree
    try testing.expectEqual(key.TREE_CONTROL, s[2].bytes[6]);
    try testing.expectEqual(@as(usize, 8192), s[3].bytes.len); // header + base frame (ino 1)
    try testing.expectEqual(key.KIND_INTERIOR, s[3].bytes[6]);
    try testing.expectEqualStrings("KBSF", s[3].bytes[4096..4100]);
    try testing.expectEqualStrings("BSET", s[3].bytes[4096 + bset.BSET_FRAME_LEN ..][0..4]);
    try testing.expect(std.mem.allEqual(u8, s[3].bytes[4096 + @offsetOf(bset.FrameHeaderWire, "appender_id_le") ..][0..8], 0)); // stamp (0, 0)
    try testing.expectEqual(plan.superblock.alloc_bitmap.start, s[4].offset);
    try testing.expectEqual(alloc_ext.page_magic, std.mem.readInt(u32, s[4].bytes[0..4], .little));
    try testing.expectEqual(@as(u8, 0b11), s[4].bytes[24]);
    try testing.expectEqual(plan.superblock.root_ledger.start + 4096, s[5].offset);
    try testing.expectEqual(ledger.ROOT_LEDGER_MAGIC, std.mem.readInt(u32, s[5].bytes[0..4], .little));
    try testing.expect(s[5].barrier_after);
    try testing.expectEqual(plan.superblock.backupOffset(), s[6].offset);
    try testing.expect(s[6].barrier_after);
    try testing.expectEqual(@as(u64, 0), s[7].offset);
    try testing.expect(s[7].barrier_after);
    try testing.expectEqualSlices(u8, s[6].bytes, s[7].bytes);
    try testing.expectEqual(@as(u64, 1), superblock.sectorGeneration(s[7].bytes));
    try testing.expectEqual(@as(usize, 3), plan.barrierCount());
    // Sector 0 is written exactly twice: zeroed first, stamped last.
    var sector0_writes: usize = 0;
    for (s) |st| sector0_writes += @intFromBool(st.offset == 0);
    try testing.expectEqual(@as(usize, 2), sector0_writes);
    const image = try formatImage(gpa, len, opts);
    defer gpa.free(image);
    try expectMounts(gpa, image, len, 2);
    // The native tree holds ino 1 (owner stamped) under its kind-carrying key.
    const layout = try node.NodeLayout.new(65536, null);
    var native = try node.verifyNodeExtent(gpa, image[@intCast(plan.tree_roots[1].node_addr)..][0..65536], layout, plan.tree_roots[1].node_addr, 0, null);
    defer native.deinit(gpa);
    var view = try native.bsetAt(gpa, 0);
    defer view.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), view.len());
    try testing.expectEqualSlices(u8, &key.inodeKey(1), view.record(0).key);
    const iv = try rec.InodeValue.decode(view.record(0).value);
    try testing.expectEqual(@as(u32, 1000), iv.uid);
    try testing.expectEqual(@as(u32, 1001), iv.gid);
    try testing.expectEqual(builder.S_IFDIR | 0o755, iv.mode);
}

test "the control tree carries the caller's format-time records: a set_config-shaped record lands in tree 0 as a seq-0 Put the mount-side lookup path finds; a member with none still has one real empty leaf" {
    const gpa = testing.allocator;
    const len = 64 * MiB;
    var value: [300]u8 = undefined;
    for (&value, 0..) |*b, i| b.* = @truncate(i);
    var opts = testOpts();
    opts.control_records = &.{.{ .key = "set_config", .seq = 0, .kind = .put, .value = &value }};
    var plan = try planFormat(gpa, len, opts, null);
    defer plan.deinit();
    const image = try gpa.alloc(u8, len);
    defer gpa.free(image);
    @memset(image, 0);
    plan.apply(image);
    try expectMounts(gpa, image, len, 2);
    const layout = try node.NodeLayout.new(65536, null);
    var control = try node.verifyNodeExtent(gpa, image[@intCast(plan.tree_roots[0].node_addr)..][0..65536], layout, plan.tree_roots[0].node_addr, 0, null);
    defer control.deinit(gpa);
    try testing.expectEqual(key.TREE_CONTROL, control.header.tree_id);
    var view = try control.bsetAt(gpa, 0);
    defer view.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), view.len());
    try testing.expectEqualStrings("set_config", view.record(0).key);
    try testing.expectEqualSlices(u8, &value, view.record(0).value);
    try testing.expectEqual(@as(u64, 0), view.record(0).seq);
    // The native tree is unchanged by the control record (ino 1 alone).
    var native = try node.verifyNodeExtent(gpa, image[@intCast(plan.tree_roots[1].node_addr)..][0..65536], layout, plan.tree_roots[1].node_addr, 0, null);
    defer native.deinit(gpa);
    var nv = try native.bsetAt(gpa, 0);
    defer nv.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), nv.len());
    // Without control records the control leaf is header-only (4096 B).
    var bare = try planFormat(gpa, len, testOpts(), null);
    defer bare.deinit();
    try testing.expectEqual(@as(usize, 4096), bare.steps[2].bytes.len);
    try testing.expect(plan.steps[2].bytes.len > 4096);
}

test "determinism: one description + uuid + set identity ⇒ byte-identical images; a different uuid moves every node seq and the superblock; the hash_seed is the set identity's, never derived from a uuid" {
    const gpa = testing.allocator;
    const len = 64 * MiB;
    const a = try formatImage(gpa, len, testOpts());
    defer gpa.free(a);
    const b = try formatImage(gpa, len, testOpts());
    defer gpa.free(b);
    try testing.expectEqualSlices(u8, a, b);
    var other = testOpts();
    other.uuid = "another-uuid-xx!".*;
    const c = try formatImage(gpa, len, other);
    defer gpa.free(c);
    try testing.expect(!std.mem.eql(u8, a, c));
    // Same set identity ⇒ same hash_seed and stamp: the xattr/dentry keys
    // agree; the node seqs and uuid differ.
    const sb_a = (try superblock.classifySector0(a[0..4096], null)).v5;
    const sb_c = (try superblock.classifySector0(c[0..4096], null)).v5;
    try testing.expectEqual(test_set.hash_seed, sb_a.hash_seed);
    try testing.expectEqual(sb_a.hash_seed, sb_c.hash_seed);
    try testing.expectEqualSlices(u8, &test_set.set_uuid, &sb_a.set_uuid);
    try testing.expect(!std.mem.eql(u8, &sb_a.uuid, &sb_c.uuid));
    // A member of a larger set stamps its stride run and native slot.
    var member = testOpts();
    member.member_position = 2;
    member.member_count = 4;
    const m = try formatImage(gpa, len, member);
    defer gpa.free(m);
    var record = (try ledger.newestValid(gpa, m[@intCast(sb_a.root_ledger.start)..][0..@intCast(sb_a.root_ledger.len)])) orelse return error.TestUnexpectedResult;
    defer record.deinit(gpa);
    try testing.expectEqual(@as(u16, 2), record.membership_stamp.member_position);
    try testing.expectEqual(@as(u16, 4), record.membership_stamp.member_count);
    // A member not hosting slot 0 is guest-only: no native keyspace.
    try testing.expectEqual(@as(?u16, null), record.membership_stamp.native_slot);
    try testing.expectEqual(ledger.SlotRun{ .start = 2, .stride = 4, .count = 16_384 }, record.membership_stamp.slots_hosted.runs[0]);
}

test "a populated description packs a multi-level native tree; the ledger and bitmap agree with the writer; free == total − nodes; the geometry floor refuses with the exact message" {
    const gpa = testing.allocator;
    const len = 64 * MiB;
    const opts = testOpts();
    var desc = try builder.ImageBuilder.init(gpa, .{ .node_size = 65536, .journal_len_override = MiB, .hash_seed = test_set.hash_seed, .uuid = test_uuid });
    defer desc.deinit();
    var name_buf: [32]u8 = undefined;
    const d = try desc.addDir(builder.ROOT_INO, "d", 0o755, 0, 0);
    for (0..3000) |i| _ = try desc.addFile(d, try std.fmt.bufPrint(&name_buf, "f{d}", .{i}), 0o644, 0, 0, i);
    var plan = try planFormatWith(gpa, len, opts, &desc, null);
    defer plan.deinit();
    // 3,002 inodes × (16 + 9 + 68) = 279,186 B + 3,001 dentries × (16 + 17
    // + 10 + ~5) ≈ 144 KB over a 46,026 budget ⇒ ≥ 9 leaves + 1 root in the
    // native tree, plus the empty control tree.
    try testing.expectEqual(@as(u64, 3003), plan.next_ino);
    try testing.expect(plan.nodes_written >= 1 + 9 + 1);
    const full = try gpa.alloc(u8, len);
    defer gpa.free(full);
    @memset(full, 0);
    plan.apply(full);
    try expectMounts(gpa, full, len, plan.nodes_written);
    // The native root is interior and routes to every leaf; the leaves hold
    // every record of the description, ino-major.
    const layout = try node.NodeLayout.new(65536, null);
    var root = try node.verifyNodeExtent(gpa, full[@intCast(plan.tree_roots[1].node_addr)..][0..65536], layout, plan.tree_roots[1].node_addr, 0, null);
    defer root.deinit(gpa);
    try testing.expectEqual(@as(u8, 1), root.header.level);
    var seps = try root.bsetAt(gpa, 0);
    defer seps.deinit(gpa);
    try testing.expect(seps.len() >= 9);
    var count: usize = 0;
    var inodes: usize = 0;
    for (0..seps.len()) |i| {
        const iv = try key.InteriorValue.decode(seps.record(i).value);
        var leaf = try node.verifyNodeExtent(gpa, full[@intCast(iv.childAddr())..][0..65536], layout, iv.childAddr(), 0, null);
        defer leaf.deinit(gpa);
        try testing.expectEqual(iv.childSeq(), leaf.header.node_seq);
        try testing.expectEqualSlices(u8, seps.record(i).key, leaf.header.max_key);
        var lv = try leaf.bsetAt(gpa, 0);
        defer lv.deinit(gpa);
        for (0..lv.len()) |j| {
            if (try key.slotTreeKeyKind(lv.record(j).key) == key.TREE_INODES) inodes += 1;
        }
        count += lv.len();
    }
    try testing.expectEqual(@as(usize, 3002 + 3001), count);
    try testing.expectEqual(@as(usize, 3002), inodes);
    // The floor.
    var diag: geometry.PlanRefusal = undefined;
    try testing.expectError(error.VolumeTooSmall, planFormat(gpa, 4 * MiB, .{ .node_size = 65536, .uuid = test_uuid, .set = test_set }, &diag));
    try testing.expect(diag == .volume_too_small);
}
