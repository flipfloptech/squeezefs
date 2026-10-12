//! `slot_tails` PAST THE INLINE CAP — the spill (charter §5.5 "the released
//! slot's leaf tails"; `docs/porting/sym_slot_leases.md` `slot_tails:{s}`):
//! a release whose flushed-tail set does not fit one tree-0 value writes
//! the entry images into heap extents claimed in the INTERNAL class and
//! journals the record as `spilled{g, runs, csum}` — the images land and
//! barrier BEFORE the entry that names them (the directory-first law a
//! page's carve follows), the hash64 over the concatenated images binds
//! the record to exactly those bytes, and the extents ride the same entry
//! as kind-4 `alloc` deltas (`growDirectory`'s discipline). The reader
//! (the plane's arm, the frame screen's rule 2) folds a spilled set back
//! into the lease table through the same runs; a record whose images fail
//! the hash is read as ABSENT (its leaves screen by the lease alone, loud,
//! counted) — never a wrong tail. A spilled record superseded by a later
//! release gives its extents back with the superseding entry (`free`
//! deltas), the leave's pages-barrier-release-bitmap order. PORTABLE: every
//! device act goes through the manager's `Volume` vtable.

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../../../core/counters.zig");
const checksum = @import("../../../core/checksum.zig");
const log = @import("../../../core/log.zig");
const manager = @import("../manager.zig");
const cr = @import("../control_records.zig");
const page = @import("../appender_page.zig");
const dir = @import("../appender_dir.zig");

pub const stats = struct {
    /// Releases whose tails set spilled past the inline cap.
    pub var slot_tails_spills: counters.ShardedCounter = .{};
    /// Spill extents claimed (one per `spillEntriesPerExtent` entries).
    pub var slot_tails_spill_extents: counters.ShardedCounter = .{};
    /// Spilled sets folded back from their extents at an arm.
    pub var slot_tails_spills_read: counters.ShardedCounter = .{};
    /// A spilled set whose images failed their hash or could not be read:
    /// read as absent (the leaves screen by the lease alone). Must stay 0.
    pub var slot_tails_spill_unreadable: counters.Tripwire = .init("slot_tails_spill_unreadable");
    /// A spill refused for want of heap: the release lands WITHOUT its
    /// tails (the leaves screen by the lease alone, loud).
    pub var slot_tails_spill_refused: counters.ShardedCounter = .{};
};

pub const Error = error{ NoSpace, Io, OutOfMemory };

/// A spilled set as written: the record's runs and hash, and the extents
/// (indices) the entry must carry as `alloc` deltas.
pub const Spilled = struct {
    gpa: Allocator,
    runs: []cr.SpillRun,
    extents: []u64,
    csum: u64,

    pub fn deinit(self: *Spilled) void {
        self.gpa.free(self.runs);
        self.gpa.free(self.extents);
    }

    pub fn record(self: *const Spilled, g: u32) cr.SlotTails {
        return .{ .spilled = .{ .g = g, .runs = self.runs, .csum = self.csum } };
    }
};

/// The page-aligned image length a run of `count` entries occupies.
fn runImageLen(count: u64) u64 {
    const raw = count * cr.TailEntryWire.LEN;
    return (raw + page.PAGE_LEN - 1) / page.PAGE_LEN * page.PAGE_LEN;
}

fn encodeEntry(e: cr.TailEntry) [cr.TailEntryWire.LEN]u8 {
    var w: cr.TailEntryWire = undefined;
    std.mem.writeInt(u64, &w.leaf_addr_le, e.leaf_addr, .little);
    std.mem.writeInt(u32, &w.tail_le, e.tail, .little);
    std.mem.writeInt(u32, &w.reserved_le, 0, .little);
    return std.mem.toBytes(w);
}

