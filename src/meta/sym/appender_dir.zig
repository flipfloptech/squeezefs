//! The appender directory (charter §5.5 "appenders ≥ 1 live in the
//! appender directory (an extent chain named by the superblock)"; `docs/
//! porting/sym_appenders_manager.md` §2.3, §3.1 step 4): one node-size heap
//! extent per chain link, the `KVAD` header page FIRST at the link's start
//! (v4: like every other unit; the reference put it last), then appender
//! pairs `[A, B]` at `link.start + 4096 + pair × 8192`. The directory IS
//! the appender allocation (spec Q-5 / 0002 row 231): a `Free` or blank
//! pair is an unallocated id, the lowest Free page is the next, the chain
//! grows by one extent when every pair is taken. Appender ids are dense —
//! 0 the fixed ring, then `1..` in chain order — and stable for the
//! volume's life. PORTABLE: the codec, the geometry and the chain walk
//! over a page-read seam (the device binding and the tests supply it).

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const checksum = @import("../../core/checksum.zig");
const page = @import("appender_page.zig");

const assert = std.debug.assert;

pub const PAGE_LEN: usize = page.PAGE_LEN;
pub const MAGIC = "KVAD";
pub const VERSION: u16 = 1;
pub const CHECKSUM_OFF: usize = PAGE_LEN - 8;
pub const Extent = page.Extent;

/// The header page: `"KVAD" ‖ version u16 ‖ reserved u16 ‖ pairs u32 ‖
/// reserved u32 ‖ chain_index u32 ‖ reserved u32 ‖ next.start u64 ‖
/// next.len u64 ‖ zero ‖ hash64 @4088`.
pub const Header = struct {
    /// Pairs this link holds (`pairCount(node_size)`; written so a reader
    /// needs no node size to walk).
    pairs: u32,
    /// 0 for the superblock's link, +1 per growth; the walk refuses a link
    /// whose index is not the one it expected.
    chain_index: u32,
    /// `{0, 0}` = the chain ends here.
    next: Extent,

    pub fn hasNext(self: Header) bool {
        return self.next.len != 0;
    }

    pub fn encode(self: Header, out: *[PAGE_LEN]u8) void {
        @memset(out, 0);
        @memcpy(out[0..4], MAGIC);
        std.mem.writeInt(u16, out[4..6], VERSION, .little);
        std.mem.writeInt(u32, out[8..12], self.pairs, .little);
        std.mem.writeInt(u32, out[16..20], self.chain_index, .little);
        std.mem.writeInt(u64, out[24..32], self.next.start, .little);
        std.mem.writeInt(u64, out[32..40], self.next.len, .little);
        std.mem.writeInt(u64, out[CHECKSUM_OFF..][0..8], checksum.hash64(out[0..CHECKSUM_OFF]), .little);
    }

    /// Total: length, magic, checksum, version, every reserved word and the
    /// padding zero, a `next` that is either none or a non-empty extent
    /// that does not overflow, at least one pair.
    pub fn decode(img: []const u8) error{Corrupt}!Header {
        if (img.len != PAGE_LEN) return error.Corrupt;
        if (!std.mem.eql(u8, img[0..4], MAGIC)) return error.Corrupt;
        if (std.mem.readInt(u64, img[CHECKSUM_OFF..][0..8], .little) != checksum.hash64(img[0..CHECKSUM_OFF])) return error.Corrupt;
        if (std.mem.readInt(u16, img[4..6], .little) != VERSION) return error.Corrupt;
        if (std.mem.readInt(u16, img[6..8], .little) != 0 or std.mem.readInt(u32, img[12..16], .little) != 0 or std.mem.readInt(u32, img[20..24], .little) != 0) return error.Corrupt;
        if (!std.mem.allEqual(u8, img[40..CHECKSUM_OFF], 0)) return error.Corrupt;
        const h: Header = .{
            .pairs = std.mem.readInt(u32, img[8..12], .little),
            .chain_index = std.mem.readInt(u32, img[16..20], .little),
            .next = .{ .start = std.mem.readInt(u64, img[24..32], .little), .len = std.mem.readInt(u64, img[32..40], .little) },
        };
        if (h.pairs == 0) return error.Corrupt;
        if (h.next.len == 0 and h.next.start != 0) return error.Corrupt;
        if (h.next.len != 0) _ = std.math.add(u64, h.next.start, h.next.len) catch return error.Corrupt;
        return h;
    }
};

