//! `squeezefs volume enable-symmetric`'s pure half (charter §5.4, §5.5
//! "`--single-writer` is the one flat opt-out"; §8; `docs/evidence/
//! rung-08-operations.md` "Deviations": the exact recipe): a `--single-
//! writer` volume becomes a symmetric one in place. Every v5 volume is
//! already a one-member forest (decision 0003 Q2), so no tree moves; what
//! the flat class lacks is the APPENDER REGION — the directory link the
//! superblock names and appender 0's four page slots at the fixed ring's
//! first 16 KiB — and the `SYMMETRIC_PLANE` bit that means both.
//!
//! The plan over a volume's images (the superblock, the newest ledger
//! record, the ring, the bitmap), in the write/barrier order the device
//! binding executes (`format.Step`s, like `format.planFormat`):
//!
//! 1. **the link** — a free heap extent (the format claims extent 0 before
//!    any tree; a flat volume's extent 0 holds a tree, so the link lives at
//!    the lowest free extent the bitmap offers), zeroed, its `KVAD` header
//!    page FIRST;
//! 2. **the ring re-based** — the fixed journal extent zeroed end to end
//!    and appender 0's `Free` page written at slot 1 (`pageSlotFor(1)`),
//!    exactly the format's image; the ring proper is the extent behind the
//!    four slots from now on. Legal only on an EMPTY replay window (the
//!    ledger's tail = the chain's head: a clean unmount), when every byte
//!    of the ring is dead — the record-seq space continues from the tail
//!    unchanged (positions are logical; the re-based geometry maps them to
//!    other pages, which is why the ring must be zeroed, never kept);
//!    → barrier;
//! 3. **the claim** — the bitmap page(s) naming the link at the next
//!    generation + the ledger record RESTATED at that seq naming the
//!    generation (the dense-ledger law: a checkpoint-class step outside a
//!    cycle consumes a seq and says so) → barrier;
//! 4. **the stamp** — `SYMMETRIC_PLANE` + `appender_dir` in ONE sector
//!    image through `superblock.planWrite`: the tail copy, barrier, sector
//!    0, barrier. The mount of the stamped volume is the ordinary
//!    symmetric one (`bind.AppenderSet.open`: appender 0's page is read
//!    newest-valid over the four slots; a Free page joins as the manager).
//!
//! Crash windows: before step 3 lands the volume is FLAT and nothing
//! references the link (garbage in a free extent, a zeroed ring the flat
//! mount reads as empty); between 3 and 4 it is FLAT with a CLAIMED extent
//! no root reaches — the re-run finds it by CONTENT (`findStrandedLink`:
//! a claimed extent whose first page is a verifying first-link header;
//! a flat volume has no directory, so any such page is a crashed run's)
//! and resumes onto it instead of leaking it. After 4 it is symmetric.
//! Refusals are typed: an already-symmetric volume, a non-empty window
//! (the owner mounts and unmounts cleanly first — the mount replays what
//! this verb never will), a journal too small to hold the four slots and
//! the ring floor, a heap with no free extent. PORTABLE: images in, steps
//! out; the Linux binding (`enable_symmetric_bind.zig`) holds the writer
//! door and executes.

const std = @import("std");
const format = @import("../format.zig");
const geometry = @import("../geometry.zig");
const superblock = @import("../superblock.zig");
const ledger = @import("../ledger.zig");
const journal = @import("../journal.zig");
const alloc_core = @import("../alloc_core.zig");
const alloc_ext = @import("../alloc_ext.zig");
const appender_page = @import("appender_page.zig");
const appender_dir = @import("appender_dir.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
const Superblock = superblock.Superblock;
const ExtentRef = superblock.ExtentRef;

pub const Step = format.Step;

pub const Error = error{ AlreadySymmetric, WindowNotEmpty, JournalTooSmall, NoFreeExtent, Corrupt, OutOfMemory };

/// Every refusal with its words, rendered in ONE place.
pub const Refusal = union(enum) {
    already_symmetric: struct { dir: ExtentRef },
    window_not_empty: struct { tail: u64, head: u64, entries: u64, torn: u64 },
    journal_too_small: struct { len: u64, floor: u64 },
    no_free_extent: struct { free: u64, reserve: u64 },
    /// The images do not decode (the bitmap's deltas, the superblock's
    /// geometry): never "convert" what the mount would refuse.
    corrupt: []const u8,

    pub fn write(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .already_symmetric => |a| try w.print("the superblock already carries SYMMETRIC_PLANE (appender directory at 0x{x}+{d}) — nothing to enable", .{ a.dir.start, a.dir.len }),
            .window_not_empty => |x| try w.print("the replay window is not empty (ledger tail {d}, chain head {d}: {d} entr{s}, {d} torn) — the last unmount was not clean; mount the volume and unmount it cleanly, then retry (the mount replays the window; this verb never does)", .{ x.tail, x.head, x.entries, if (x.entries == 1) "y" else "ies", x.torn }),
            .journal_too_small => |j| try w.print("the fixed journal extent ({d} B) cannot hold appender 0's four page slots and the ring floor ({d} B) — reformat with a larger --meta-journal-mb", .{ j.len, j.floor }),
            .no_free_extent => |n| try w.print("no free heap extent for the appender directory link ({d} free, {d} reserved for compaction)", .{ n.free, n.reserve }),
            .corrupt => |what| try w.print("the volume's {s} does not decode — refusing to convert what a mount would refuse", .{what}),
        }
    }
};

