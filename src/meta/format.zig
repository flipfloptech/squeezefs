//! `squeezefs format`'s pure half (charter §5.1; `docs/porting/kv_alloc_superblock.md`
//! §2.2–§2.3, §9, §12): the complete, valid, checkpointed v3 image of one
//! metadata volume as an ordered list of `(offset, bytes, barrier_after)`
//! steps in the reference builder's write/barrier order, plus `formatImage`,
//! which applies them to an in-RAM volume. The io binding registers the
//! device (O_DIRECT from the first byte) and executes the steps; nothing
//! here touches a device or randomness (`uuid` and `set_uuid` are inputs).
//!
//! The order is `geometry.buildPlan`'s: zero `[0, heap.start)` (quick-format
//! hygiene — the heap is NOT wiped; dead frames fail the uuid-namespaced seq
//! chain) → every tree root as a REAL leaf (an empty tree is still one
//! header-only node) → the forest's directory extent, `KVAD` header and
//! appender-0 `Free` page → every bitmap page at generation 1 → the
//! bootstrap ledger record (seq 1 ⇒ slot 1) → barrier → the superblock's
//! tail copy, barrier, sector 0, barrier. Sector 0 is zeroed first and
//! rewritten LAST, so no crash prefix is mountable (it reads Blank).

const std = @import("std");
const key = @import("key.zig");
const rec = @import("record.zig");
const node = @import("node.zig");
const node_seq = @import("node_seq.zig");
const alloc_core = @import("alloc_core.zig");
const alloc_ext = @import("alloc_ext.zig");
const superblock = @import("superblock.zig");
const geometry = @import("geometry.zig");
const ledger = @import("ledger.zig");
const builder = @import("builder.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
const SuperblockV3 = superblock.SuperblockV3;
const ExtentRef = superblock.ExtentRef;

/// The reference's wipe chunk.
pub const zero_chunk: usize = 1 << 20;

/// `FormatV3Options` + the identities `format_v3_inner` mints (random in
/// production; fixed for deterministic images).
pub const FormatOptions = struct {
    node_size: u64 = superblock.default_node_size,
    /// `--meta-journal-mb` in bytes; null = the clamp.
    journal_len_override: ?u64 = null,
    class: geometry.FormatClass = .symmetric,
    seams: geometry.Seams = .{},
    /// Per-volume identity: the node-seq namespace and the superblock's.
    uuid: [16]u8,
    /// The set's identity: the membership stamp's `set_uuid`; the set-wide
    /// `hash_seed` is `xxh3(set_uuid)`.
    set_uuid: [16]u8,
    /// This member's place in the set (`plan_meta_slot_set(n).stamps[pos]`).
    member_position: u16 = 0,
    member_count: u16 = 1,
    /// The root directory's owner: the invoking user (`SUDO_UID:SUDO_GID`
    /// under sudo); the builder default is 0:0.
    root_uid: u32 = 0,
    root_gid: u32 = 0,
    /// `user.squeezefs.format_config` on ino 1 (the mount bootstrap config).
    format_config_xattr: ?[]const u8 = null,
    /// `nextSuperblockGeneration(primary, tail)` as read BEFORE the wipe:
    /// 1 on a blank volume; a reformat over a valid tail copy continues
    /// its generation so DUR-5's newest-valid-wins never picks the stale
    /// copy over the fresh primary.
    sb_generation: u64 = 1,
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
    superblock: SuperblockV3,
    /// The effective class (the FORMAT_FLAT seam applied).
    class: geometry.FormatClass,
    features_incompat: u64,
    ledger_seq: u64,
    next_ino: u64,
    /// Btree nodes written (all trees, all levels).
    nodes_written: u64,
    /// Heap extents claimed: the nodes plus the forest's directory extent.
    extents_allocated: u64,
    node_seq_watermark: u64,
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

/// `format_v3_inner`: the root-only description (owner stamped, the config
/// xattr if given), the one-member membership stamp (or the caller's
/// member), the set-wide hash seed — then `build`.
pub fn planFormat(gpa: Allocator, volume_len: u64, opts: FormatOptions, diag: ?*geometry.PlanRefusal) FormatError!FormatPlan {
    var desc = try builder.ImageBuilder.init(gpa, .{
        .node_size = @intCast(opts.node_size),
        .journal_len_override = opts.journal_len_override,
        .hash_seed = geometry.hashSeed(opts.set_uuid),
        .uuid = opts.uuid,
    });
    defer desc.deinit();
    try desc.setOwner(builder.ROOT_INO, opts.root_uid, opts.root_gid);
    if (opts.format_config_xattr) |cfg| try desc.setXattr(builder.ROOT_INO, builder.FORMAT_CONFIG_XATTR, cfg);
    return planFormatWith(gpa, volume_len, opts, &desc, diag);
}

/// `ImageBuilder::build` over an arbitrary description (tests build fixture
/// volumes through it).
pub fn planFormatWith(gpa: Allocator, volume_len: u64, opts: FormatOptions, desc: *const builder.ImageBuilder, diag: ?*geometry.PlanRefusal) FormatError!FormatPlan {
    var arena = std.heap.ArenaAllocator.init(gpa);
    errdefer arena.deinit();
    const a = arena.allocator();

    // 2. Plan + the class's feature word.
    var sb = try geometry.plan(volume_len, opts.node_size, opts.journal_len_override, opts.uuid, desc.cfg.hash_seed, diag);
    const class = geometry.effectiveClass(opts.class, opts.seams);
    const word = geometry.featureWord(class, opts.seams);
    sb.features_incompat = word;
    const forest = class == .symmetric or class == .symmetric_explicit;
    const build_plan = geometry.buildPlan(sb, class, word);

    // 4. The allocator over the planned heap; 5. the layout (v2 frames
    // stamped (0, 0) on a forest); 6. the writer from the uuid base.
    const total = sb.totalExtents();
    var alloc = try alloc_ext.ExtentAllocator.format(gpa, total, alloc_core.compactionReserveExtents(total), alloc_ext.builder_pending_cap);
    defer alloc.deinit();
    const node_size: usize = @intCast(opts.node_size);
    const layout = if (forest) try node.NodeLayout.newSymmetric(node_size, null) else try node.NodeLayout.new(node_size, null);
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
            const records: []const rec.Record = switch (t.records) {
                .control_empty, .block_refs_empty => &.{},
                .native => try desc.nativeRecords(a),
                .inodes => try desc.inodeRecords(a),
                .dentries => try desc.dentryRecords(a),
                .xattrs => try desc.xattrRecords(a),
            };
            const refs = try a.alloc(rec.RecordRef, records.len);
            for (records, refs) |*r, *o| o.* = r.ref();
            const first_image = images.items.len;
            const root = try writer.writeTree(t.tree_id, refs);
            for (images.items[first_image..]) |*w| try steps.append(a, .{ .offset = w.node_addr, .bytes = w.image, .barrier_after = false });
            try roots.append(a, .{ .tree_id = t.tree_id, .node_addr = root.addr, .node_seq = root.seq });
        },
        .claim_dir_extent => {
            // The NEXT lowest free extent after the nodes; zeroed (quick
            // format zeroes only the fixed region).
            const extent = try alloc.claimInternal();
            sb.appender_dir = .{ .start = sb.heap.start + extent * opts.node_size, .len = opts.node_size };
            try pushZero(a, &steps, zeros, sb.appender_dir.start, sb.appender_dir.len);
        },
        .write_dir_header => {
            const page = try a.create([geometry.appender_page_len]u8);
            geometry.dirHeaderPage(opts.node_size, page);
            try steps.append(a, .{ .offset = geometry.dirHeaderOffset(sb.appender_dir), .bytes = page, .barrier_after = false });
        },
        .write_appender_page => |p| {
            const page = try a.create([geometry.appender_page_len]u8);
            geometry.appender0FreePage(sb.journal, page);
            try steps.append(a, .{ .offset = p.offset, .bytes = page, .barrier_after = false });
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
            const stamp = try ledgerStamp(a, geometry.MembershipStamp.member(opts.set_uuid, opts.member_position, opts.member_count));
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
            const plan = try superblock.planWrite(sb, volume_len, opts.sb_generation, null);
            const image = try a.dupe(u8, &plan.image);
            for (plan.slice()) |w| try steps.append(a, .{ .offset = w.offset, .bytes = image, .barrier_after = w.barrier_after });
        },
    };

    const extents_allocated = writer.nodesWritten() + @intFromBool(forest);
    assert(alloc.freeExtents() == total - extents_allocated);
    return .{
        .arena = arena,
        .steps = try steps.toOwnedSlice(a),
        .superblock = sb,
        .class = class,
        .features_incompat = word,
        .ledger_seq = geometry.bootstrap_ledger_seq,
        .next_ino = desc.nextIno(),
        .nodes_written = writer.nodesWritten(),
        .extents_allocated = extents_allocated,
        .node_seq_watermark = writer.nodeSeqWatermark(),
        .tree_roots = try roots.toOwnedSlice(a),
    };
}