/// `(node_size/4096 − 1) / 2`: the header page, then whole pairs — 31 at
/// 256 KiB, 7 at 64 KiB.
pub fn pairCount(node_size: u64) u32 {
    return @intCast((node_size / PAGE_LEN - 1) / 2);
}

/// Directory slots `[A, B]` of `pair` inside `link`.
pub fn pairOffsets(link: Extent, pair: u32) [2]u64 {
    const a = link.start + PAGE_LEN + @as(u64, pair) * 2 * PAGE_LEN;
    assert(a + 2 * PAGE_LEN <= link.end());
    return .{ a, a + PAGE_LEN };
}

/// The dense id law: `1 + chain_index × pairs + pair`.
pub fn appenderIdOf(chain_index: u32, pair: u32, pairs: u32) u32 {
    return 1 + chain_index * pairs + pair;
}

pub const Location = struct { chain_index: u32, pair: u32 };

/// The inverse for `id ≥ 1`.
pub fn locate(appender_id: u32, pairs: u32) Location {
    assert(appender_id >= 1 and pairs > 0);
    const i = appender_id - 1;
    return .{ .chain_index = i / pairs, .pair = i % pairs };
}

/// The page-read seam the walk runs over (the device loader or a RAM image).
pub const PageReader = struct {
    ctx: *anyopaque,
    read: *const fn (ctx: *anyopaque, offset: u64, out: *[PAGE_LEN]u8) error{Io}!void,
};

pub const WalkError = error{ Io, Corrupt, OutOfMemory };

/// One link of the walked chain.
pub const Link = struct {
    extent: Extent,
    header: Header,
};

/// Walk the chain from the superblock's extent: every header verified, the
/// `chain_index` sequence enforced, the chain bounded by `max_links` (the
/// heap's extent count — a longer chain is a cycle ⇒ Corrupt), every
/// `next` a `node_size` extent inside `heap`. Appends the links to `out`.
pub fn walkChain(gpa: std.mem.Allocator, reader: PageReader, dir: Extent, node_size: u64, heap: Extent, max_links: u64, out: *std.ArrayList(Link)) WalkError!void {
    var link = dir;
    var expect: u32 = 0;
    var buf: [PAGE_LEN]u8 = undefined;
    while (true) {
        if (@as(u64, expect) >= max_links) return error.Corrupt;
        if (link.len != node_size or link.start < heap.start or link.end() > heap.end() or (link.start - heap.start) % node_size != 0) return error.Corrupt;
        try reader.read(reader.ctx, link.start, &buf);
        const h = Header.decode(&buf) catch return error.Corrupt;
        if (h.chain_index != expect or h.pairs != pairCount(node_size)) return error.Corrupt;
        try out.append(gpa, .{ .extent = link, .header = h });
        if (!h.hasNext()) return;
        link = h.next;
        expect += 1;
    }
}

/// One directory pair's images, classified.
pub const PairImages = struct {
    appender_id: u32,
    offsets: [2]u64,
    pages: [2]?page.Page,
    /// Any slot a foreign binary wrote: the pair is unreadable here.
    foreign: bool,

    /// The newer valid directory image, if any.
    pub fn newest(self: *const PairImages) ?page.Page {
        var best: ?page.Page = null;
        for (self.pages) |maybe| {
            const p = maybe orelse continue;
            if (best == null or p.generation > best.?.generation) best = p;
        }
        return best;
    }

    /// Unallocated: blank or Free on its newest image.
    pub fn isFree(self: *const PairImages) bool {
        const p = self.newest() orelse return true;
        return p.state == .free;
    }
};

/// Read + classify the pair `pair` of `link` (the ring-side slots are read
/// by the region once a directory image names a first segment).
pub fn readPair(reader: PageReader, link: Link, pair: u32) error{Io}!PairImages {
    const offs = pairOffsets(link.extent, pair);
    var out: PairImages = .{ .appender_id = appenderIdOf(link.header.chain_index, pair, link.header.pairs), .offsets = offs, .pages = .{ null, null }, .foreign = false };
    var buf: [PAGE_LEN]u8 = undefined;
    for (offs, 0..) |off, i| {
        try reader.read(reader.ctx, off, &buf);
        switch (page.classify(&buf)) {
            .valid => |p| out.pages[i] = p,
            .blank, .corrupt => {},
            .foreign_layout => out.foreign = true,
        }
    }
    return out;
}

