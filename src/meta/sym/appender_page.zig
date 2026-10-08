//! The appender page (charter §5.5 "Appenders"; `docs/porting/
//! sym_appenders_manager.md` §2.1–§2.5): every writer's 4 KiB record —
//! identity, term, state, `is_manager`, the ring segment table (≤ 8), the
//! seq words (`head_hint`, `ledger_tail_seq`, `ckpt_seq`, `seq_offset`),
//! the unclaimed grant's largest ≤ 4 runs and the leased slot entries
//! (≤ `SLOT_PAGE_BUDGET`). The page IS the appender's ledger record (tail,
//! checkpoint seq, roots, cursors) AND its directory entry: one write per
//! checkpoint, one read per recovery. It never names tree 0's root or the
//! stamp, so two managers are unrepresentable (KD-SYM-3). PORTABLE.
//!
//! v4 layout (fixed 296 B, xxh3 LAST over `[0..4088)`, the version right
//! after the magic — the superblock's conventions, 0002 row 43):
//! ```
//! @0   "KVAP"            @4  layout_version u16 = 1   @6  reserved u16
//! @8   appender_id u32   @12 reserved u32              @16 generation u64
//! @24  node_token u64    @32 mount_slot u32            @36 reserved u32
//! @40  writer_id u64     @48 term u64
//! @56  state u8  @57 is_manager u8  @58 home_volume u16  @60 n_segments u8
//! @61  n_grants u8  @62 n_slots u16
//! @64  head_hint u64  @72 ledger_tail_seq u64  @80 ckpt_seq u64  @88 seq_offset u64
//! @96  recovered_by_term u64
//! @104 segments[8]{start u64, len u64}      @232 grants[4]{start u64, len u64}
//! @296 SlotEntry × n_slots (35 B each, slot-ascending), zero tail to @4088
//! @4088 xxh3 u64
//! ```
//! `SLOT_PAGE_BUDGET = (4088 − 296) / 35 = 108` — DERIVED from the fixed
//! part and the entry width, never a free constant (the charter's 108 at
//! the fixed part's natural widths, restated by a comptime assert).
//!
//! Four slots, newest-valid-wins: `pageSlotFor(generation) = generation
//! % 4` rotates so the three predecessors stay intact and a torn newest
//! falls back to a predecessor of the SAME appender with two of slack.
//! The DIRECTORY-FIRST law (§2.4): a directory image must always name the
//! region's CURRENT ring table (the reader finds R0/R1 through a directory
//! image), so a page naming a table the directory does not yet name is
//! written to every such directory slot before any ring-side slot is used
//! — `dirNamedMask` and `pageWriteSlots` are the pure law.

const std = @import("std");
const checksum = @import("../../core/checksum.zig");
const journal = @import("../journal.zig");

const assert = std.debug.assert;

pub const PAGE_LEN: usize = 4096;
pub const MAGIC = "KVAP";
pub const LAYOUT_VERSION: u16 = 1;
/// The checksum's offset: LAST, over everything before it.
pub const CHECKSUM_OFF: usize = PAGE_LEN - 8;
pub const FIXED_LEN: usize = 296;
pub const SLOT_ENTRY_LEN: usize = 35;
/// `(page − checksum − fixed) / entry`.
pub const SLOT_PAGE_BUDGET: u16 = @intCast((CHECKSUM_OFF - FIXED_LEN) / SLOT_ENTRY_LEN);
/// One 256 KiB segment = 64 pages; eight = the 2 MiB largest growth step
/// before the page's remaining bytes bind.
pub const RING_SEGMENTS_MAX: u8 = 8;
/// The current run, its successor, two returns in flight — the alloc
/// lane's ahead-refill depth.
pub const GRANT_RUNS_MAX: u8 = 4;
/// Directory pair + two ring-side (two predecessors of slack for a torn
/// newest).
pub const PAGE_SLOTS: u8 = 4;
/// Appender 0's four page slots are the fixed ring's first four pages.
pub const APPENDER0_RESERVED_PAGES: u64 = 4;
/// Appenders ≥ 1: the ring-side pair is the first segment's first two pages.
pub const RING_SIDE_LEDGER_PAGES: u64 = 2;

pub const State = enum(u8) {
    free = 0,
    live = 1,
    recovering = 2,
    recovered = 3,
};

pub const SlotEntryState = enum(u8) {
    live = 0,
    releasing = 1,
};

/// A device-byte extent.
pub const Extent = struct {
    start: u64,
    len: u64,

    pub fn end(self: Extent) u64 {
        return self.start + self.len;
    }

    pub fn eql(a: Extent, b: Extent) bool {
        return a.start == b.start and a.len == b.len;
    }
};