/// Write `tails` into freshly claimed extents (one run per extent, at most
/// `spillEntriesPerExtent` entries each), barriered; the extents stay
/// UNPUBLISHED until the caller's entry names them — a failure here gives
/// every claimed extent back at once.
pub fn spill(gpa: Allocator, vol: manager.Volume, tails: []const cr.TailEntry) Error!Spilled {
    const g = vol.vt.geometry(vol.ctx);
    const per = cr.spillEntriesPerExtent(g.node_size);
    std.debug.assert(per > 0 and tails.len > 0);
    const n_ext: usize = @intCast((tails.len + per - 1) / per);
    var extents: std.ArrayList(u64) = .empty;
    errdefer {
        for (extents.items) |e| vol.vt.releaseUnpublished(vol.ctx, e);
        extents.deinit(gpa);
    }
    for (0..n_ext) |_| try extents.append(gpa, vol.vt.claimInternal(vol.ctx) catch {
        stats.slot_tails_spill_refused.inc();
        return error.NoSpace;
    });
    const runs = try gpa.alloc(cr.SpillRun, n_ext);
    errdefer gpa.free(runs);
    var hasher: checksum.Hasher = .init();
    var writes: std.ArrayList(manager.PageWriteOp) = .empty;
    defer writes.deinit(gpa);
    var images: std.ArrayList([]align(1) u8) = .empty;
    defer {
        for (images.items) |im| gpa.free(im);
        images.deinit(gpa);
    }
    var at: usize = 0;
    for (extents.items, runs) |e, *run| {
        const count: u32 = @intCast(@min(per, tails.len - at));
        const img = try gpa.alloc(u8, @intCast(runImageLen(count)));
        try images.append(gpa, img);
        @memset(img, 0);
        for (tails[at .. at + count], 0..) |t, i| @memcpy(img[i * cr.TailEntryWire.LEN ..][0..cr.TailEntryWire.LEN], &encodeEntry(t));
        hasher.update(img[0 .. count * cr.TailEntryWire.LEN]);
        const addr = g.heap.start + e * g.node_size;
        run.* = .{ .extent_addr = addr, .count = count };
        var off: u64 = 0;
        while (off < img.len) : (off += page.PAGE_LEN) try writes.append(gpa, .{ .offset = addr + off, .image = img[@intCast(off)..][0..page.PAGE_LEN] });
        at += count;
    }
    vol.vt.writePages(vol.ctx, writes.items) catch return error.Io;
    stats.slot_tails_spills.inc();
    stats.slot_tails_spill_extents.add(n_ext);
    return .{ .gpa = gpa, .runs = runs, .extents = try extents.toOwnedSlice(gpa), .csum = hasher.finalLow64() };
}

/// The entries of a spilled record read back through its runs (any page
/// reader over the volume: the manager's, a joiner's device); false when
/// the images are unreadable or fail their hash (the tripwire — the set is
/// read as absent, the leaves screen by the lease alone).
pub fn read(gpa: Allocator, reader: dir.PageReader, d: cr.DecodedSlotTails, out: *std.ArrayList(cr.TailEntry)) Allocator.Error!bool {
    std.debug.assert(d.spilled);
    var hasher: checksum.Hasher = .init();
    var i: usize = 0;
    while (i < d.count) : (i += 1) {
        const run = d.spillRunAt(i);
        const len = runImageLen(run.count);
        var off: u64 = 0;
        var pg: [page.PAGE_LEN]u8 = undefined;
        var consumed: u64 = 0;
        while (off < len) : (off += page.PAGE_LEN) {
            reader.read(reader.ctx, run.extent_addr + off, &pg) catch {
                stats.slot_tails_spill_unreadable.fire("a spilled slot_tails image could not be read");
                return false;
            };
            const want: u64 = @as(u64, run.count) * cr.TailEntryWire.LEN - consumed;
            const take: usize = @intCast(@min(want, page.PAGE_LEN));
            hasher.update(pg[0..take]);
            var k: usize = 0;
            while (k + cr.TailEntryWire.LEN <= take) : (k += cr.TailEntryWire.LEN) {
                const w = std.mem.bytesToValue(cr.TailEntryWire, pg[k..][0..cr.TailEntryWire.LEN]);
                try out.append(gpa, .{ .leaf_addr = std.mem.readInt(u64, &w.leaf_addr_le, .little), .tail = std.mem.readInt(u32, &w.tail_le, .little) });
            }
            consumed += take;
        }
    }
    if (hasher.finalLow64() != d.csum) {
        stats.slot_tails_spill_unreadable.fire("a spilled slot_tails record's images fail their hash");
        return false;
    }
    stats.slot_tails_spills_read.inc();
    return true;
}