fn refuse(diag: ?*Refusal, r: Refusal) Error {
    if (diag) |d| d.* = r;
    return switch (r) {
        .already_symmetric => error.AlreadySymmetric,
        .window_not_empty => error.WindowNotEmpty,
        .journal_too_small => error.JournalTooSmall,
        .no_free_extent => error.NoFreeExtent,
        .corrupt => error.Corrupt,
    };
}

/// The volume as read: every image the plan judges.
pub const Inputs = struct {
    sb: Superblock,
    /// The newest valid ledger record.
    mounted: ledger.LedgerRecord,
    /// The fixed journal extent's bytes (`sb.journal`, whole).
    ring: []const u8,
    /// The device's O_DIRECT grain (the ring geometry's pad law).
    grain: u64,
    /// The bitmap region's bytes (`sb.alloc_bitmap`, whole).
    bitmap: []const u8,
    /// A crashed run's claimed link (`findStrandedLink`); the plan resumes
    /// onto it and claims nothing new.
    stranded_link: ?u64,
    /// `superblock.nextSuperblockGeneration` over the sectors as read.
    sb_generation: u64,
};

pub const Plan = struct {
    arena: std.heap.ArenaAllocator,
    steps: []Step,
    /// The stamped superblock the last two steps write.
    superblock: Superblock,
    /// The directory link.
    link: ExtentRef,
    /// The ledger record restated (null when the run resumed onto a
    /// claimed link and consumed no seq).
    ledger_seq: ?u64,
    bitmap_generation: ?u64,
    resumed: bool,

    pub fn deinit(self: *Plan) void {
        self.arena.deinit();
        self.* = undefined;
    }

    /// Apply every step to an in-RAM volume image.
    pub fn apply(self: *const Plan, image: []u8) void {
        for (self.steps) |s| @memcpy(image[@intCast(s.offset)..][0..s.bytes.len], s.bytes);
    }

    pub fn barrierCount(self: *const Plan) usize {
        var n: usize = 0;
        for (self.steps) |s| n += @intFromBool(s.barrier_after);
        return n;
    }
};

/// The flat ring's geometry — the one the flat mount replays with.
pub fn flatRingGeometry(sb: Superblock, grain: u64) struct { layout: journal.RingLayout, geo: journal.CoreGeometry } {
    const ring = geometry.fixedRingExtent(sb);
    const layout = journal.RingLayout.fromExtent(.{ .start = ring.start, .len = ring.len });
    return .{ .layout = layout, .geo = journal.productionGeometry(layout.pages, grain) };
}

/// The chain walk's verdict on the flat ring: empty iff the clean-unmount
/// law holds (no entry at or past the tail, the head AT the tail).
pub const Window = struct { tail: u64, head: u64, entries: u64, torn: u64 };

pub fn inspectWindow(gpa: Allocator, sb: Superblock, mounted: ledger.LedgerRecord, ring: []const u8, grain: u64) Allocator.Error!Window {
    const g = flatRingGeometry(sb, grain);
    var rec = try journal.replayScanImage(gpa, ring, g.geo, mounted.journal_tail_seq, journal.FIXED_RING_APPENDER);
    defer rec.deinit();
    return .{ .tail = mounted.journal_tail_seq, .head = rec.head_pos, .entries = rec.entries.len, .torn = rec.dropped_torn };
}