/// `{node_token, mount_slot, writer_id}` — 24 B (the port's u64 writer id,
/// 0002 row 185).
pub const Identity = struct {
    node_token: u64 = 0,
    mount_slot: u32 = 0,
    writer_id: u64 = 0,

    pub const none: Identity = .{};

    pub fn eql(a: Identity, b: Identity) bool {
        return a.node_token == b.node_token and a.mount_slot == b.mount_slot and a.writer_id == b.writer_id;
    }

    /// Same `(node, mount slot)` — the stable part a rejoin presents.
    pub fn sameMount(a: Identity, b: Identity) bool {
        return a.node_token == b.node_token and a.mount_slot == b.mount_slot;
    }
};

pub const SlotEntry = struct {
    slot: u16,
    state: SlotEntryState,
    g: u32,
    /// The tree's image count (a COUNT, the affinity cap's input).
    slot_tree_extents: u32,
    root_addr: u64,
    root_seq: u64,
    cursor: u64,
};

/// The segment table: whole heap extents, ≤ 8, in ring order.
pub const SegmentTable = struct {
    items: [RING_SEGMENTS_MAX]Extent = @splat(.{ .start = 0, .len = 0 }),
    len: u8 = 0,

    pub fn slice(self: *const SegmentTable) []const Extent {
        return self.items[0..self.len];
    }

    pub fn eql(a: *const SegmentTable, b: *const SegmentTable) bool {
        if (a.len != b.len) return false;
        for (a.slice(), b.slice()) |x, y| if (!x.eql(y)) return false;
        return true;
    }

    pub fn totalBytes(self: *const SegmentTable) u64 {
        var n: u64 = 0;
        for (self.slice()) |s| n += s.len;
        return n;
    }

    pub fn push(self: *SegmentTable, e: Extent) error{TableFull}!void {
        if (self.len == RING_SEGMENTS_MAX) return error.TableFull;
        self.items[self.len] = e;
        self.len += 1;
    }

    pub fn one(e: Extent) SegmentTable {
        var t: SegmentTable = .{};
        t.push(e) catch unreachable;
        return t;
    }
};

pub const GrantTable = struct {
    items: [GRANT_RUNS_MAX]Extent = @splat(.{ .start = 0, .len = 0 }),
    len: u8 = 0,

    pub fn slice(self: *const GrantTable) []const Extent {
        return self.items[0..self.len];
    }

    pub fn push(self: *GrantTable, e: Extent) error{TableFull}!void {
        if (self.len == GRANT_RUNS_MAX) return error.TableFull;
        self.items[self.len] = e;
        self.len += 1;
    }
};

/// The decoded page.
pub const Page = struct {
    appender_id: u32,
    /// Newest-valid-wins key: +1 per write.
    generation: u64,
    identity: Identity = .none,
    /// +1 per (re)join.
    term: u64 = 0,
    recovered_by_term: u64 = 0,
    state: State = .free,
    is_manager: bool = false,
    /// The joiner's HOME volume ordinal (spec Q-6: the membership shard key).
    home_volume: u16 = 0,
    segments: SegmentTable = .{},
    /// The ring head (the seq-space watermark the next carve continues from).
    head_hint: u64 = 0,
    /// This ring's replay tail.
    ledger_tail_seq: u64 = 0,
    ckpt_seq: u64 = 0,
    /// The ring's record-seq offset IN FORCE at the write.
    seq_offset: u64 = 0,
    /// The unclaimed remainder's LARGEST ≤ 4 runs (extent INDEX runs widened
    /// to u64: the segment table's shape — spec Q-12).
    grants: GrantTable = .{},
    slots: [SLOT_PAGE_BUDGET]SlotEntry = undefined,
    n_slots: u16 = 0,

    /// What format and a region release write: identity zeros, Free, no
    /// grant, no slots; `segments` as given (appender 0's fixed ring at
    /// format; empty after a joiner's leave).
    pub fn free(appender_id: u32, generation: u64, segments: SegmentTable) Page {
        return .{ .appender_id = appender_id, .generation = generation, .segments = segments };
    }

    pub fn slotEntries(self: *const Page) []const SlotEntry {
        return self.slots[0..self.n_slots];
    }

    pub fn ringBytes(self: *const Page) u64 {
        return self.segments.totalBytes();
    }

    /// Entries must be slot-ascending (the on-disk law): refused otherwise.
    pub fn setSlots(self: *Page, entries: []const SlotEntry) error{ TooManySlots, Unsorted }!void {
        if (entries.len > SLOT_PAGE_BUDGET) return error.TooManySlots;
        for (entries, 0..) |e, i| if (i > 0 and entries[i - 1].slot >= e.slot) return error.Unsorted;
        @memcpy(self.slots[0..entries.len], entries);
        self.n_slots = @intCast(entries.len);
    }

    pub fn encode(self: *const Page, out: *[PAGE_LEN]u8) EncodeError!void {
        return encodePage(self, out);
    }
};