/// The lowest Free (or blank) pair across the chain — the next appender id
/// — or null when every pair is taken (the caller grows the chain).
pub fn lowestFreePair(reader: PageReader, links: []const Link) error{ Io, ForeignLayout }!?PairImages {
    for (links) |l| {
        var pair: u32 = 0;
        while (pair < l.header.pairs) : (pair += 1) {
            const imgs = try readPair(reader, l, pair);
            if (imgs.foreign) return error.ForeignLayout;
            if (imgs.isFree()) return imgs;
        }
    }
    return null;
}

/// A directory write (the loader executes; a barrier where named).
pub const Step = struct {
    offset: u64,
    image: [PAGE_LEN]u8,
    barrier_after: bool,
};

/// The chain growth's durable order (§3.1 step 4): the new link's header
/// (chain_index + 1, next = none) + barrier, THEN the previous last header
/// re-written with `next = new` + barrier — a torn growth leaves a chain
/// that ends where it did. The new extent is zeroed by the caller before
/// (a stale pair image there would be a phantom appender).
pub fn growPlan(last: Link, new_link: Extent) [2]Step {
    var s: [2]Step = undefined;
    const fresh: Header = .{ .pairs = last.header.pairs, .chain_index = last.header.chain_index + 1, .next = .{ .start = 0, .len = 0 } };
    s[0] = .{ .offset = new_link.start, .image = undefined, .barrier_after = true };
    fresh.encode(&s[0].image);
    const relinked: Header = .{ .pairs = last.header.pairs, .chain_index = last.header.chain_index, .next = new_link };
    s[1] = .{ .offset = last.extent.start, .image = undefined, .barrier_after = true };
    relinked.encode(&s[1].image);
    return s;
}

