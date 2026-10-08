//! The PAGE-BUDGET OVERFLOW LAW (charter §5.5 "Page-budget overflow law";
//! `docs/porting/sym_slot_leases.md` §5): a region holding more slots than
//! its page names (`SLOT_PAGE_BUDGET = 108`) publishes the overflow roots
//! into tree 0; a publication remembers its HOME (`Tree0 | Page`); the ONE
//! page writer ranks entries by what a slot LOSES if dropped — mid-handover
//! first, then page-homed-only, then unpublished, then tree-0-named — and
//! publishes a page-homed slot into tree 0 BEFORE the page drops it. A
//! slot's cursor rides page 0 BEFORE barrier #1 (a death between the
//! ledger record and the page write must not reopen slots one cycle
//! stale). PORTABLE: pure plans over the table's words; the writes are the
//! plane's (bind.zig) at the checkpoint's C4b/C11 hooks.
//!
//! The `sym-storm` loss (spec §5.2): the page named the 108 LOWEST slots,
//! so a later first touch of a LOWER slot pushed a page-published slot off
//! the page with its mark intact — named nowhere durable; the lessee's
//! death recovered it at tree 0's grant-time root (15 fsynced files lost).
//! Hence the rank and publish-before-drop.

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../../../core/counters.zig");
const page = @import("../appender_page.zig");
const cr = @import("../control_records.zig");
const lt = @import("lease_table.zig");

pub const SLOT_PAGE_BUDGET: u16 = page.SLOT_PAGE_BUDGET;

pub const stats = struct {
    /// A page-homed root demoted to tree 0 before the page dropped it.
    pub var meta_kv_forest_page_publications_demoted: counters.ShardedCounter = .{};
    /// Cursor refreshes of the already-named slots written before barrier #1.
    pub var meta_kv_cursor_page_writes: counters.ShardedCounter = .{};
    /// Overflow roots published into tree 0 by the holder's own cycle.
    pub var slot_roots_published_overflow: counters.ShardedCounter = .{};
};

pub const Home = enum(u8) {
    /// Named by neither home yet: its floor keeps its records in the window.
    unpublished = 0,
    tree0 = 1,
    page = 2,
};

/// What is durably named where, per slot (a RAM fact of the holder).
pub const Publication = struct {
    root: cr.Root = .{ .addr = 0, .seq = 0 },
    cursor: u64 = 0,
    home: Home = .unpublished,
    /// Tree 0 named THIS root at some point (the home may have moved on).
    tree0_root: cr.Root = .{ .addr = 0, .seq = 0 },
};

/// One held slot as the page writer sees it.
pub const Held = struct {
    slot: u16,
    g: u32,
    /// The live words (the tree's root, the cell's cursor, the extents).
    root: cr.Root,
    cursor: u64,
    extents: u32,
    /// Mid-handover (the caller's `releasing` or the table's state).
    releasing: bool,
    native: bool,
    pub_: Publication,
};

/// The rank: lower is kept first (spec §5.2's table).
pub fn rank(h: Held) u8 {
    if (h.releasing) return 0;
    if (h.pub_.home == .page) return 1;
    if (h.pub_.home == .unpublished) return 2;
    return 3;
}

pub const Plan = struct {
    /// The entries the page carries, slot-ascending (the on-disk law).
    entries: []page.SlotEntry,
    /// Held slots cut past the budget whose only home was the page: to be
    /// published into tree 0 BEFORE the page write (publish-before-drop).
    demote: []u16,
    /// Held slots cut whose home is tree 0 already or nothing (no act).
    off_page: []u16,

    pub fn deinit(self: *Plan, gpa: Allocator) void {
        gpa.free(self.entries);
        gpa.free(self.demote);
        gpa.free(self.off_page);
    }
};