pub const EncodeError = error{ TooManySegments, TooManyGrants, TooManySlots, Unsorted };

const SlotEntryWire = extern struct {
    slot_le: [2]u8,
    state: u8,
    g_le: [4]u8,
    slot_tree_extents_le: [4]u8,
    root_addr_le: [8]u8,
    root_seq_le: [8]u8,
    cursor_le: [8]u8,
};

comptime {
    assert(@sizeOf(SlotEntryWire) == SLOT_ENTRY_LEN);
    assert(SLOT_PAGE_BUDGET == 108);
    assert(FIXED_LEN + @as(usize, SLOT_PAGE_BUDGET) * SLOT_ENTRY_LEN <= CHECKSUM_OFF);
    assert(APPENDER0_RESERVED_PAGES == PAGE_SLOTS);
}

inline fn put(img: []u8, comptime T: type, off: usize, v: T) void {
    std.mem.writeInt(T, img[off..][0..@sizeOf(T)], v, .little);
}

inline fn get(img: []const u8, comptime T: type, off: usize) T {
    return std.mem.readInt(T, img[off..][0..@sizeOf(T)], .little);
}

pub fn encodePage(p: *const Page, out: *[PAGE_LEN]u8) EncodeError!void {
    if (p.segments.len > RING_SEGMENTS_MAX) return error.TooManySegments;
    if (p.grants.len > GRANT_RUNS_MAX) return error.TooManyGrants;
    if (p.n_slots > SLOT_PAGE_BUDGET) return error.TooManySlots;
    for (p.slotEntries(), 0..) |e, i| if (i > 0 and p.slots[i - 1].slot >= e.slot) return error.Unsorted;
    @memset(out, 0);
    @memcpy(out[0..4], MAGIC);
    put(out, u16, 4, LAYOUT_VERSION);
    put(out, u32, 8, p.appender_id);
    put(out, u64, 16, p.generation);
    put(out, u64, 24, p.identity.node_token);
    put(out, u32, 32, p.identity.mount_slot);
    put(out, u64, 40, p.identity.writer_id);
    put(out, u64, 48, p.term);
    out[56] = @backingInt(p.state);
    out[57] = @intFromBool(p.is_manager);
    put(out, u16, 58, p.home_volume);
    out[60] = p.segments.len;
    out[61] = p.grants.len;
    put(out, u16, 62, p.n_slots);
    put(out, u64, 64, p.head_hint);
    put(out, u64, 72, p.ledger_tail_seq);
    put(out, u64, 80, p.ckpt_seq);
    put(out, u64, 88, p.seq_offset);
    put(out, u64, 96, p.recovered_by_term);
    for (p.segments.slice(), 0..) |s, i| {
        put(out, u64, 104 + i * 16, s.start);
        put(out, u64, 104 + i * 16 + 8, s.len);
    }
    for (p.grants.slice(), 0..) |g, i| {
        put(out, u64, 232 + i * 16, g.start);
        put(out, u64, 232 + i * 16 + 8, g.len);
    }
    for (p.slotEntries(), 0..) |e, i| {
        var w: SlotEntryWire = undefined;
        std.mem.writeInt(u16, &w.slot_le, e.slot, .little);
        w.state = @backingInt(e.state);
        std.mem.writeInt(u32, &w.g_le, e.g, .little);
        std.mem.writeInt(u32, &w.slot_tree_extents_le, e.slot_tree_extents, .little);
        std.mem.writeInt(u64, &w.root_addr_le, e.root_addr, .little);
        std.mem.writeInt(u64, &w.root_seq_le, e.root_seq, .little);
        std.mem.writeInt(u64, &w.cursor_le, e.cursor, .little);
        @memcpy(out[FIXED_LEN + i * SLOT_ENTRY_LEN ..][0..SLOT_ENTRY_LEN], &std.mem.toBytes(w));
    }
    put(out, u64, CHECKSUM_OFF, checksum.xxh3(out[0..CHECKSUM_OFF]));
}

/// The four-slot reader's classification (`A:593-675`).
pub const Classified = union(enum) {
    valid: Page,
    /// All zero: never written (the format leaves three of appender 0's
    /// slots so; a fresh directory pair).
    blank,
    /// A torn or damaged image: the reader falls back to a predecessor.
    corrupt,
    /// Magic + checksum verify but the layout version is not ours: the
    /// whole four-slot read REFUSES, never a fallback (a newer binary's
    /// page would be misread as "blank" otherwise).
    foreign_layout,
};

pub fn classify(img: []const u8) Classified {
    if (img.len != PAGE_LEN) return .corrupt;
    if (std.mem.allEqual(u8, img, 0)) return .blank;
    if (!std.mem.eql(u8, img[0..4], MAGIC)) return .corrupt;
    if (get(img, u64, CHECKSUM_OFF) != checksum.xxh3(img[0..CHECKSUM_OFF])) return .corrupt;
    if (get(img, u16, 4) != LAYOUT_VERSION) return .foreign_layout;
    const p = decodeVerified(img) catch return .corrupt;
    return .{ .valid = p };
}