/// A claimed extent carrying a verifying FIRST-link header (`chain_index
/// 0`, the node size's pair count, no successor): the link a crashed run
/// claimed before its stamp. Judged by CONTENT over every claimed extent
/// (one page read each; the tree roots' extents are node headers, never
/// `KVAD`), lowest first. Null = no residue.
pub fn findStrandedLink(reader: appender_dir.PageReader, alloc: *const alloc_ext.ExtentAllocator, sb: Superblock) error{Io}!?u64 {
    const total = sb.totalExtents();
    var buf: [appender_dir.PAGE_LEN]u8 = undefined;
    var e: u64 = 0;
    while (e < total) : (e += 1) {
        if (!alloc.isAllocated(e)) continue;
        try reader.read(reader.ctx, sb.heap.start + e * sb.node_size, &buf);
        const h = appender_dir.Header.decode(&buf) catch continue;
        if (h.chain_index == 0 and !h.hasNext() and h.pairs == appender_dir.pairCount(sb.node_size)) return e;
    }
    return null;
}

/// The plan (module doc). The bitmap is loaded with the EMPTY window the
/// verdict just proved (nothing to fold); the link is claimed from the
/// mirror exactly as the format claims its extents.
pub fn plan(gpa: Allocator, in: Inputs, diag: ?*Refusal) Error!Plan {
    const sb = in.sb;
    if (sb.symmetricPlaneStamped()) return refuse(diag, .{ .already_symmetric = .{ .dir = sb.appender_dir } });
    // The symmetric class's override floor (`geometry.plan`): the ring
    // behind the four slots must still admit the reserve + one max entry.
    const floor = geometry.journal_override_floor + geometry.appender0_reserved_bytes;
    if (sb.journal.len < floor) return refuse(diag, .{ .journal_too_small = .{ .len = sb.journal.len, .floor = floor } });
    const window = try inspectWindow(gpa, sb, in.mounted, in.ring, in.grain);
    if (window.entries != 0 or window.head != window.tail) return refuse(diag, .{ .window_not_empty = .{ .tail = window.tail, .head = window.head, .entries = window.entries, .torn = window.torn } });

    const total = sb.totalExtents();
    var alloc = alloc_ext.ExtentAllocator.load(gpa, in.bitmap, total, alloc_core.compactionReserveExtents(total), alloc_ext.builder_pending_cap, in.mounted.journal_tail_seq, &.{}, null) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        error.MalformedDelta => return refuse(diag, .{ .corrupt = "allocator bitmap" }),
    };
    defer alloc.deinit();

    var arena = std.heap.ArenaAllocator.init(gpa);
    errdefer arena.deinit();
    const a = arena.allocator();
    var steps: std.ArrayList(Step) = .empty;
    const zeros = try a.alloc(u8, format.zero_chunk);
    @memset(zeros, 0);

    // 1. The link: the stranded one (claimed already) or the lowest free.
    const resumed = in.stranded_link != null;
    const link_idx: u64 = if (in.stranded_link) |e| blk: {
        if (e >= total or !alloc.isAllocated(e)) return refuse(diag, .{ .corrupt = "stranded link census" });
        break :blk e;
    } else alloc.claimInternal() catch return refuse(diag, .{ .no_free_extent = .{ .free = alloc.freeExtents(), .reserve = alloc.reserveExtents() } });
    const link: ExtentRef = .{ .start = sb.heap.start + link_idx * sb.node_size, .len = sb.node_size };
    try format.pushZero(a, &steps, zeros, link.start, link.len);
    const hdr = try a.create([appender_dir.PAGE_LEN]u8);
    appender_dir.formatHeader(sb.node_size, hdr);
    try steps.append(a, .{ .offset = link.start, .bytes = hdr, .barrier_after = false });

    // 2. The ring re-based: every byte dead (the window is empty), the
    // whole extent zeroed, appender 0's Free page at slot 1 — the
    // format's image byte for byte.
    var stamped = sb;
    _ = stamped.setSymmetricPlane(link, null) catch unreachable;
    try format.pushZero(a, &steps, zeros, sb.journal.start, sb.journal.len);
    const page_img = try a.create([appender_page.PAGE_LEN]u8);
    const free_page = appender_page.Page.free(journal.FIXED_RING_APPENDER, 1, appender_page.SegmentTable.one(.{ .start = sb.journal.start, .len = sb.journal.len }));
    assert(appender_page.pageSlotFor(free_page.generation) == 1);
    free_page.encode(page_img) catch return refuse(diag, .{ .corrupt = "appender page encoder" });
    try steps.append(a, .{ .offset = sb.journal.start + appender_page.PAGE_LEN, .bytes = page_img, .barrier_after = true });

    // 3. The claim at the next generation, the record restated at that
    // seq (`resumeCheckpointSeq`'s law: above the ledger AND every page
    // copy, so the write never ties a slot).
    var ledger_seq: ?u64 = null;
    var bitmap_generation: ?u64 = null;
    var set = try alloc.beginDirtyPageWrite(alloc_ext.resumeCheckpointSeq(in.mounted.seq, alloc.resumeGeneration()) + 1);
    defer set.deinit();
    if (set.writes.len != 0) {
        assert(!resumed);
        var gen: u64 = 0;
        for (set.writes) |*w| {
            const img = try a.dupe(u8, &w.image);
            try steps.append(a, .{ .offset = sb.alloc_bitmap.start + w.offset, .bytes = img, .barrier_after = false });
            gen = @max(gen, w.generation);
        }
        set.commit(&alloc);
        var rec = in.mounted.restatement(gen);
        rec.alloc_bitmap_generation = gen;
        const slot = try a.create([ledger.ROOT_LEDGER_SLOT_LEN]u8);
        slot.* = rec.encodeSlot() catch return refuse(diag, .{ .corrupt = "ledger record" });
        try steps.append(a, .{ .offset = ledger.slotOffset(sb.root_ledger.start, rec.slotIndex()), .bytes = slot, .barrier_after = true });
        ledger_seq = gen;
        bitmap_generation = gen;
    }

    // 4. The stamp: both copies, each barriered.
    var sb_diag: superblock.Refusal = undefined;
    const sbw = superblock.planWrite(stamped, in.sb_generation, &sb_diag) catch return refuse(diag, .{ .corrupt = "stamped superblock geometry" });
    const sb_img = try a.dupe(u8, &sbw.image);
    for (sbw.slice()) |w| try steps.append(a, .{ .offset = w.offset, .bytes = sb_img, .barrier_after = w.barrier_after });

    return .{
        .arena = arena,
        .steps = try steps.toOwnedSlice(a),
        .superblock = stamped,
        .link = link,
        .ledger_seq = ledger_seq,
        .bitmap_generation = bitmap_generation,
        .resumed = resumed,
    };
}