/// The extent indices a spilled record's runs occupy (the superseding
/// entry's `free` deltas).
pub fn extentsOf(gpa: Allocator, vol: manager.Volume, d: cr.DecodedSlotTails) Allocator.Error![]u64 {
    std.debug.assert(d.spilled);
    const g = vol.vt.geometry(vol.ctx);
    var out: std.ArrayList(u64) = .empty;
    errdefer out.deinit(gpa);
    var i: usize = 0;
    while (i < d.count) : (i += 1) {
        const run = d.spillRunAt(i);
        if (run.extent_addr < g.heap.start) continue;
        try out.append(gpa, (run.extent_addr - g.heap.start) / g.node_size);
    }
    return out.toOwnedSlice(gpa);
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the spill (rung 10 item 5b): a set past the inline cap lands in ceil(n / per_extent) claimed extents, barriered, the record's runs and hash name exactly those images; the read folds every entry back in order; a flipped byte reads as ABSENT (the tripwire); the extents are the record's for the superseding entry's frees" {
    const gpa = testing.allocator;
    // node 4 KiB ⇒ 256 entries per extent; 600 entries ⇒ 3 extents.
    const tv = try manager.TestVolume.init(gpa, 8 << 20, 4096, 64);
    defer tv.deinit();
    const vol = tv.volume();
    const n: usize = 600;
    const tails = try gpa.alloc(cr.TailEntry, n);
    defer gpa.free(tails);
    for (tails, 0..) |*t, i| t.* = .{ .leaf_addr = 0x1000 * (i + 1), .tail = @intCast(100 + i) };
    try testing.expectEqual(@as(u32, 256), cr.spillEntriesPerExtent(4096));
    const spills0 = stats.slot_tails_spills.load();
    const claimed0 = tv.allocatedExtents();
    var sp = try spill(gpa, vol, tails);
    defer sp.deinit();
    try testing.expectEqual(@as(usize, 3), sp.runs.len);
    try testing.expectEqual(@as(usize, 3), sp.extents.len);
    try testing.expectEqual(claimed0 + 3, tv.allocatedExtents());
    try testing.expectEqual(@as(u32, 256), sp.runs[0].count);
    try testing.expectEqual(@as(u32, 88), sp.runs[2].count);
    try testing.expectEqual(spills0 + 1, stats.slot_tails_spills.load());
    // The record encodes as spilled and decodes to the same runs.
    const rec = sp.record(9);
    const buf = try gpa.alloc(u8, cr.slotTailsEncodedLen(rec));
    defer gpa.free(buf);
    const enc = try cr.encodeSlotTails(rec, buf);
    const d = try cr.decodeSlotTails(enc);
    try testing.expect(d.spilled and d.count == 3 and d.g == 9 and d.csum == sp.csum);
    // The read folds every entry back, in order.
    var back: std.ArrayList(cr.TailEntry) = .empty;
    defer back.deinit(gpa);
    try testing.expect(try read(gpa, vol.reader(), d, &back));
    try testing.expectEqual(n, back.items.len);
    for (tails, back.items) |a, b| try testing.expect(a.leaf_addr == b.leaf_addr and a.tail == b.tail);
    const exts = try extentsOf(gpa, vol, d);
    defer gpa.free(exts);
    try testing.expectEqualSlices(u64, sp.extents, exts);
    // A flipped byte in the second extent's image: absent, the tripwire.
    tv.at(sp.runs[1].extent_addr)[5] ^= 0x40;
    const trip0 = counters.tripwireCount("slot_tails_spill_unreadable");
    stats.slot_tails_spill_unreadable.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    back.clearRetainingCapacity();
    try testing.expect(!try read(gpa, vol.reader(), d, &back));
    try log.testing.expectAllFired();
    try testing.expectEqual(trip0 + 1, counters.tripwireCount("slot_tails_spill_unreadable"));
}

test "a spill the heap cannot hold claims nothing durable: every extent taken goes back, NoSpace, counted" {
    const gpa = testing.allocator;
    // 2 extents total; 600 entries at 4 KiB nodes want 3.
    const tv = try manager.TestVolume.init(gpa, 8 << 20, 4096, 2);
    defer tv.deinit();
    const tails = try gpa.alloc(cr.TailEntry, 600);
    defer gpa.free(tails);
    for (tails, 0..) |*t, i| t.* = .{ .leaf_addr = 0x1000 * (i + 1), .tail = 1 };
    const claimed0 = tv.allocatedExtents();
    const refused0 = stats.slot_tails_spill_refused.load();
    try testing.expectError(error.NoSpace, spill(gpa, tv.volume(), tails));
    try testing.expectEqual(claimed0, tv.allocatedExtents());
    try testing.expectEqual(refused0 + 1, stats.slot_tails_spill_refused.load());
}