/// The field decode of a checksum-verified image: every reserved word
/// zero, every count within its bound BEFORE any element is read, unused
/// table entries zero, slots ascending, the tail zero — decode ∘ encode
/// = id.
fn decodeVerified(img: []const u8) error{Corrupt}!Page {
    if (get(img, u16, 6) != 0 or get(img, u32, 12) != 0 or get(img, u32, 36) != 0) return error.Corrupt;
    const state = std.enums.fromInt(State, img[56]) orelse return error.Corrupt;
    if (img[57] > 1) return error.Corrupt;
    const n_segments = img[60];
    const n_grants = img[61];
    const n_slots = get(img, u16, 62);
    if (n_segments > RING_SEGMENTS_MAX or n_grants > GRANT_RUNS_MAX or n_slots > SLOT_PAGE_BUDGET) return error.Corrupt;
    var p: Page = .{
        .appender_id = get(img, u32, 8),
        .generation = get(img, u64, 16),
        .identity = .{ .node_token = get(img, u64, 24), .mount_slot = get(img, u32, 32), .writer_id = get(img, u64, 40) },
        .term = get(img, u64, 48),
        .state = state,
        .is_manager = img[57] == 1,
        .home_volume = get(img, u16, 58),
        .head_hint = get(img, u64, 64),
        .ledger_tail_seq = get(img, u64, 72),
        .ckpt_seq = get(img, u64, 80),
        .seq_offset = get(img, u64, 88),
        .recovered_by_term = get(img, u64, 96),
    };
    for (0..RING_SEGMENTS_MAX) |i| {
        const e: Extent = .{ .start = get(img, u64, 104 + i * 16), .len = get(img, u64, 104 + i * 16 + 8) };
        if (i < n_segments) {
            if (e.len == 0) return error.Corrupt;
            _ = std.math.add(u64, e.start, e.len) catch return error.Corrupt;
            p.segments.push(e) catch unreachable;
        } else if (e.start != 0 or e.len != 0) return error.Corrupt;
    }
    for (0..GRANT_RUNS_MAX) |i| {
        const e: Extent = .{ .start = get(img, u64, 232 + i * 16), .len = get(img, u64, 232 + i * 16 + 8) };
        if (i < n_grants) {
            if (e.len == 0) return error.Corrupt;
            _ = std.math.add(u64, e.start, e.len) catch return error.Corrupt;
            p.grants.push(e) catch unreachable;
        } else if (e.start != 0 or e.len != 0) return error.Corrupt;
    }
    var i: usize = 0;
    while (i < n_slots) : (i += 1) {
        const w = std.mem.bytesToValue(SlotEntryWire, img[FIXED_LEN + i * SLOT_ENTRY_LEN ..][0..SLOT_ENTRY_LEN]);
        const e: SlotEntry = .{
            .slot = std.mem.readInt(u16, &w.slot_le, .little),
            .state = std.enums.fromInt(SlotEntryState, w.state) orelse return error.Corrupt,
            .g = std.mem.readInt(u32, &w.g_le, .little),
            .slot_tree_extents = std.mem.readInt(u32, &w.slot_tree_extents_le, .little),
            .root_addr = std.mem.readInt(u64, &w.root_addr_le, .little),
            .root_seq = std.mem.readInt(u64, &w.root_seq_le, .little),
            .cursor = std.mem.readInt(u64, &w.cursor_le, .little),
        };
        if (i > 0 and p.slots[i - 1].slot >= e.slot) return error.Corrupt;
        p.slots[i] = e;
    }
    p.n_slots = n_slots;
    if (!std.mem.allEqual(u8, img[FIXED_LEN + @as(usize, n_slots) * SLOT_ENTRY_LEN .. CHECKSUM_OFF], 0)) return error.Corrupt;
    return p;
}

/// Strict decode: a valid image or `Corrupt` (`ForeignLayout` typed apart).
pub fn decode(img: []const u8) error{ Corrupt, Blank, ForeignLayout }!Page {
    return switch (classify(img)) {
        .valid => |p| p,
        .blank => error.Blank,
        .corrupt => error.Corrupt,
        .foreign_layout => error.ForeignLayout,
    };
}

// ---------------------------------------------------------------------------
// The four slots.
// ---------------------------------------------------------------------------

/// Slot index of the write at `generation`: the three predecessors stay
/// intact.
pub fn pageSlotFor(generation: u64) u2 {
    return @intCast(generation % PAGE_SLOTS);
}

pub const ReadError = error{ForeignLayout};