// ---------------------------------------------------------------------------
// Tests (PURE: a flat format image stands in for the device).
// ---------------------------------------------------------------------------

const testing = std.testing;
const jc = @import("../journal_core.zig");
const key = @import("../key.zig");
const record = @import("../record.zig");
const MiB: u64 = 1 << 20;
const test_uuid: [16]u8 = "kv-backend-test!".*;
const test_set: geometry.SetIdentity = .{ .set_uuid = "set-uuid-fixture".*, .hash_seed = 0x5EED_CAFE_F00D_D00D };
const test_len: u64 = 64 * MiB;
/// A regular file's O_DIRECT grain (the venue's ext4).
const test_grain: u64 = 4096;

fn flatOpts() format.FormatOptions {
    return .{ .class = .single_writer, .node_size = 65536, .journal_len_override = MiB, .uuid = test_uuid, .set = test_set };
}

/// The flat image's superblock, newest ledger record and the three images
/// the plan reads.
const Volume = struct {
    image: []u8,
    sb: Superblock,
    mounted: ledger.LedgerRecord,

    fn load(gpa: Allocator, image: []u8) !Volume {
        const fmt = try superblock.classifyVolumeImages(image[0..4096], image[@intCast(superblock.backupOffsetFor(test_len).?)..][0..4096], test_len, null);
        const sb = fmt.format.v5;
        const mounted = (try ledger.newestValid(gpa, image[@intCast(sb.root_ledger.start)..][0..@intCast(sb.root_ledger.len)])) orelse return error.TestUnexpectedResult;
        return .{ .image = image, .sb = sb, .mounted = mounted };
    }

    fn deinit(self: *Volume, gpa: Allocator) void {
        self.mounted.deinit(gpa);
    }

    fn ring(self: *const Volume) []u8 {
        return self.image[@intCast(self.sb.journal.start)..][0..@intCast(self.sb.journal.len)];
    }

    fn bitmap(self: *const Volume) []u8 {
        return self.image[@intCast(self.sb.alloc_bitmap.start)..][0..@intCast(self.sb.alloc_bitmap.len)];
    }

    fn inputs(self: *const Volume, stranded: ?u64) Inputs {
        return .{ .sb = self.sb, .mounted = self.mounted, .ring = self.ring(), .grain = test_grain, .bitmap = self.bitmap(), .stranded_link = stranded, .sb_generation = superblock.nextSuperblockGeneration(self.image[0..4096], null) };
    }

    fn pageReader(self: *Volume) appender_dir.PageReader {
        return .{ .ctx = self, .read = &readPage };
    }

    fn readPage(ctx: *anyopaque, offset: u64, out: *[appender_dir.PAGE_LEN]u8) error{Io}!void {
        const self: *Volume = @ptrCast(@alignCast(ctx));
        @memcpy(out, self.image[@intCast(offset)..][0..appender_dir.PAGE_LEN]);
    }

    fn loadAlloc(self: *const Volume, gpa: Allocator) !alloc_ext.ExtentAllocator {
        const total = self.sb.totalExtents();
        return alloc_ext.ExtentAllocator.load(gpa, self.bitmap(), total, alloc_core.compactionReserveExtents(total), alloc_ext.builder_pending_cap, self.mounted.journal_tail_seq, &.{}, null);
    }
};