/// The first link's header — what `format` writes.
pub fn formatHeader(node_size: u64, out: *[PAGE_LEN]u8) void {
    (Header{ .pairs = pairCount(node_size), .chain_index = 0, .next = .{ .start = 0, .len = 0 } }).encode(out);
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// A RAM device for the walk tests.
const RamDevice = struct {
    image: []u8,
    base: u64,
    reads: u32 = 0,

    fn reader(self: *RamDevice) PageReader {
        return .{ .ctx = self, .read = read };
    }

    fn read(ctx: *anyopaque, offset: u64, out: *[PAGE_LEN]u8) error{Io}!void {
        const self: *RamDevice = @ptrCast(@alignCast(ctx));
        self.reads += 1;
        if (offset < self.base or offset + PAGE_LEN > self.base + self.image.len) return error.Io;
        @memcpy(out, self.image[@intCast(offset - self.base)..][0..PAGE_LEN]);
    }
};

test "the KVAD header round-trips; pairs per extent derive from the node size (31 at 256 KiB, 7 at 64 KiB); every foreign spelling refuses" {
    try testing.expectEqual(@as(u32, 31), pairCount(262_144));
    try testing.expectEqual(@as(u32, 7), pairCount(65_536));
    try testing.expectEqual(@as(u32, 127), pairCount(1 << 20));
    var img: [PAGE_LEN]u8 = undefined;
    const h: Header = .{ .pairs = 7, .chain_index = 2, .next = .{ .start = 0x800000, .len = 65_536 } };
    h.encode(&img);
    try testing.expectEqualStrings("KVAD", img[0..4]);
    try testing.expectEqual(h, try Header.decode(&img));
    formatHeader(65_536, &img);
    const f = try Header.decode(&img);
    try testing.expect(!f.hasNext() and f.chain_index == 0 and f.pairs == 7);
    var m = img;
    m[6] = 1;
    std.mem.writeInt(u64, m[CHECKSUM_OFF..][0..8], checksum.hash64(m[0..CHECKSUM_OFF]), .little);
    try testing.expectError(error.Corrupt, Header.decode(&m));
    m = img;
    m[100] = 1;
    std.mem.writeInt(u64, m[CHECKSUM_OFF..][0..8], checksum.hash64(m[0..CHECKSUM_OFF]), .little);
    try testing.expectError(error.Corrupt, Header.decode(&m));
    m = img;
    m[4] = 2;
    std.mem.writeInt(u64, m[CHECKSUM_OFF..][0..8], checksum.hash64(m[0..CHECKSUM_OFF]), .little);
    try testing.expectError(error.Corrupt, Header.decode(&m));
    m = img;
    std.mem.writeInt(u32, m[8..12], 0, .little);
    std.mem.writeInt(u64, m[CHECKSUM_OFF..][0..8], checksum.hash64(m[0..CHECKSUM_OFF]), .little);
    try testing.expectError(error.Corrupt, Header.decode(&m));
    m = img;
    std.mem.writeInt(u64, m[24..32], 5, .little); // next.start with len 0
    std.mem.writeInt(u64, m[CHECKSUM_OFF..][0..8], checksum.hash64(m[0..CHECKSUM_OFF]), .little);
    try testing.expectError(error.Corrupt, Header.decode(&m));
    m = img;
    m[7] ^= 1; // torn
    try testing.expectError(error.Corrupt, Header.decode(&m));
    try testing.expectError(error.Corrupt, Header.decode(img[0..4095]));
}

test "pair geometry and the dense id law invert each other" {
    const link: Extent = .{ .start = 0x400000, .len = 65_536 };
    try testing.expectEqual([2]u64{ 0x401000, 0x402000 }, pairOffsets(link, 0));
    try testing.expectEqual([2]u64{ 0x40D000, 0x40E000 }, pairOffsets(link, 6));
    try testing.expectEqual(@as(u32, 1), appenderIdOf(0, 0, 7));
    try testing.expectEqual(@as(u32, 8), appenderIdOf(1, 0, 7));
    try testing.expectEqual(@as(u32, 17), appenderIdOf(2, 2, 7));
    for (1..100) |id| {
        const loc = locate(@intCast(id), 7);
        try testing.expectEqual(@as(u32, @intCast(id)), appenderIdOf(loc.chain_index, loc.pair, 7));
        try testing.expect(loc.pair < 7);
    }
}

test "the chain walk: links in order with their indices enforced; a wrong index, an extent outside the heap, a cycle past the bound each refuse; growth leaves a torn chain where it was" {
    const gpa = testing.allocator;
    const node: u64 = 65_536;
    const heap: Extent = .{ .start = 0x400000, .len = 8 * node };
    const image = try gpa.alloc(u8, heap.len);
    defer gpa.free(image);
    @memset(image, 0);
    var dev: RamDevice = .{ .image = image, .base = heap.start };
    const link0: Extent = .{ .start = heap.start, .len = node };
    formatHeader(node, image[0..PAGE_LEN]);
    var links: std.ArrayList(Link) = .empty;
    defer links.deinit(gpa);
    try walkChain(gpa, dev.reader(), link0, node, heap, 8, &links);
    try testing.expectEqual(@as(usize, 1), links.items.len);
    try testing.expectEqual(@as(u32, 7), links.items[0].header.pairs);
    // Every pair of a fresh link is free; the lowest is pair 0 = id 1.
    const free = (try lowestFreePair(dev.reader(), links.items)).?;
    try testing.expectEqual(@as(u32, 1), free.appender_id);
    try testing.expectEqual(pairOffsets(link0, 0), free.offsets);
    // Grow: header of the new link + barrier, then the old header relinked.
    const link1: Extent = .{ .start = heap.start + 3 * node, .len = node };
    const steps = growPlan(links.items[0], link1);
    try testing.expectEqual(link1.start, steps[0].offset);
    try testing.expect(steps[0].barrier_after and steps[1].barrier_after);
    try testing.expectEqual(link0.start, steps[1].offset);
    // Torn growth: only the new header landed — the chain still ends at link 0.
    @memcpy(image[@intCast(link1.start - heap.start)..][0..PAGE_LEN], &steps[0].image);
    links.clearRetainingCapacity();
    try walkChain(gpa, dev.reader(), link0, node, heap, 8, &links);
    try testing.expectEqual(@as(usize, 1), links.items.len);
    // Both landed: two links, indices 0 and 1.
    @memcpy(image[0..PAGE_LEN], &steps[1].image);
    links.clearRetainingCapacity();
    try walkChain(gpa, dev.reader(), link0, node, heap, 8, &links);
    try testing.expectEqual(@as(usize, 2), links.items.len);
    try testing.expectEqual(@as(u32, 1), links.items[1].header.chain_index);
    try testing.expectEqual(@as(u32, appenderIdOf(1, 0, 7)), (try readPair(dev.reader(), links.items[1], 0)).appender_id);
    // A Live page on pair 0 of link 0 moves the lowest free to pair 1.
    var live: page.Page = .{ .appender_id = 1, .generation = 1, .state = .live };
    try live.segments.push(.{ .start = heap.start + 5 * node, .len = node });
    var pimg: [PAGE_LEN]u8 = undefined;
    try live.encode(&pimg);
    @memcpy(image[@intCast(pairOffsets(link0, 0)[0] - heap.start)..][0..PAGE_LEN], &pimg);
    const next_free = (try lowestFreePair(dev.reader(), links.items)).?;
    try testing.expectEqual(@as(u32, 2), next_free.appender_id);
    const taken = try readPair(dev.reader(), links.items[0], 0);
    try testing.expect(!taken.isFree() and taken.newest().?.appender_id == 1);
    // A foreign layout in a pair refuses the allocation read.
    std.mem.writeInt(u16, pimg[4..6], 9, .little);
    std.mem.writeInt(u64, pimg[page.CHECKSUM_OFF..][0..8], checksum.hash64(pimg[0..page.CHECKSUM_OFF]), .little);
    @memcpy(image[@intCast(pairOffsets(link0, 0)[1] - heap.start)..][0..PAGE_LEN], &pimg);
    try testing.expectError(error.ForeignLayout, lowestFreePair(dev.reader(), links.items));
    // A cycle: link 1 points back at link 0 — the walk refuses at the bound
    // (index mismatch first) and never loops.
    (Header{ .pairs = 7, .chain_index = 1, .next = link0 }).encode(image[@intCast(link1.start - heap.start)..][0..PAGE_LEN]);
    links.clearRetainingCapacity();
    try testing.expectError(error.Corrupt, walkChain(gpa, dev.reader(), link0, node, heap, 8, &links));
    // A next outside the heap refuses; a wrong pairs word refuses.
    (Header{ .pairs = 7, .chain_index = 0, .next = .{ .start = heap.end(), .len = node } }).encode(image[0..PAGE_LEN]);
    links.clearRetainingCapacity();
    try testing.expectError(error.Corrupt, walkChain(gpa, dev.reader(), link0, node, heap, 8, &links));
    (Header{ .pairs = 31, .chain_index = 0, .next = .{ .start = 0, .len = 0 } }).encode(image[0..PAGE_LEN]);
    links.clearRetainingCapacity();
    try testing.expectError(error.Corrupt, walkChain(gpa, dev.reader(), link0, node, heap, 8, &links));
    // A bound of 0 links refuses before any read.
    links.clearRetainingCapacity();
    try testing.expectError(error.Corrupt, walkChain(gpa, dev.reader(), link0, node, heap, 0, &links));
}

/// Random bytes never pass the checksum, so the body also RESTAMPS a full
/// page (the checksum over the fuzzed bytes) to reach every check behind it;
/// a header that decodes re-encodes byte-identically.
fn headerFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [PAGE_LEN + 8]u8 = undefined;
    const len = smith.slice(&buf);
    _ = Header.decode(buf[0..len]) catch {};
    if (len >= PAGE_LEN) {
        const img = buf[0..PAGE_LEN];
        std.mem.writeInt(u64, img[CHECKSUM_OFF..][0..8], checksum.hash64(img[0..CHECKSUM_OFF]), .little);
        if (Header.decode(img)) |h| {
            var re: [PAGE_LEN]u8 = undefined;
            h.encode(&re);
            try std.testing.expectEqualSlices(u8, img, &re);
        } else |_| {}
    }
}

test "fuzz: the appender directory header decoder is total on arbitrary and restamped pages; a decoded header re-encodes byte-identically" {
    try fuzzkit.fuzz({}, headerFuzz, .{});
}