/// Newest-valid-wins over the four slot images (null = not read: a
/// ring-side slot before a directory image names a first segment). Any
/// `ForeignLayout` refuses the whole read.
pub fn newestValid(images: [PAGE_SLOTS]?[]const u8) ReadError!?Page {
    var best: ?Page = null;
    for (images) |maybe| {
        const img = maybe orelse continue;
        switch (classify(img)) {
            .valid => |p| if (best == null or p.generation > best.?.generation) {
                best = p;
            },
            .blank, .corrupt => {},
            .foreign_layout => return error.ForeignLayout,
        }
    }
    return best;
}

/// Bit `i` iff directory slot `i`'s valid image names `table` (the slot's
/// image is null when unread/blank/corrupt).
pub fn dirNamedMask(dir_pages: [2]?Page, table: *const SegmentTable) u2 {
    var mask: u2 = 0;
    for (dir_pages, 0..) |maybe, i| {
        const p = maybe orelse continue;
        if (p.segments.eql(table)) mask |= @as(u2, 1) << @intCast(i);
    }
    return mask;
}

/// The slots the next page write lands in, in order, each one generation
/// apart: both directory slots naming the table ⇒ the rotation's one slot;
/// else every directory slot NOT yet naming it (A then B) — the
/// directory-first law.
pub const WriteSlots = struct {
    items: [2]u2 = @splat(0),
    len: u8 = 0,

    pub fn slice(self: *const WriteSlots) []const u2 {
        return self.items[0..self.len];
    }
};

pub fn pageWriteSlots(dir_named: u2, generation: u64) WriteSlots {
    var w: WriteSlots = .{};
    if (dir_named == 0b11) {
        w.items[0] = pageSlotFor(generation);
        w.len = 1;
        return w;
    }
    for (0..2) |i| {
        if (dir_named & (@as(u2, 1) << @intCast(i)) == 0) {
            w.items[w.len] = @intCast(i);
            w.len += 1;
        }
    }
    return w;
}

// ---------------------------------------------------------------------------
// Where the four slots live.
// ---------------------------------------------------------------------------

/// Appender 0's `[A, B, R0, R1]`: the fixed journal extent's first four
/// pages; its ring proper begins at `journal.start + 16384`.
pub fn appender0PageOffsets(journal_start: u64) [PAGE_SLOTS]u64 {
    return .{ journal_start, journal_start + PAGE_LEN, journal_start + 2 * PAGE_LEN, journal_start + 3 * PAGE_LEN };
}

pub fn appender0RingExtent(jrn: Extent) Extent {
    const reserved = APPENDER0_RESERVED_PAGES * PAGE_LEN;
    assert(jrn.len > reserved);
    return .{ .start = jrn.start + reserved, .len = jrn.len - reserved };
}

/// Appenders ≥ 1: `[R0, R1]` are the ring's FIRST segment's first two
/// pages; the ring proper of that segment starts at `+8192`.
pub fn ringSideOffsets(first_segment: Extent) [2]u64 {
    return .{ first_segment.start, first_segment.start + PAGE_LEN };
}

pub fn firstSegmentRingPart(first_segment: Extent) Extent {
    const reserved = RING_SIDE_LEDGER_PAGES * PAGE_LEN;
    assert(first_segment.len > reserved);
    return .{ .start = first_segment.start + reserved, .len = first_segment.len - reserved };
}

/// The ring segments a region's journal maps positions over: appender 0's
/// is the fixed ring behind its four pages; a joiner's is its table with
/// the first segment's two ring-side pages carved off.
pub fn ringPartOf(appender_id: u32, table: *const SegmentTable) SegmentTable {
    assert(table.len > 0);
    var out: SegmentTable = .{};
    for (table.slice(), 0..) |s, i| {
        const part = if (i != 0) s else if (appender_id == journal.FIXED_RING_APPENDER) appender0RingExtent(s) else firstSegmentRingPart(s);
        out.push(part) catch unreachable;
    }
    return out;
}

// ---------------------------------------------------------------------------
// Tests (spec §7 "Page / directory / ring").
// ---------------------------------------------------------------------------

const testing = std.testing;

fn samplePage() Page {
    var p: Page = .{ .appender_id = 3, .generation = 9, .identity = .{ .node_token = 0x1111, .mount_slot = 0x5001, .writer_id = 0x2222 }, .term = 2, .state = .live, .is_manager = false, .home_volume = 1, .head_hint = 100, .ledger_tail_seq = 90, .ckpt_seq = 7, .seq_offset = 500, .recovered_by_term = 0 };
    p.segments.push(.{ .start = 0x400000, .len = 0x40000 }) catch unreachable;
    p.segments.push(.{ .start = 0x800000, .len = 0x40000 }) catch unreachable;
    p.grants.push(.{ .start = 40, .len = 8 }) catch unreachable;
    p.setSlots(&.{ .{ .slot = 3, .state = .live, .g = 1, .slot_tree_extents = 2, .root_addr = 0x900000, .root_seq = 55, .cursor = 1 << 40 | 7 }, .{ .slot = 9, .state = .releasing, .g = 4, .slot_tree_extents = 0, .root_addr = 0, .root_seq = 0, .cursor = 2 } }) catch unreachable;
    return p;
}