fn refusalText(buf: []u8, r: Refusal) []const u8 {
    var w: std.Io.Writer = .fixed(buf);
    r.write(&w) catch unreachable;
    return w.buffered();
}

/// The converted image mounts as the symmetric class: the stamp, the
/// chain, appender 0's page, the claim, the ledger, the empty ring proper.
fn expectSymmetric(gpa: Allocator, image: []u8, link: ExtentRef, want_ledger_seq: u64, want_generation: u64) !void {
    var v = try Volume.load(gpa, image);
    defer v.deinit(gpa);
    try testing.expect(v.sb.symmetricPlaneStamped());
    try testing.expectEqual(link, v.sb.appender_dir);
    try testing.expectEqual(@as(u64, 0), v.sb.features_incompat & ~superblock.symmetric_plane);
    // The chain: one link, the format's header, every pair zero.
    var links: std.ArrayList(appender_dir.Link) = .empty;
    defer links.deinit(gpa);
    try appender_dir.walkChain(gpa, v.pageReader(), .{ .start = link.start, .len = link.len }, v.sb.node_size, .{ .start = v.sb.heap.start, .len = v.sb.heap.len }, v.sb.totalExtents(), &links);
    try testing.expectEqual(@as(usize, 1), links.items.len);
    try testing.expect(std.mem.allEqual(u8, image[@intCast(link.start + 4096)..][0..@intCast(link.len - 4096)], 0));
    // Appender 0's four slots: slot 1 the Free page naming the fixed
    // extent, the other three zero; the ring proper all zero.
    const offs = appender_page.appender0PageOffsets(v.sb.journal.start);
    const p0 = (try appender_page.newestValid(.{ image[@intCast(offs[0])..][0..4096], image[@intCast(offs[1])..][0..4096], image[@intCast(offs[2])..][0..4096], image[@intCast(offs[3])..][0..4096] })).?;
    try testing.expect(p0.state == .free and p0.appender_id == 0 and p0.generation == 1 and p0.identity.eql(.none));
    try testing.expectEqual(appender_page.Extent{ .start = v.sb.journal.start, .len = v.sb.journal.len }, p0.segments.items[0]);
    for ([_]usize{ 0, 2, 3 }) |i| try testing.expect(std.mem.allEqual(u8, image[@intCast(offs[i])..][0..4096], 0));
    const ring = geometry.fixedRingExtent(v.sb);
    try testing.expectEqual(v.sb.journal.start + geometry.appender0_reserved_bytes, ring.start);
    try testing.expect(std.mem.allEqual(u8, image[@intCast(ring.start)..][0..@intCast(ring.len)], 0));
    // The ledger: the restated record at the consumed seq, the same roots
    // and tail, the new generation; the bitmap claims the link and still
    // every root.
    try testing.expectEqual(want_ledger_seq, v.mounted.seq);
    try testing.expectEqual(want_generation, v.mounted.alloc_bitmap_generation);
    try testing.expectEqual(@as(usize, 2), v.mounted.tree_roots.len);
    var loaded = try v.loadAlloc(gpa);
    defer loaded.deinit();
    try testing.expectEqual(want_generation, loaded.resumeGeneration());
    try testing.expect(loaded.isAllocated((link.start - v.sb.heap.start) / v.sb.node_size));
    for (v.mounted.tree_roots) |root| try testing.expect(loaded.isAllocated((root.node_addr - v.sb.heap.start) / v.sb.node_size));
    // The symmetric mount's replay of the re-based ring: nothing, the
    // head at the tail (a clean unmount).
    const g = flatRingGeometry(v.sb, test_grain);
    var rec = try journal.replayScanImage(gpa, image[@intCast(ring.start)..][0..@intCast(ring.len)], g.geo, v.mounted.journal_tail_seq, journal.FIXED_RING_APPENDER);
    defer rec.deinit();
    try testing.expectEqual(@as(usize, 0), rec.entries.len);
    try testing.expectEqual(v.mounted.journal_tail_seq, rec.head_pos);
    try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
}