/// `prepare_page_entries`: rank, cut at the budget, sort the kept by slot.
pub fn plan(gpa: Allocator, held: []const Held, budget: usize) Allocator.Error!Plan {
    const sorted = try gpa.dupe(Held, held);
    defer gpa.free(sorted);
    std.mem.sort(Held, sorted, {}, struct {
        fn lt_(_: void, a: Held, b: Held) bool {
            const ra = rank(a);
            const rb = rank(b);
            if (ra != rb) return ra < rb;
            return a.slot < b.slot;
        }
    }.lt_);
    const kept_n = @min(budget, sorted.len);
    const entries = try gpa.alloc(page.SlotEntry, kept_n);
    errdefer gpa.free(entries);
    for (sorted[0..kept_n], 0..) |h, i| entries[i] = .{ .slot = h.slot, .state = if (h.releasing) .releasing else .live, .g = h.g, .slot_tree_extents = h.extents, .root_addr = h.root.addr, .root_seq = h.root.seq, .cursor = h.cursor };
    std.mem.sort(page.SlotEntry, entries, {}, struct {
        fn lt_(_: void, a: page.SlotEntry, b: page.SlotEntry) bool {
            return a.slot < b.slot;
        }
    }.lt_);
    var demote: std.ArrayList(u16) = .empty;
    errdefer demote.deinit(gpa);
    var off: std.ArrayList(u16) = .empty;
    errdefer off.deinit(gpa);
    for (sorted[kept_n..]) |h| {
        // A page-homed publication at ANY root loses acked records if
        // dropped (at an older root: every record flushed under it that
        // the live root's floor no longer covers — Issue 12); tree 0 must
        // name the LIVE root before the page forgets the slot.
        if (h.pub_.home == .page) try demote.append(gpa, h.slot) else try off.append(gpa, h.slot);
    }
    return .{ .entries = entries, .demote = try demote.toOwnedSlice(gpa), .off_page = try off.toOwnedSlice(gpa) };
}

/// The overflow law's publication set at C4b (before the tail rule): a
/// held slot whose root is NOT page-homed at its LIVE root — unpublished,
/// or tree-0-homed at a stale root/cursor ("mints move no root": a stale
/// cursor republishes too) — is written into tree 0 as `Leased` at its
/// current words. Bounded by `out`.
pub fn tree0Publications(held: []const Held, page_will_name: []const u16, out: []u16) []u16 {
    var n: usize = 0;
    for (held) |h| {
        if (h.releasing or h.native) continue;
        var on_page = false;
        for (page_will_name) |s| on_page = on_page or s == h.slot;
        // Named by the page this cycle: page-homed after the write (the
        // pre-barrier refresh carries its live words).
        if (on_page) continue;
        const stale = h.pub_.home != .tree0 or h.pub_.root.addr != h.root.addr or h.pub_.root.seq != h.root.seq or h.pub_.cursor < h.cursor;
        if (stale and n < out.len) {
            out[n] = h.slot;
            n += 1;
        }
    }
    return out[0..n];
}

/// The cursor-only refresh before barrier #1 (spec §5.3): the previous
/// image's entries with each cursor (and root/extents) raised to the live
/// words — no plan, no drop, no admission; a cycle whose page-named
/// cursors did not move writes nothing. Answers the refreshed entries
/// (null = nothing moved).
pub fn cursorRefresh(gpa: Allocator, previous: []const page.SlotEntry, held: []const Held) Allocator.Error!?[]page.SlotEntry {
    var moved = false;
    const out = try gpa.alloc(page.SlotEntry, previous.len);
    errdefer gpa.free(out);
    for (previous, 0..) |e, i| {
        out[i] = e;
        for (held) |h| if (h.slot == e.slot and h.g == e.g) {
            if (h.cursor > e.cursor or h.root.addr != e.root_addr or h.root.seq != e.root_seq or h.extents != e.slot_tree_extents) {
                out[i].cursor = @max(e.cursor, h.cursor);
                out[i].root_addr = h.root.addr;
                out[i].root_seq = h.root.seq;
                out[i].slot_tree_extents = h.extents;
                moved = true;
            }
            break;
        };
    }
    if (!moved) {
        gpa.free(out);
        return null;
    }
    stats.meta_kv_cursor_page_writes.inc();
    return out;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn heldAt(slot: u16, home: Home, releasing: bool) Held {
    return .{ .slot = slot, .g = 1, .root = .{ .addr = 4096 * (@as(u64, slot) + 1), .seq = 7 }, .cursor = 10, .extents = 1, .releasing = releasing, .native = false, .pub_ = .{ .root = .{ .addr = 4096 * (@as(u64, slot) + 1), .seq = 7 }, .cursor = 10, .home = home } };
}

test "the page plan ranks mid-handover, page-homed, unpublished, tree-0-named; the budget cuts the tail; the kept are slot-ascending; a page-homed slot cut is demoted (published into tree 0 BEFORE the page drops it), a tree-0-named one simply leaves; 109 slots ⇒ one off the page" {
    const gpa = testing.allocator;
    var hs = [_]Held{ heldAt(50, .tree0, false), heldAt(10, .page, false), heldAt(70, .unpublished, false), heldAt(5, .tree0, true), heldAt(60, .page, false) };
    var p = try plan(gpa, &hs, 3);
    defer p.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), p.entries.len);
    try testing.expectEqual(@as(u16, 5), p.entries[0].slot);
    try testing.expectEqual(page.SlotEntryState.releasing, p.entries[0].state);
    try testing.expectEqual(@as(u16, 10), p.entries[1].slot);
    try testing.expectEqual(@as(u16, 60), p.entries[2].slot);
    try testing.expectEqualSlices(u16, &.{}, p.demote);
    try testing.expectEqualSlices(u16, &.{ 70, 50 }, p.off_page);
    // A tighter budget drops a page-homed slot: demoted.
    var p2 = try plan(gpa, &hs, 1);
    defer p2.deinit(gpa);
    try testing.expectEqualSlices(u16, &.{ 10, 60 }, p2.demote);
    // 109 held, all page-homed, lowest slots: the highest leaves and is demoted.
    var many: [109]Held = undefined;
    for (&many, 0..) |*h, i| h.* = heldAt(@intCast(1000 + i), .page, false);
    var p3 = try plan(gpa, &many, SLOT_PAGE_BUDGET);
    defer p3.deinit(gpa);
    try testing.expectEqual(@as(usize, 108), p3.entries.len);
    try testing.expectEqualSlices(u16, &.{1108}, p3.demote);
    for (p3.entries[1..], 1..) |e, i| try testing.expect(p3.entries[i - 1].slot < e.slot);
}