fn expectPageEql(a: *const Page, b: *const Page) !void {
    try testing.expectEqual(a.appender_id, b.appender_id);
    try testing.expectEqual(a.generation, b.generation);
    try testing.expect(a.identity.eql(b.identity));
    try testing.expectEqual(a.term, b.term);
    try testing.expectEqual(a.recovered_by_term, b.recovered_by_term);
    try testing.expectEqual(a.state, b.state);
    try testing.expectEqual(a.is_manager, b.is_manager);
    try testing.expectEqual(a.home_volume, b.home_volume);
    try testing.expect(a.segments.eql(&b.segments));
    try testing.expectEqual(a.head_hint, b.head_hint);
    try testing.expectEqual(a.ledger_tail_seq, b.ledger_tail_seq);
    try testing.expectEqual(a.ckpt_seq, b.ckpt_seq);
    try testing.expectEqual(a.seq_offset, b.seq_offset);
    try testing.expectEqualSlices(Extent, a.grants.slice(), b.grants.slice());
    try testing.expectEqualSlices(SlotEntry, a.slotEntries(), b.slotEntries());
}

test "slot_page_budget derives from the page's fixed part: (4088 − 296) / 35 = 108, never a literal; the bounds are the charter's" {
    try testing.expectEqual(@as(u16, 108), SLOT_PAGE_BUDGET);
    try testing.expectEqual(@as(usize, 108), (CHECKSUM_OFF - FIXED_LEN) / SLOT_ENTRY_LEN);
    try testing.expectEqual(@as(u8, 8), RING_SEGMENTS_MAX);
    try testing.expectEqual(@as(u8, 4), GRANT_RUNS_MAX);
    try testing.expectEqual(@as(u8, 4), PAGE_SLOTS);
    try testing.expectEqual(@as(usize, 4088), CHECKSUM_OFF);
}

test "the page round-trips at every bound (8 segments / 4 runs / 108 slots) and the encoder refuses one more of each" {
    var p = samplePage();
    var img: [PAGE_LEN]u8 = undefined;
    try p.encode(&img);
    try testing.expectEqualStrings("KVAP", img[0..4]);
    try testing.expectEqual(@as(u16, 1), get(&img, u16, 4));
    const back = try decode(&img);
    try expectPageEql(&p, &back);
    // Every bound.
    var full: Page = .{ .appender_id = 1, .generation = 1 };
    for (0..RING_SEGMENTS_MAX) |i| try full.segments.push(.{ .start = (i + 1) * 0x40000, .len = 0x40000 });
    for (0..GRANT_RUNS_MAX) |i| try full.grants.push(.{ .start = i * 100 + 1, .len = 3 });
    var entries: [SLOT_PAGE_BUDGET]SlotEntry = undefined;
    for (&entries, 0..) |*e, i| e.* = .{ .slot = @intCast(i), .state = .live, .g = 1, .slot_tree_extents = 1, .root_addr = i, .root_seq = i, .cursor = i };
    try full.setSlots(&entries);
    try full.encode(&img);
    const fb = try decode(&img);
    try expectPageEql(&full, &fb);
    try testing.expectError(error.TableFull, full.segments.push(.{ .start = 1, .len = 1 }));
    try testing.expectError(error.TableFull, full.grants.push(.{ .start = 1, .len = 1 }));
    var too_many: [SLOT_PAGE_BUDGET + 1]SlotEntry = undefined;
    for (&too_many, 0..) |*e, i| e.* = .{ .slot = @intCast(i), .state = .live, .g = 1, .slot_tree_extents = 1, .root_addr = i, .root_seq = i, .cursor = i };
    try testing.expectError(error.TooManySlots, full.setSlots(&too_many));
    // An unsorted list refuses at setSlots and at encode.
    try testing.expectError(error.Unsorted, p.setSlots(&.{ .{ .slot = 5, .state = .live, .g = 1, .slot_tree_extents = 1, .root_addr = 1, .root_seq = 1, .cursor = 1 }, .{ .slot = 5, .state = .live, .g = 1, .slot_tree_extents = 1, .root_addr = 1, .root_seq = 1, .cursor = 1 } }));
    p.slots[0].slot = 10;
    try testing.expectError(error.Unsorted, p.encode(&img));
}