test "enable-symmetric over a flat format image: the link at the lowest free extent (2: behind the two trees at 0 and 1 — never extent 0, which holds a tree) zeroed with its KVAD header, the ring zeroed with appender 0's Free page at slot 1, the bitmap at generation 2 + the ledger restated at seq 2, the stamp last; 4 barriers in the stated order; the result reads as the symmetric class and mounts clean; a second run refuses AlreadySymmetric" {
    const gpa = testing.allocator;
    const image = try format.formatImage(gpa, test_len, flatOpts());
    defer gpa.free(image);
    var v = try Volume.load(gpa, image);
    defer v.deinit(gpa);
    try testing.expect(!v.sb.symmetricPlaneStamped());
    try testing.expectEqual(@as(u64, 1), v.mounted.seq);
    try testing.expectEqual(@as(u64, 0), v.mounted.journal_tail_seq);
    var p = try plan(gpa, v.inputs(null), null);
    defer p.deinit();
    try testing.expect(!p.resumed);
    // The flat format packs the control tree at extent 0 and the native
    // tree at extent 1; the lowest free extent is 2.
    try testing.expectEqual(ExtentRef{ .start = v.sb.heap.start + 2 * 65536, .len = 65536 }, p.link);
    try testing.expectEqual(@as(?u64, 2), p.ledger_seq);
    try testing.expectEqual(@as(?u64, 2), p.bitmap_generation);
    try testing.expectEqual(@as(usize, 4), p.barrierCount());
    // The order: link zero + header, journal zero (1 MiB = one chunk) +
    // page (barrier), bitmap page + ledger slot (barrier), SB tail
    // (barrier), sector 0 (barrier).
    const s = p.steps;
    try testing.expectEqual(@as(usize, 1 + 1 + 1 + 1 + 1 + 1 + 2), s.len);
    try testing.expectEqual(p.link.start, s[0].offset);
    try testing.expect(std.mem.allEqual(u8, s[0].bytes, 0) and s[0].bytes.len == 65536);
    try testing.expectEqual(p.link.start, s[1].offset);
    try testing.expectEqualStrings(appender_dir.MAGIC, s[1].bytes[0..4]);
    try testing.expectEqual(v.sb.journal.start, s[2].offset);
    try testing.expectEqual(@as(usize, MiB), s[2].bytes.len);
    try testing.expectEqual(v.sb.journal.start + 4096, s[3].offset);
    try testing.expectEqualStrings(appender_page.MAGIC, s[3].bytes[0..4]);
    try testing.expect(s[3].barrier_after);
    try testing.expectEqual(v.sb.alloc_bitmap.start + alloc_ext.pageSlotOffset(0, 1), s[4].offset);
    try testing.expectEqual(v.sb.root_ledger.start + 2 * 4096, s[5].offset);
    try testing.expect(s[5].barrier_after);
    try testing.expectEqual(v.sb.backupOffset(), s[6].offset);
    try testing.expectEqual(@as(u64, 0), s[7].offset);
    try testing.expect(s[6].barrier_after and s[7].barrier_after);
    try testing.expectEqualSlices(u8, s[6].bytes, s[7].bytes);
    p.apply(image);
    try expectSymmetric(gpa, image, p.link, 2, 2);
    // The second run.
    var v2 = try Volume.load(gpa, image);
    defer v2.deinit(gpa);
    var diag: Refusal = undefined;
    try testing.expectError(error.AlreadySymmetric, plan(gpa, v2.inputs(null), &diag));
    var buf: [256]u8 = undefined;
    try testing.expect(std.mem.startsWith(u8, refusalText(&buf, diag), "the superblock already carries SYMMETRIC_PLANE"));
}