/// `zero_range` in ≤ 1 MiB chunks, every chunk a view of ONE zero buffer.
fn pushZero(a: Allocator, steps: *std.ArrayList(Step), zeros: []const u8, start: u64, len: u64) Allocator.Error!void {
    var off = start;
    const end = start + len;
    while (off < end) {
        const n: usize = @intCast(@min(end - off, zeros.len));
        try steps.append(a, .{ .offset = off, .bytes = zeros[0..n], .barrier_after = false });
        off += n;
    }
}

/// The geometry module's one-run stamp as the ledger codec's shape.
fn ledgerStamp(a: Allocator, g: geometry.MembershipStamp) ledger.Error!ledger.MembershipStamp {
    const runs = try a.alloc(ledger.SlotRun, 1);
    runs[0] = .{ .start = g.slots_hosted.start, .stride = g.slots_hosted.stride, .count = g.slots_hosted.count };
    return .{
        .set_uuid = g.set_uuid,
        .set_epoch = g.set_epoch,
        .member_position = g.member_position,
        .member_count = g.member_count,
        .routing_width = g.routing_width,
        .slots_hosted = try ledger.SlotSet.fromRuns(runs),
        .native_slot = g.native_slot,
        .slot_cursors = &.{},
    };
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
const test_set_uuid: [16]u8 = "set-uuid-fixture".*;

fn testOpts(class: geometry.FormatClass) FormatOptions {
    return .{ .node_size = 65536, .journal_len_override = MiB, .class = class, .uuid = test_uuid, .set_uuid = test_set_uuid };
}

/// Mount-side wiring over an image: SB → newest ledger → every root loads.
fn expectMounts(gpa: Allocator, image: []const u8, volume_len: u64, forest: bool, want_roots: []const u8, extents_allocated: u64) !void {
    const fmt = try superblock.classifyVolumeImages(image[0..4096], image[@intCast(superblock.backupOffset(volume_len).?)..][0..4096], volume_len, null);
    try testing.expectEqual(superblock.SuperblockSlot.primary, fmt.slot);
    const sb = fmt.format.v3;
    try testing.expectEqual(forest, sb.symmetricForestStamped());
    const layout = if (forest) try node.NodeLayout.newSymmetric(sb.node_size, null) else try node.NodeLayout.new(sb.node_size, null);
    var record = (try ledger.newestValid(gpa, image[@intCast(sb.root_ledger.start)..][0..@intCast(sb.root_ledger.len)])) orelse return error.TestUnexpectedResult;
    defer record.deinit(gpa);
    try testing.expectEqual(@as(u64, 1), record.seq);
    try testing.expectEqual(want_roots.len, record.tree_roots.len);
    for (record.tree_roots, want_roots) |root, tree_id| {
        try testing.expectEqual(tree_id, root.tree_id);
        var n = try node.verifyNodeExtent(gpa, image[@intCast(root.node_addr)..][0..sb.node_size], layout, root.node_addr, record.journal_tail_seq, null);
        defer n.deinit(gpa);
        try testing.expectEqual(root.node_seq, n.header.node_seq);
        try testing.expectEqual(@as(u64, 0), n.dropped_tail_bsets);
    }
    // The bitmap: the newest page claims exactly the ledger's extents (+ dir).
    var loaded = try alloc_ext.ExtentAllocator.load(gpa, image[@intCast(sb.alloc_bitmap.start)..][0..@intCast(sb.alloc_bitmap.len)], sb.totalExtents(), alloc_core.compactionReserveExtents(sb.totalExtents()), 4, 0, &.{}, null);
    defer loaded.deinit();
    try testing.expectEqual(@as(u64, 1), loaded.resumeGeneration());
    try testing.expectEqual(sb.totalExtents() - extents_allocated, loaded.freeExtents());
    for (record.tree_roots) |root| try testing.expect(loaded.isAllocated((root.node_addr - sb.heap.start) / sb.node_size));
    if (forest) try testing.expect(loaded.isAllocated((sb.appender_dir.start - sb.heap.start) / sb.node_size));
    // The journal ring is all zero: nothing to replay.
    try testing.expect(std.mem.allEqual(u8, image[@intCast(sb.journal.start + 4 * 4096)..][0..@intCast(sb.journal.len - 4 * 4096)], 0));
}

test "flat (--single-writer) plan: word 0xD7, trees 1/2/3 as real leaves at base+1..3, bitmap slot A gen 1, ledger seq 1 in slot 1 with the stamp, SB tail copy then sector 0, 3 barriers; mounts" {
    const gpa = testing.allocator;
    const len = 64 * MiB;
    var plan = try planFormat(gpa, len, testOpts(.single_writer), null);
    defer plan.deinit();
    try testing.expectEqual(geometry.FormatClass.single_writer, plan.class);
    try testing.expectEqual(geometry.single_writer_word, plan.features_incompat);
    try testing.expectEqual(@as(u64, 3), plan.nodes_written);
    try testing.expectEqual(@as(u64, 3), plan.extents_allocated);
    try testing.expectEqual(@as(u64, 2), plan.next_ino);
    try testing.expectEqual(@as(u64, 1), plan.ledger_seq);
    const base = node_seq.nodeSeqBase(test_uuid);
    try testing.expectEqual(base + 3, plan.node_seq_watermark);
    try testing.expectEqual(@as(usize, 3), plan.tree_roots.len);
    for (plan.tree_roots, 1..) |r, i| {
        try testing.expectEqual(@as(u8, @intCast(i)), r.tree_id);
        try testing.expectEqual(base + i, r.node_seq);
        try testing.expectEqual(plan.superblock.heap.start + (i - 1) * 65536, r.node_addr);
    }
    try testing.expect(plan.superblock.appender_dir.isNone());
    // Steps: 2 zero chunks (heap.start = 1,245,184 < 2 MiB), 3 nodes, 1
    // bitmap page, 1 ledger slot (barrier), tail SB (barrier), sector 0
    // (barrier).
    const s = plan.steps;
    try testing.expectEqual(@as(usize, 2 + 3 + 1 + 1 + 2), s.len);
    try testing.expectEqual(@as(u64, 0), s[0].offset);
    try testing.expectEqual(zero_chunk, s[0].bytes.len);
    try testing.expectEqual(@as(u64, MiB), s[1].offset);
    try testing.expectEqual(@as(usize, 1_245_184 - (1 << 20)), s[1].bytes.len);
    try testing.expect(std.mem.allEqual(u8, s[1].bytes, 0));
    try testing.expectEqual(plan.superblock.heap.start, s[2].offset);
    try testing.expectEqualStrings("KVND", s[2].bytes[0..4]);
    try testing.expectEqual(@as(usize, 8192), s[2].bytes.len); // header + base frame (ino 1)
    try testing.expectEqual(@as(usize, 4096), s[3].bytes.len); // empty dentry tree
    try testing.expectEqual(@as(usize, 4096), s[4].bytes.len); // empty xattr tree (no config)
    try testing.expectEqual(plan.superblock.alloc_bitmap.start, s[5].offset);
    try testing.expectEqual(alloc_ext.page_magic, std.mem.readInt(u32, s[5].bytes[0..4], .little));
    try testing.expectEqual(@as(u8, 0b111), s[5].bytes[24]);
    try testing.expectEqual(plan.superblock.root_ledger.start + 4096, s[6].offset);
    try testing.expectEqual(ledger.ROOT_LEDGER_MAGIC, std.mem.readInt(u32, s[6].bytes[0..4], .little));
    try testing.expect(s[6].barrier_after);
    try testing.expectEqual(superblock.backupSlotFor(plan.superblock, len).?, s[7].offset);
    try testing.expect(s[7].barrier_after);
    try testing.expectEqual(@as(u64, 0), s[8].offset);
    try testing.expect(s[8].barrier_after);
    try testing.expectEqualSlices(u8, s[7].bytes, s[8].bytes);
    try testing.expectEqual(@as(u64, 1), superblock.sectorGeneration(s[8].bytes));
    try testing.expectEqual(@as(usize, 3), plan.barrierCount());
    // Sector 0 is written exactly twice: zeroed first, stamped last.
    var sector0_writes: usize = 0;
    for (s) |st| sector0_writes += @intFromBool(st.offset == 0);
    try testing.expectEqual(@as(usize, 2), sector0_writes);
    const image = try formatImage(gpa, len, testOpts(.single_writer));
    defer gpa.free(image);
    try expectMounts(gpa, image, len, false, &.{ 1, 2, 3 }, 3);
}

test "forest (default) plan: word 0x2FFD7, tree 8 (empty) then tree 0 (native) with v2 (0,0) frames, the dir extent claimed next and zeroed, KVAD header, appender page at journal.start + 4096, sb.appender_dir set; mounts" {
    const gpa = testing.allocator;
    const len = 64 * MiB;
    var opts = testOpts(.symmetric);
    opts.format_config_xattr = "{\"name\":\"squeezefs\"}";
    opts.root_uid = 1000;
    opts.root_gid = 1001;
    var plan = try planFormat(gpa, len, opts, null);
    defer plan.deinit();
    try testing.expectEqual(geometry.default_format_word, plan.features_incompat);
    try testing.expectEqual(@as(u64, 2), plan.nodes_written);
    try testing.expectEqual(@as(u64, 3), plan.extents_allocated);
    const sb = plan.superblock;
    try testing.expectEqual(ExtentRef{ .start = sb.heap.start + 2 * 65536, .len = 65536 }, sb.appender_dir);
    try testing.expectEqual(@as(u8, 8), plan.tree_roots[0].tree_id);
    try testing.expectEqual(@as(u8, 0), plan.tree_roots[1].tree_id);
    const s = plan.steps;
    // 2 zero chunks, 2 nodes, dir zero, KVAD, appender page, bitmap, ledger,
    // SB × 2.
    try testing.expectEqual(@as(usize, 2 + 2 + 1 + 1 + 1 + 1 + 1 + 2), s.len);
    try testing.expectEqual(@as(usize, 4096), s[2].bytes.len); // the empty control tree
    try testing.expectEqual(@as(u8, 8), s[2].bytes[6]);
    try testing.expectEqual(@as(usize, 8192), s[3].bytes.len);
    try testing.expectEqual(@as(u8, 0), s[3].bytes[6]);
    try testing.expectEqualStrings("KBSF", s[3].bytes[4096..4100]);
    try testing.expectEqual(@as(u16, 2), std.mem.readInt(u16, s[3].bytes[4100..4102], .little));
    try testing.expect(std.mem.allEqual(u8, s[3].bytes[4096 + 24 .. 4096 + 32], 0)); // stamp (0, 0)
    try testing.expectEqual(sb.appender_dir.start, s[4].offset);
    try testing.expectEqual(@as(usize, 65536), s[4].bytes.len);
    try testing.expect(std.mem.allEqual(u8, s[4].bytes, 0));
    try testing.expectEqual(geometry.dirHeaderOffset(sb.appender_dir), s[5].offset);
    try testing.expectEqual(geometry.appender_dir_magic, std.mem.readInt(u32, s[5].bytes[0..4], .little));
    try testing.expectEqual(sb.journal.start + 4096, s[6].offset);
    try testing.expectEqual(geometry.appender_page_magic, std.mem.readInt(u32, s[6].bytes[0..4], .little));
    try testing.expectEqual(@as(u8, 0b111), s[7].bytes[24]);
    try testing.expect(s[8].barrier_after);
    const image = try formatImage(gpa, len, opts);
    defer gpa.free(image);
    try expectMounts(gpa, image, len, true, &.{ 8, 0 }, 3);
    // The native tree holds ino 1 (owner stamped) and the config xattr under
    // forest keys.
    const layout = try node.NodeLayout.newSymmetric(65536, null);
    var native = try node.verifyNodeExtent(gpa, image[@intCast(plan.tree_roots[1].node_addr)..][0..65536], layout, plan.tree_roots[1].node_addr, 0, null);
    defer native.deinit(gpa);
    var view = try native.bsetAt(gpa, 0);
    defer view.deinit(gpa);
    try testing.expectEqual(@as(usize, 2), view.len());
    const iv = try rec.InodeValue.decode(view.record(0).value);
    try testing.expectEqual(@as(u32, 1000), iv.uid);
    try testing.expectEqual(@as(u32, 1001), iv.gid);
    try testing.expectEqual(builder.S_IFDIR | 0o755, iv.mode);
    const xv = try rec.XattrValue.decode(view.record(1).value);
    try testing.expectEqualStrings(builder.FORMAT_CONFIG_XATTR, xv.name);
    try testing.expectEqualStrings("{\"name\":\"squeezefs\"}", xv.value);
    try testing.expectEqual(key.TREE_XATTRS, try key.forestKeyKind(view.record(1).key));
    // The explicit form builds the same image; FORMAT_FLAT turns only the
    // default class flat.
    var explicit = opts;
    explicit.class = .symmetric_explicit;
    explicit.seams.format_flat = true;
    const image2 = try formatImage(gpa, len, explicit);
    defer gpa.free(image2);
    try testing.expectEqualSlices(u8, image, image2);
    var flat_seam = opts;
    flat_seam.seams.format_flat = true;
    var flat_plan = try planFormat(gpa, len, flat_seam, null);
    defer flat_plan.deinit();
    try testing.expectEqual(geometry.FormatClass.single_writer, flat_plan.class);
    try testing.expectEqual(@as(u64, 3), flat_plan.nodes_written);
}

test "determinism: one description + uuid + set_uuid ⇒ byte-identical images; a different uuid moves every node seq and the superblock; the pre-flip class and the bit-9 seam plant tree 6" {
    const gpa = testing.allocator;
    const len = 64 * MiB;
    const a = try formatImage(gpa, len, testOpts(.symmetric));
    defer gpa.free(a);
    const b = try formatImage(gpa, len, testOpts(.symmetric));
    defer gpa.free(b);
    try testing.expectEqualSlices(u8, a, b);
    var other = testOpts(.symmetric);
    other.uuid = "another-uuid-xx!".*;
    const c = try formatImage(gpa, len, other);
    defer gpa.free(c);
    try testing.expect(!std.mem.eql(u8, a, c));
    // Same set uuid ⇒ same hash_seed and stamp: the xattr/dentry keys agree;
    // the node seqs and uuid differ.
    const sb_a = (try superblock.classifySector0(a[0..4096], null)).v3;
    const sb_c = (try superblock.classifySector0(c[0..4096], null)).v3;
    try testing.expectEqual(sb_a.hash_seed, sb_c.hash_seed);
    try testing.expect(!std.mem.eql(u8, &sb_a.uuid, &sb_c.uuid));
    var mwf = try planFormat(gpa, len, testOpts(.multi_writer_flat), null);
    defer mwf.deinit();
    try testing.expectEqual(geometry.multi_writer_flat_word, mwf.features_incompat);
    try testing.expectEqual(@as(usize, 4), mwf.tree_roots.len);
    try testing.expectEqual(@as(u8, 6), mwf.tree_roots[3].tree_id);
    try testing.expect(mwf.superblock.appender_dir.isNone());
    var seamed = testOpts(.single_writer);
    seamed.seams.stamp_block_refs = true;
    var refs = try planFormat(gpa, len, seamed, null);
    defer refs.deinit();
    try testing.expectEqual(geometry.single_writer_word | superblock.kv_block_refcounts, refs.features_incompat);
    try testing.expectEqual(@as(u64, 4), refs.nodes_written);
    const refs_image = try formatImage(gpa, len, seamed);
    defer gpa.free(refs_image);
    try expectMounts(gpa, refs_image, len, false, &.{ 1, 2, 3, 6 }, 4);
}

/// Cross-check 2: with the identity the reference minted (uuid, set_uuid,
/// root owner, config xattr, superblock generation) pinned as inputs, the
/// Zig image of the same length and class is BYTE-IDENTICAL to the
/// reference's — every node, frame, bitmap page, ledger slot and both
/// superblock copies.
fn crossCheckReferenceFormat(gpa: Allocator, path: []const u8, class: geometry.FormatClass) !bool {
    var vol = (try node.ReferenceVolume.open(testing.io, path)) orelse return false;
    defer vol.close();
    const volume_len = try vol.length();
    const sector0 = try vol.readAt(gpa, 0, superblock.sector_len);
    defer gpa.free(sector0);
    const sb = (try superblock.classifySector0(sector0, null)).v3;
    const ledger_img = try vol.readAt(gpa, sb.root_ledger.start, @intCast(sb.root_ledger.len));
    defer gpa.free(ledger_img);
    var record = (try ledger.newestValid(gpa, ledger_img)) orelse return error.TestUnexpectedResult;
    defer record.deinit(gpa);
    const stamp = record.membership_stamp orelse return error.TestUnexpectedResult;
    const layout = if (sb.symmetricForestStamped()) try node.NodeLayout.newSymmetric(sb.node_size, null) else try node.NodeLayout.new(sb.node_size, null);
    var config: ?[]u8 = null;
    defer if (config) |c| gpa.free(c);
    var owner: [2]u32 = .{ 0, 0 };
    for (record.tree_roots) |root| {
        const extent = try vol.readAt(gpa, root.node_addr, sb.node_size);
        defer gpa.free(extent);
        var n = try node.verifyNodeExtent(gpa, extent, layout, root.node_addr, 0, null);
        defer n.deinit(gpa);
        if (n.bsetCount() == 0) continue;
        var view = try n.bsetAt(gpa, 0);
        defer view.deinit(gpa);
        for (0..view.len()) |i| {
            const r = view.record(i);
            const kind: u8 = if (sb.symmetricForestStamped()) try key.forestKeyKind(r.key) else root.tree_id;
            if (kind == key.TREE_INODES) {
                const iv = try rec.InodeValue.decode(r.value);
                owner = .{ iv.uid, iv.gid };
            } else if (kind == key.TREE_XATTRS) {
                const x = try rec.XattrValue.decode(r.value);
                if (std.mem.eql(u8, x.name, builder.FORMAT_CONFIG_XATTR)) config = try gpa.dupe(u8, x.value);
            }
        }
    }
    const image = try formatImage(gpa, volume_len, .{
        .node_size = sb.node_size,
        .journal_len_override = if (sb.journal.len == geometry.journalRingLen(volume_len)) null else sb.journal.len,
        .class = class,
        .uuid = sb.uuid,
        .set_uuid = stamp.set_uuid,
        .member_position = stamp.member_position,
        .member_count = stamp.member_count,
        .root_uid = owner[0],
        .root_gid = owner[1],
        .format_config_xattr = config,
        .sb_generation = superblock.sectorGeneration(sector0),
    });
    defer gpa.free(image);
    // Stream the reference in 1 MiB chunks against the image.
    var off: u64 = 0;
    while (off < volume_len) : (off += zero_chunk) {
        const n: usize = @intCast(@min(zero_chunk, volume_len - off));
        const chunk = try vol.readAt(gpa, off, n);
        defer gpa.free(chunk);
        try testing.expectEqualSlices(u8, chunk, image[@intCast(off)..][0..n]);
    }
    return true;
}

test "cross-check 2: the Zig `--single-writer` image with the reference's identity pinned is byte-identical to /build/tmp/ref-sw.img (skipped when absent)" {
    if (!try crossCheckReferenceFormat(testing.allocator, "/build/tmp/ref-sw.img", .single_writer)) return error.SkipZigTest;
}

test "cross-check 2: the Zig default (symmetric forest) image with the reference's identity pinned is byte-identical to /build/tmp/ref-sym.img (skipped when absent)" {
    if (!try crossCheckReferenceFormat(testing.allocator, "/build/tmp/ref-sym.img", .symmetric)) return error.SkipZigTest;
}

test "a populated description packs multi-level trees; the ledger and bitmap agree with the writer; free == total − allocated; the geometry floor refuses with the exact message" {
    const gpa = testing.allocator;
    const len = 64 * MiB;
    const opts = testOpts(.single_writer);
    var desc = try builder.ImageBuilder.init(gpa, .{ .node_size = 65536, .journal_len_override = MiB, .hash_seed = geometry.hashSeed(test_set_uuid), .uuid = test_uuid });
    defer desc.deinit();
    var name_buf: [32]u8 = undefined;
    const d = try desc.addDir(builder.ROOT_INO, "d", 0o755, 0, 0);
    for (0..3000) |i| _ = try desc.addFile(d, try std.fmt.bufPrint(&name_buf, "f{d}", .{i}), 0o644, 0, 0, i);
    var plan = try planFormatWith(gpa, len, opts, &desc, null);
    defer plan.deinit();
    // 3,002 inodes (root, d, 3,000 files) × (15 + 8 + 57) = 240,160 B over
    // a 46,032 budget ⇒ 6 leaves + 1 root; 3,001 dentries ≈ 3,001 × (15 +
    // 16 + 10 + ~5) ≈ 138 KB ⇒ 3–4 leaves + root; xattrs: 1 empty leaf.
    try testing.expectEqual(@as(u64, 3003), plan.next_ino);
    try testing.expect(plan.nodes_written >= 7 + 4 + 1);
    try testing.expectEqual(plan.nodes_written, plan.extents_allocated);
    const full = try gpa.alloc(u8, len);
    defer gpa.free(full);
    @memset(full, 0);
    plan.apply(full);
    try expectMounts(gpa, full, len, false, &.{ 1, 2, 3 }, plan.extents_allocated);
    // The inode root is interior and routes to every leaf.
    const layout = try node.NodeLayout.new(65536, null);
    var root = try node.verifyNodeExtent(gpa, full[@intCast(plan.tree_roots[0].node_addr)..][0..65536], layout, plan.tree_roots[0].node_addr, 0, null);
    defer root.deinit(gpa);
    try testing.expectEqual(@as(u8, 1), root.header.level);
    var seps = try root.bsetAt(gpa, 0);
    defer seps.deinit(gpa);
    try testing.expect(seps.len() >= 5);
    var count: usize = 0;
    for (0..seps.len()) |i| {
        const iv = try key.InteriorValue.decode(seps.record(i).value);
        var leaf = try node.verifyNodeExtent(gpa, full[@intCast(iv.childAddr())..][0..65536], layout, iv.childAddr(), 0, null);
        defer leaf.deinit(gpa);
        try testing.expectEqual(iv.childSeq(), leaf.header.node_seq);
        try testing.expectEqualSlices(u8, seps.record(i).key, leaf.header.max_key);
        var lv = try leaf.bsetAt(gpa, 0);
        defer lv.deinit(gpa);
        count += lv.len();
    }
    try testing.expectEqual(@as(usize, 3002), count);
    // The floor.
    var diag: geometry.PlanRefusal = undefined;
    try testing.expectError(error.VolumeTooSmall, planFormat(gpa, 4 * MiB, .{ .node_size = 65536, .uuid = test_uuid, .set_uuid = test_set_uuid }, &diag));
    try testing.expect(diag == .volume_too_small);
}