test "decode is total and canonical: reserved bytes, an unsorted list, an unknown state, is_manager = 2, a zero-length segment, a non-zero tail each refuse; a foreign layout byte with a valid checksum is ForeignLayout; all-zero is Blank" {
    const p = samplePage();
    var img: [PAGE_LEN]u8 = undefined;
    try p.encode(&img);
    const Mut = struct {
        fn stamp(buf: *[PAGE_LEN]u8) void {
            put(buf, u64, CHECKSUM_OFF, checksum.xxh3(buf[0..CHECKSUM_OFF]));
        }
    };
    var m = img;
    m[6] = 1;
    Mut.stamp(&m);
    try testing.expectError(error.Corrupt, decode(&m));
    m = img;
    m[56] = 4;
    Mut.stamp(&m);
    try testing.expectError(error.Corrupt, decode(&m));
    m = img;
    m[57] = 2;
    Mut.stamp(&m);
    try testing.expectError(error.Corrupt, decode(&m));
    m = img;
    m[60] = 9;
    Mut.stamp(&m);
    try testing.expectError(error.Corrupt, decode(&m));
    m = img;
    put(&m, u16, 62, 109);
    Mut.stamp(&m);
    try testing.expectError(error.Corrupt, decode(&m));
    m = img;
    put(&m, u64, 104 + 8, 0); // segment 0 zero-length
    Mut.stamp(&m);
    try testing.expectError(error.Corrupt, decode(&m));
    m = img;
    put(&m, u64, 104 + 2 * 16, 7); // unused segment entry non-zero
    Mut.stamp(&m);
    try testing.expectError(error.Corrupt, decode(&m));
    m = img;
    put(&m, u16, FIXED_LEN + SLOT_ENTRY_LEN, 1); // second slot below the first
    Mut.stamp(&m);
    try testing.expectError(error.Corrupt, decode(&m));
    m = img;
    m[FIXED_LEN + 2 * SLOT_ENTRY_LEN + 3] = 1; // the zero tail
    Mut.stamp(&m);
    try testing.expectError(error.Corrupt, decode(&m));
    m = img;
    m[FIXED_LEN + 2] = 2; // slot entry state
    Mut.stamp(&m);
    try testing.expectError(error.Corrupt, decode(&m));
    // A torn image (no checksum restamp) is Corrupt; a foreign layout verifies then refuses typed.
    m = img;
    m[100] ^= 1;
    try testing.expectError(error.Corrupt, decode(&m));
    m = img;
    put(&m, u16, 4, 2);
    Mut.stamp(&m);
    try testing.expectError(error.ForeignLayout, decode(&m));
    try testing.expect(classify(&m) == .foreign_layout);
    const zero: [PAGE_LEN]u8 = @splat(0);
    try testing.expectError(error.Blank, decode(&zero));
    try testing.expect(classify(zero[0..100]) == .corrupt);
    m = img;
    m[0] = 'X';
    try testing.expect(classify(&m) == .corrupt);
}

test "newest valid page wins over the four slots; a torn newest falls back to its predecessor; a ForeignLayout in any slot refuses the whole read" {
    var imgs: [PAGE_SLOTS][PAGE_LEN]u8 = undefined;
    var p = samplePage();
    for (&imgs, 0..) |*img, i| {
        p.generation = 10 + i;
        try p.encode(img);
    }
    const got = (try newestValid(.{ &imgs[0], &imgs[1], &imgs[2], &imgs[3] })).?;
    try testing.expectEqual(@as(u64, 13), got.generation);
    imgs[3][200] ^= 0xFF; // torn
    const fb = (try newestValid(.{ &imgs[0], &imgs[1], &imgs[2], &imgs[3] })).?;
    try testing.expectEqual(@as(u64, 12), fb.generation);
    // Unread ring-side slots are skipped.
    const dir_only = (try newestValid(.{ &imgs[0], &imgs[1], null, null })).?;
    try testing.expectEqual(@as(u64, 11), dir_only.generation);
    const zero: [PAGE_LEN]u8 = @splat(0);
    try testing.expectEqual(@as(?Page, null), try newestValid(.{ &zero, &zero, null, null }));
    put(&imgs[0], u16, 4, 3);
    put(&imgs[0], u64, CHECKSUM_OFF, checksum.xxh3(imgs[0][0..CHECKSUM_OFF]));
    try testing.expectError(error.ForeignLayout, newestValid(.{ &imgs[0], &imgs[1], &imgs[2], &imgs[3] }));
}