test "enable-symmetric refuses a NON-EMPTY replay window (one landed entry past the tail: entries 1, head past the tail) naming tail/head/entries, and writes nothing; the empty window of a clean unmount at a non-zero tail passes" {
    const gpa = testing.allocator;
    const image = try format.formatImage(gpa, test_len, flatOpts());
    defer gpa.free(image);
    var v = try Volume.load(gpa, image);
    defer v.deinit(gpa);
    const g = flatRingGeometry(v.sb, test_grain);
    // One entry landed at the tail (0): the flat mount would replay it.
    {
        var core = jc.JournalCore.init(g.geo, 0, 0);
        var reg = try jc.InflightRegistry.init(gpa, 0, 4);
        defer reg.deinit();
        const k = key.inodeKey(77);
        const value = (record.InodeValue{ .mode = 0o100644, .nlink = 1 }).encode();
        var recs = [_]journal.RecordFrame{.{ .tag = journal.tagFor(key.TREE_INODES, 0), .seq = 0, .kind = .put, .key = &k, .value = &value }};
        const len = try journal.entryLenFor(&recs);
        const adm = core.tryAdmit(len, .user).?;
        const r = try reg.reserveRegistered(&core, adm);
        recs[0].seq = r.seq_base;
        var img = try journal.buildEntryWrite(gpa, g.layout, g.geo, journal.FIXED_RING_APPENDER, r.res, &recs);
        defer img.deinit();
        journal.applyOps(image, 0, img.ops);
        reg.complete(&core, r.res);
    }
    var diag: Refusal = undefined;
    try testing.expectError(error.WindowNotEmpty, plan(gpa, v.inputs(null), &diag));
    try testing.expectEqual(@as(u64, 0), diag.window_not_empty.tail);
    try testing.expectEqual(@as(u64, 1), diag.window_not_empty.entries);
    try testing.expect(diag.window_not_empty.head > 0);
    var buf: [512]u8 = undefined;
    try testing.expect(std.mem.indexOf(u8, refusalText(&buf, diag), "1 entry, 0 torn") != null);
    try testing.expect(!v.sb.symmetricPlaneStamped());
    // The clean unmount after it: the tail moved to the head, the entry
    // behind it is dead — the window is empty and the plan proceeds (the
    // seq space continues from the tail over the zeroed ring).
    const head = diag.window_not_empty.head;
    var clean = v.mounted.restatement(2);
    clean.journal_tail_seq = head;
    const slot = try clean.encodeSlot();
    @memcpy(image[@intCast(ledger.slotOffset(v.sb.root_ledger.start, clean.slotIndex()))..][0..slot.len], &slot);
    var v2 = try Volume.load(gpa, image);
    defer v2.deinit(gpa);
    try testing.expectEqual(head, v2.mounted.journal_tail_seq);
    var p = try plan(gpa, v2.inputs(null), null);
    defer p.deinit();
    p.apply(image);
    try expectSymmetric(gpa, image, p.link, 3, 3);
    var v3 = try Volume.load(gpa, image);
    defer v3.deinit(gpa);
    try testing.expectEqual(head, v3.mounted.journal_tail_seq);
}