test "tree-0 publications at C4b: a slot the page will name at its live root is skipped; one it will not name is published when unpublished, or tree-0-homed at a stale root or cursor; releasing and native never" {
    var hs = [_]Held{ heldAt(1, .page, false), heldAt(2, .unpublished, false), heldAt(3, .tree0, false), heldAt(4, .tree0, false), heldAt(5, .tree0, false), heldAt(6, .tree0, true) };
    hs[4].cursor = 11;
    hs[3].root.seq = 8;
    var out: [8]u16 = undefined;
    try testing.expectEqualSlices(u16, &.{ 2, 4, 5 }, tree0Publications(&hs, &.{1}, &out));
    // Named by the page this cycle ⇒ nothing for tree 0.
    try testing.expectEqualSlices(u16, &.{ 4, 5 }, tree0Publications(&hs, &.{ 1, 2 }, &out));
    hs[1].native = true;
    try testing.expectEqualSlices(u16, &.{ 4, 5 }, tree0Publications(&hs, &.{1}, &out));
}

test "the cursor refresh before barrier #1 rewrites only the already-named slots whose live words moved, keeps the rest verbatim, and writes nothing when nothing moved" {
    const gpa = testing.allocator;
    const prev = [_]page.SlotEntry{ .{ .slot = 3, .state = .live, .g = 1, .slot_tree_extents = 1, .root_addr = 4096 * 4, .root_seq = 7, .cursor = 10 }, .{ .slot = 9, .state = .live, .g = 2, .slot_tree_extents = 1, .root_addr = 4096 * 10, .root_seq = 7, .cursor = 10 } };
    var hs = [_]Held{ heldAt(3, .page, false), heldAt(9, .page, false) };
    try testing.expectEqual(@as(?[]page.SlotEntry, null), try cursorRefresh(gpa, &prev, &hs));
    hs[0].cursor = 55;
    const w0 = stats.meta_kv_cursor_page_writes.load();
    const r = (try cursorRefresh(gpa, &prev, &hs)).?;
    defer gpa.free(r);
    try testing.expectEqual(@as(u64, 55), r[0].cursor);
    try testing.expectEqual(prev[1], r[1]);
    try testing.expectEqual(w0 + 1, stats.meta_kv_cursor_page_writes.load());
    // A slot whose g moved on (re-granted) is not the previous entry's.
    hs[1].g = 3;
    hs[1].cursor = 99;
    const r2 = (try cursorRefresh(gpa, &prev, &hs)).?;
    defer gpa.free(r2);
    try testing.expectEqual(@as(u64, 10), r2[1].cursor);
}