test "page slot rotation covers the four slots; the directory-first tie: pageWriteSlots over every (mask, generation) cell" {
    var seen: [PAGE_SLOTS]bool = @splat(false);
    for (0..8) |g| seen[pageSlotFor(g)] = true;
    for (seen) |s| try testing.expect(s);
    try testing.expectEqual(@as(u2, 1), pageSlotFor(1));
    try testing.expectEqual(@as(u2, 0), pageSlotFor(4));
    for (0..8) |g| {
        // Both named ⇒ the rotation's one slot.
        const both = pageWriteSlots(0b11, g);
        try testing.expectEqualSlices(u2, &.{pageSlotFor(g)}, both.slice());
        // Neither ⇒ A then B.
        try testing.expectEqualSlices(u2, &.{ 0, 1 }, pageWriteSlots(0b00, g).slice());
        // One ⇒ the other, whatever the generation says.
        try testing.expectEqualSlices(u2, &.{1}, pageWriteSlots(0b01, g).slice());
        try testing.expectEqualSlices(u2, &.{0}, pageWriteSlots(0b10, g).slice());
    }
    // dirNamedMask reads the directory images' tables.
    const t = SegmentTable.one(.{ .start = 0x400000, .len = 0x40000 });
    var a: Page = .{ .appender_id = 1, .generation = 1, .segments = t };
    var b: Page = .{ .appender_id = 1, .generation = 2 };
    try b.segments.push(.{ .start = 0x800000, .len = 0x40000 });
    try testing.expectEqual(@as(u2, 0b01), dirNamedMask(.{ a, b }, &t));
    try testing.expectEqual(@as(u2, 0b11), dirNamedMask(.{ a, a }, &t));
    try testing.expectEqual(@as(u2, 0b00), dirNamedMask(.{ null, b }, &t));
    a.generation = 3;
    try testing.expectEqual(@as(u2, 0b10), dirNamedMask(.{ b, a }, &t));
}

test "the four slots' device offsets: appender 0 at the fixed journal's first four pages with the ring proper at +16384; a joiner's ring-side pair at its first segment with the ring part at +8192" {
    const js: u64 = 135_168;
    try testing.expectEqual([4]u64{ js, js + 4096, js + 8192, js + 12_288 }, appender0PageOffsets(js));
    const ring = appender0RingExtent(.{ .start = js, .len = 1 << 20 });
    try testing.expectEqual(Extent{ .start = js + 16_384, .len = (1 << 20) - 16_384 }, ring);
    const seg: Extent = .{ .start = 0x400000, .len = 0x40000 };
    try testing.expectEqual([2]u64{ 0x400000, 0x401000 }, ringSideOffsets(seg));
    try testing.expectEqual(Extent{ .start = 0x402000, .len = 0x40000 - 8192 }, firstSegmentRingPart(seg));
    var t = SegmentTable.one(seg);
    try t.push(.{ .start = 0x800000, .len = 0x40000 });
    const part = ringPartOf(1, &t);
    try testing.expectEqual(@as(u8, 2), part.len);
    try testing.expectEqual(firstSegmentRingPart(seg), part.items[0]);
    try testing.expectEqual(t.items[1], part.items[1]);
    const t0 = SegmentTable.one(.{ .start = js, .len = 1 << 20 });
    try testing.expectEqual(ring, ringPartOf(0, &t0).items[0]);
    // A Free page keeps the id, generation and table alone.
    const f = Page.free(0, 1, t0);
    try testing.expect(f.state == .free and f.identity.eql(.none) and f.n_slots == 0 and f.grants.len == 0 and f.segments.eql(&t0));
}

fn pageFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [PAGE_LEN]u8 = undefined;
    const len = smith.slice(&buf);
    const c = classify(buf[0..len]);
    if (c == .valid) {
        var re: [PAGE_LEN]u8 = undefined;
        try c.valid.encode(&re);
        try testing.expectEqualSlices(u8, buf[0..len], &re);
    }
}

test "fuzz: classify is total; a valid page re-encodes byte-identically" {
    var img: [PAGE_LEN]u8 = undefined;
    try samplePage().encode(&img);
    const framed = comptime blk: {
        var len: [4]u8 = undefined;
        std.mem.writeInt(u32, &len, PAGE_LEN, .little);
        break :blk len;
    };
    var corpus_buf: [4 + PAGE_LEN]u8 = undefined;
    @memcpy(corpus_buf[0..4], &framed);
    @memcpy(corpus_buf[4..], &img);
    try std.testing.fuzz({}, pageFuzz, .{ .corpus = &.{&corpus_buf} });
    var prng = std.Random.DefaultPrng.init(0xA9E);
    const r = prng.random();
    var m = img;
    for (0..3000) |_| {
        m = img;
        for (0..r.intRangeAtMost(usize, 1, 4)) |_| m[r.intRangeLessThan(usize, 0, PAGE_LEN)] ^= @as(u8, 1) << r.int(u3);
        put(&m, u64, CHECKSUM_OFF, checksum.xxh3(m[0..CHECKSUM_OFF]));
        if (classify(&m) == .valid) {
            var re: [PAGE_LEN]u8 = undefined;
            try classify(&m).valid.encode(&re);
            try testing.expectEqualSlices(u8, &m, &re);
        }
    }
}