test "enable-symmetric's resume: a run that died between the claim and the stamp leaves a claimed KVAD-headed extent on a flat volume — findStrandedLink names it by content, the re-run resumes onto it (no new claim, no new ledger seq) and the stamp names the SAME extent; a flat volume with no residue answers null; a symmetric mount's extent 0 directory is never 'stranded' (it is the superblock's)" {
    const gpa = testing.allocator;
    const image = try format.formatImage(gpa, test_len, flatOpts());
    defer gpa.free(image);
    {
        var v = try Volume.load(gpa, image);
        defer v.deinit(gpa);
        var al = try v.loadAlloc(gpa);
        defer al.deinit();
        try testing.expectEqual(@as(?u64, null), try findStrandedLink(v.pageReader(), &al, v.sb));
    }
    // The crash: every step but the stamp (the last two) applied.
    var link: ExtentRef = undefined;
    {
        var v = try Volume.load(gpa, image);
        defer v.deinit(gpa);
        var p = try plan(gpa, v.inputs(null), null);
        defer p.deinit();
        link = p.link;
        for (p.steps[0 .. p.steps.len - 2]) |s| @memcpy(image[@intCast(s.offset)..][0..s.bytes.len], s.bytes);
    }
    var v = try Volume.load(gpa, image);
    defer v.deinit(gpa);
    try testing.expect(!v.sb.symmetricPlaneStamped());
    try testing.expectEqual(@as(u64, 2), v.mounted.seq);
    var al = try v.loadAlloc(gpa);
    defer al.deinit();
    const stranded = (try findStrandedLink(v.pageReader(), &al, v.sb)).?;
    try testing.expectEqual((link.start - v.sb.heap.start) / v.sb.node_size, stranded);
    var p = try plan(gpa, v.inputs(stranded), null);
    defer p.deinit();
    try testing.expect(p.resumed);
    try testing.expectEqual(link, p.link);
    try testing.expectEqual(@as(?u64, null), p.ledger_seq);
    // No bitmap page, no ledger slot: link + ring + the two SB copies.
    try testing.expectEqual(@as(usize, 3), p.barrierCount());
    for (p.steps) |s| try testing.expect(s.offset < v.sb.alloc_bitmap.start or s.offset >= v.sb.heap.start or s.offset == v.sb.backupOffset());
    p.apply(image);
    try expectSymmetric(gpa, image, link, 2, 2);
    // A stranded index the bitmap does not claim is refused, never trusted.
    var v2 = try Volume.load(gpa, image);
    defer v2.deinit(gpa);
    var diag: Refusal = undefined;
    try testing.expectError(error.AlreadySymmetric, plan(gpa, v2.inputs(null), &diag));
}

test "enable-symmetric refuses a journal extent below the symmetric floor (the four slots + reserve + one max entry) and a heap with no free extent; a flat volume formatted at the flat floor is the former" {
    const gpa = testing.allocator;
    var opts = flatOpts();
    opts.journal_len_override = geometry.journal_override_floor;
    const image = try format.formatImage(gpa, test_len, opts);
    defer gpa.free(image);
    var v = try Volume.load(gpa, image);
    defer v.deinit(gpa);
    var diag: Refusal = undefined;
    try testing.expectError(error.JournalTooSmall, plan(gpa, v.inputs(null), &diag));
    try testing.expectEqual(geometry.journal_override_floor + geometry.appender0_reserved_bytes, diag.journal_too_small.floor);
    var buf: [256]u8 = undefined;
    try testing.expect(std.mem.indexOf(u8, refusalText(&buf, diag), "--meta-journal-mb") != null);
    // No free extent: every bit set in the bitmap image.
    const image2 = try format.formatImage(gpa, test_len, flatOpts());
    defer gpa.free(image2);
    var v2 = try Volume.load(gpa, image2);
    defer v2.deinit(gpa);
    {
        var al = try v2.loadAlloc(gpa);
        defer al.deinit();
        var e: u64 = 0;
        while (e < v2.sb.totalExtents()) : (e += 1) {
            if (!al.isAllocated(e)) _ = try al.claimInternal();
        }
        const pages = try al.writeDirtyPagesToRegion(v2.bitmap(), 2);
        gpa.free(pages);
    }
    try testing.expectError(error.NoFreeExtent, plan(gpa, v2.inputs(null), &diag));
    try testing.expectEqual(@as(u64, 0), diag.no_free_extent.free);
}

test "the refusal texts render" {
    var buf: [512]u8 = undefined;
    try testing.expect(std.mem.indexOf(u8, refusalText(&buf, .{ .window_not_empty = .{ .tail = 5, .head = 9, .entries = 2, .torn = 1 } }), "2 entries, 1 torn") != null);
    try testing.expect(std.mem.indexOf(u8, refusalText(&buf, .{ .no_free_extent = .{ .free = 0, .reserve = 8 } }), "0 free, 8 reserved") != null);
    try testing.expect(std.mem.indexOf(u8, refusalText(&buf, .{ .corrupt = "allocator bitmap" }), "allocator bitmap") != null);
}
