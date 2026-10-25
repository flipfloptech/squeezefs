//! The tree-0 `slot_state:{s}` record as the routing plane reads it
//! (charter §5.5 "Tree 0 … `slot_state:{s} → Unleased{root, cursor, …} |
//! Leased{…}`"; §5.4 a guest slot tree's root lives in the control tree,
//! never in the fixed ledger — the ledger's 4 KiB slot cannot hold 65 536
//! roots). The ONE codec is `sym/control_records.zig`'s (v2, 68 B, binary
//! key `kind 8 ‖ tag ‖ slot u16 BE`, one shape for Unleased / Leased /
//! Releasing); this module is the routing plane's face over it: the key,
//! the scan range in slot index order, the record a one-writer mount
//! publishes (the UNLEASED shape — the root and the ino cursor as a FLOOR,
//! max wins at mount with the stamp's cursor) and the words it reads back.

const std = @import("std");
const cr = @import("../sym/control_records.zig");
const page = @import("../sym/appender_page.zig");
const lease_table = @import("../sym/slots/lease_table.zig");
const tree = @import("../tree.zig");
const node_seq = @import("../node_seq.zig");
const journal = @import("../journal.zig");

pub const SlotState = cr.SlotState;
pub const Root = cr.Root;
pub const KEY_LEN: usize = cr.Tag.slot_state.keyLen();

/// The control-tree key of routing slot `s`.
pub fn keyOf(slot: u16) cr.ControlKey {
    return cr.slotStateKey(slot);
}

/// The inverse: null for a key that is not a `slot_state` record.
pub fn slotOfKey(k: []const u8) ?u16 {
    const tag = cr.tagOfKey(k) orelse return null;
    return if (tag == .slot_state) cr.slotOfKey(k) else null;
}

/// The inclusive scan range every `slot_state` record sorts inside, in
/// slot index order.
pub fn scanRange() cr.KeyRange {
    return cr.rangeOf(.slot_state);
}

/// The UNLEASED record a one-writer mount publishes for a guest tree:
/// the root pointer the mount opens the tree at and the slot's ino
/// cursor (a FLOOR, never lowered).
pub fn unleased(root_addr: u64, root_seq: u64, next_local_ino: u64) SlotState {
    return .{ .variant = .unleased, .g = 0, .root = .{ .addr = root_addr, .seq = root_seq }, .cursor = next_local_ino };
}

pub fn encode(s: SlotState) [cr.SlotStateWire.LEN]u8 {
    return s.encode();
}

pub const decode = cr.SlotStateWire.decode;

/// A guest slot's root as an OFFLINE reader (status/df/fsck, a mount's
/// open) must see it: tree 0's word overridden by its LESSEE's page entry
/// when the page is the live home (`lease_table.settleEntry`: a `Live` or
/// `Releasing` entry at the record's generation under the page's owner —
/// the two-homes law, "a slot's root is named by tree 0 OR by its
/// lessee's page at every instant"; a conflict is C14's). A `Leased`
/// record's words are the grant-time snapshot: the lessee's SMOs move
/// the root and its pool recycles the image the snapshot names, so a
/// reader at tree 0's word alone reads a home the lessee left. A slot
/// leased but never minted has no root yet (`root.addr == 0`).
/// `lessee`: the appender whose page the root was adopted from, else the
/// one a `leased` / `releasing` record names (null when unleased) —
/// another appender's tree is a projection to this mount.
pub const GuestRoot = struct { slot: u16, root: Root, cursor: u64, extents: u64, lessee: ?u32 };

pub const FoldError = error{ Corrupt, Conflict, OutOfMemory };

/// What the folded tree 0 is to its pages.
pub const Tree0View = enum {
    /// A writer's open: tree 0 replayed from its ring, at least as new as
    /// every page (T1 lands the record before any page names the slot) —
    /// an entry above its generation is residue or C14.
    replayed,
    /// A reader's open: the checkpointed PROJECTION, no ring — it lags the
    /// manager's ring by up to one checkpoint, so a page entry ABOVE its
    /// generation is a lease the projection has not caught up with, and the
    /// page is that slot's live home: adopted, the highest generation
    /// winning. (A lease the projection names whose lessee's page no
    /// longer carries it is the mirror case: the word is a home the lessee
    /// left, and the caller re-points until the manager's next record names
    /// the release.)
    projected,
};

/// Whether a page's root at tree 0's generation is NEWER than tree 0's
/// word there. A generation's roots are its lessee's mints, in the
/// lessee's incarnation space, plus the one it was granted or recovered
/// with (the previous lessee's, possibly another space). Within one space
/// the later mint is the newer root (one writer's monotone handle: a
/// demotion's record beats an older page copy a power cut left); across
/// spaces the root inside the lessee's own space is the newer — the
/// generation's only foreign root is its grant-time one (a recovered tree
/// moves under its adopter's space, below the dead joiner's). A node seq
/// is never ordered across spaces. `lessee_space`: the page owner's
/// incarnation ordinal when the fold knows it (appender 0, the manager:
/// 0), else null — a joiner's page, a reader's live home.
fn pageRootNewer(page_root: cr.Root, tree0_root: cr.Root, lessee_space: ?u64, volume_base: u64) bool {
    // A slot granted before its first mint names no root in tree 0.
    if (tree0_root.addr == 0) return true;
    const sp = node_seq.spaceOf(volume_base, page_root.seq);
    if (sp == node_seq.spaceOf(volume_base, tree0_root.seq)) return page_root.seq >= tree0_root.seq;
    const own = lessee_space orelse return true;
    return sp == own;
}

/// Fold tree 0's `slot_state` records (`kvs`, the scan range's) with the
/// entries of `pages` — appender 0's and every live lessee's, each
/// settled under its owner — into `out`: every record, a never-minted
/// slot with `root.addr == 0` (its cursor is still a floor). `volume_base`:
/// the volume's `nodeSeqBase(uuid)`, whose incarnation spaces order a
/// generation's two homes (`pageRootNewer`).
pub fn foldGuestRoots(gpa: std.mem.Allocator, kvs: []const tree.KeyValue, pages: []const page.Page, view: Tree0View, volume_base: u64, out: *std.ArrayList(GuestRoot)) FoldError!void {
    for (kvs) |kv| {
        const slot = slotOfKey(kv.key) orelse continue;
        const v = decode(kv.value) catch return error.Corrupt;
        var root = v.root;
        var cursor = v.cursor;
        var extents: u64 = v.slot_tree_extents;
        var g = v.g;
        var lessee: ?u32 = if (v.variant == .unleased) null else v.appender_id;
        // Whether `root` is a page's: at tree 0's generation a settled page
        // entry beats tree 0's word only when its root is the newer
        // (`pageRootNewer`); between pages only the generation orders.
        var from_page = false;
        for (pages) |p| for (p.slotEntries()) |e| if (e.slot == slot) {
            const ahead = view == .projected and e.g > v.g;
            const verdict: lease_table.Settle = if (ahead)
                .{ .adopt = .{ .g = e.g, .words = .{ .root = .{ .addr = e.root_addr, .seq = e.root_seq }, .cursor = e.cursor, .extents = e.slot_tree_extents } } }
            else
                lease_table.settleEntry(v, e, p.appender_id);
            switch (verdict) {
                .adopt, .complete_release => |a| if (a.g > g or (a.g == g and !from_page and pageRootNewer(a.words.root, root, if (p.appender_id == journal.FIXED_RING_APPENDER) 0 else null, volume_base))) {
                    from_page = true;
                    root = a.words.root;
                    cursor = @max(cursor, a.words.cursor);
                    extents = a.words.extents;
                    g = a.g;
                    if (ahead) lessee = p.appender_id;
                },
                .conflict => return error.Conflict,
                .drop_entry, .stale_entry => {},
            }
        };
        try out.append(gpa, .{ .slot = slot, .root = root, .cursor = cursor, .extents = extents, .lessee = lessee });
    }
}

const testing = std.testing;
const key = @import("../key.zig");

test "the routing face: keys in slot index order inside the scan range; the published unleased record round-trips through the ONE codec; a foreign key is null" {
    const a = keyOf(2);
    const b = keyOf(10);
    try testing.expectEqual(KEY_LEN, a.len);
    try testing.expect(key.lessThan(a.slice(), b.slice()));
    const r = scanRange();
    try testing.expect(!key.lessThan(a.slice(), r.start.slice()) and !key.lessThan(r.end.slice(), b.slice()));
    try testing.expectEqual(@as(?u16, 10), slotOfKey(b.slice()));
    try testing.expectEqual(@as(?u16, null), slotOfKey(cr.slotTailsKey(10).slice()));
    try testing.expectEqual(@as(?u16, null), slotOfKey("set_config"));
    const v = unleased(0x1234_0000, 77, 9000);
    const got = try decode(&encode(v));
    try testing.expectEqual(v, got);
    try testing.expect(got.variant == .unleased and got.appender_id == 0);
    try testing.expectError(error.Corrupt, decode(encode(v)[0..27]));
}

test "the offline fold: tree 0's root stands alone; a Live page entry at the record's g under the manager overrides it when newer; a leased-never-minted slot folds with root 0; a Live entry under another lessee is the conflict; a slot leased to appender 5 takes ITS page's newer root (the two-homes law), never appender 0's stale entry for it" {
    const gpa = testing.allocator;
    const leased: cr.SlotState = .{ .variant = .leased, .appender_id = 0, .g = 2, .root = .{ .addr = 4096, .seq = 1 }, .cursor = 5 };
    const virgin: cr.SlotState = .{ .variant = .leased, .appender_id = 0, .g = 1, .root = .{ .addr = 0, .seq = 0 }, .cursor = 2 };
    const kvs = [_]tree.KeyValue{ .{ .key = @constCast(keyOf(3).slice()), .value = @constCast(&leased.encode()) }, .{ .key = @constCast(keyOf(4).slice()), .value = @constCast(&virgin.encode()) } };
    var p0 = page.Page.free(0, 1, page.SegmentTable.one(.{ .start = 0, .len = 4096 }));
    try p0.setSlots(&.{.{ .slot = 3, .state = .live, .g = 2, .slot_tree_extents = 7, .root_addr = 8192, .root_seq = 9, .cursor = 50 }});
    var out: std.ArrayList(GuestRoot) = .empty;
    defer out.deinit(gpa);
    try foldGuestRoots(gpa, &kvs, &.{}, .replayed, 0, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    try testing.expectEqual(@as(u64, 4096), out.items[0].root.addr);
    try testing.expectEqual(@as(u64, 0), out.items[1].root.addr);
    try testing.expectEqual(@as(u64, 2), out.items[1].cursor);
    out.clearRetainingCapacity();
    try foldGuestRoots(gpa, &kvs, &.{p0}, .replayed, 0, &out);
    try testing.expectEqual(@as(u64, 8192), out.items[0].root.addr);
    try testing.expectEqual(@as(u64, 50), out.items[0].cursor);
    try testing.expectEqual(@as(u64, 7), out.items[0].extents);
    try p0.setSlots(&.{.{ .slot = 3, .state = .live, .g = 2, .slot_tree_extents = 7, .root_addr = 8192, .root_seq = 9, .cursor = 50 }});
    var other = leased;
    other.appender_id = 5;
    const kvs2 = [_]tree.KeyValue{.{ .key = @constCast(keyOf(3).slice()), .value = @constCast(&other.encode()) }};
    out.clearRetainingCapacity();
    try testing.expectError(error.Conflict, foldGuestRoots(gpa, &kvs2, &.{p0}, .replayed, 0, &out));
    // Slot 3 leased to appender 5 at g 3: appender 0's page still carries
    // its entry at g 2 (stale residue, dropped), appender 5's page names
    // the root its SMOs moved to — the fold takes the lessee's.
    var moved = leased;
    moved.appender_id = 5;
    moved.g = 3;
    const kvs3 = [_]tree.KeyValue{.{ .key = @constCast(keyOf(3).slice()), .value = @constCast(&moved.encode()) }};
    var p5 = page.Page.free(5, 1, page.SegmentTable.one(.{ .start = 0, .len = 4096 }));
    try p5.setSlots(&.{.{ .slot = 3, .state = .live, .g = 3, .slot_tree_extents = 2, .root_addr = 16384, .root_seq = 12, .cursor = 60 }});
    out.clearRetainingCapacity();
    try foldGuestRoots(gpa, &kvs3, &.{ p0, p5 }, .replayed, 0, &out);
    try testing.expectEqual(@as(u64, 16384), out.items[0].root.addr);
    try testing.expectEqual(@as(u64, 12), out.items[0].root.seq);
    try testing.expectEqual(@as(u64, 60), out.items[0].cursor);
    try testing.expectEqual(@as(?u32, 5), out.items[0].lessee);
    // Without the lessee's page the fold stands at the grant-time word.
    out.clearRetainingCapacity();
    try foldGuestRoots(gpa, &kvs3, &.{p0}, .replayed, 0, &out);
    try testing.expectEqual(@as(u64, 4096), out.items[0].root.addr);
}

test "a slot the manager adopted from a dead joiner: its page entry at tree 0's generation names a root its own SMOs minted in incarnation 0's space — LOWER than the joiner's grant-time root from incarnation 1's — and the fold takes the page's: homes are ordered by generation alone, never by node seq across spaces (smoke THE SHARED HOT DIRECTORY ACROSS DAEMONS: the open picked B's dead root at 0xa00000, whose extent A had recycled, and refused Corrupt)" {
    const gpa = testing.allocator;
    const b: u64 = 1 << 40;
    const space: u64 = 1 << 38;
    // Tree 0: leased to appender 0 at g 2, the words the recovery installed
    // (B's root, minted in incarnation 1's space).
    const adopted: cr.SlotState = .{ .variant = .leased, .appender_id = 0, .g = 2, .root = .{ .addr = 0xa00000, .seq = b + space + 1 }, .cursor = 19 };
    const kvs = [_]tree.KeyValue{.{ .key = @constCast(keyOf(65).slice()), .value = @constCast(&adopted.encode()) }};
    // Appender 0's page: the live home at g 2, the root A's SMOs moved the
    // tree to (incarnation 0's space).
    var p0 = page.Page.free(0, 1, page.SegmentTable.one(.{ .start = 0, .len = 4096 }));
    try p0.setSlots(&.{.{ .slot = 65, .state = .live, .g = 2, .slot_tree_extents = 3, .root_addr = 0x1f40000, .root_seq = b + 89, .cursor = 40 }});
    var out: std.ArrayList(GuestRoot) = .empty;
    defer out.deinit(gpa);
    try foldGuestRoots(gpa, &kvs, &.{p0}, .replayed, b, &out);
    try testing.expectEqual(@as(u64, 0x1f40000), out.items[0].root.addr);
    try testing.expectEqual(b + 89, out.items[0].root.seq);
    try testing.expectEqual(@as(u64, 40), out.items[0].cursor);
    // A release the next open completes from the page (row 5): the same.
    try p0.setSlots(&.{.{ .slot = 65, .state = .releasing, .g = 2, .slot_tree_extents = 3, .root_addr = 0x1f40000, .root_seq = b + 89, .cursor = 41 }});
    out.clearRetainingCapacity();
    try foldGuestRoots(gpa, &kvs, &.{p0}, .replayed, b, &out);
    try testing.expectEqual(@as(u64, 0x1f40000), out.items[0].root.addr);
}

test "a page copy OLDER than tree 0's replayed record at the same generation — the manager demoted the slot to tree 0 (its record landed) and the page rewrite that drops it did not (a power cut between the two) — folds to tree 0's newer root: within one seq space the later mint is the newer home, the page's word an older root whose records the ledger's tail may already have passed" {
    const gpa = testing.allocator;
    const b: u64 = 1 << 40;
    const demoted: cr.SlotState = .{ .variant = .leased, .appender_id = 0, .g = 1, .root = .{ .addr = 0x5000, .seq = b + 20 }, .cursor = 9 };
    const kvs = [_]tree.KeyValue{.{ .key = @constCast(keyOf(7).slice()), .value = @constCast(&demoted.encode()) }};
    var p0 = page.Page.free(0, 1, page.SegmentTable.one(.{ .start = 0, .len = 4096 }));
    try p0.setSlots(&.{.{ .slot = 7, .state = .live, .g = 1, .slot_tree_extents = 1, .root_addr = 0x4000, .root_seq = b + 10, .cursor = 8 }});
    var out: std.ArrayList(GuestRoot) = .empty;
    defer out.deinit(gpa);
    try foldGuestRoots(gpa, &kvs, &.{p0}, .replayed, b, &out);
    try testing.expectEqual(@as(u64, 0x5000), out.items[0].root.addr);
    try testing.expectEqual(b + 20, out.items[0].root.seq);
    try testing.expectEqual(@as(u64, 9), out.items[0].cursor);
}

test "the projected fold (a reader's tree 0, no ring): a page entry ABOVE the record's generation is a lease the projection has not caught up with — adopted with its owner as the lessee, the highest generation winning over an older lessee's live entry; the replayed fold judges the same entry stale residue" {
    const gpa = testing.allocator;
    // The projection still says Unleased at g 1 (root 4096); appender 5's
    // page says Live at g 2 (its grant landed after the checkpoint),
    // appender 0's page carries a Live entry at g 1 for the slot it held.
    const unleased_: cr.SlotState = .{ .variant = .unleased, .appender_id = 0, .g = 1, .root = .{ .addr = 4096, .seq = 1 }, .cursor = 5 };
    const kvs = [_]tree.KeyValue{.{ .key = @constCast(keyOf(3).slice()), .value = @constCast(&unleased_.encode()) }};
    var p0 = page.Page.free(0, 1, page.SegmentTable.one(.{ .start = 0, .len = 4096 }));
    try p0.setSlots(&.{.{ .slot = 3, .state = .live, .g = 1, .slot_tree_extents = 1, .root_addr = 8192, .root_seq = 2, .cursor = 6 }});
    var p5 = page.Page.free(5, 1, page.SegmentTable.one(.{ .start = 0, .len = 4096 }));
    try p5.setSlots(&.{.{ .slot = 3, .state = .live, .g = 2, .slot_tree_extents = 2, .root_addr = 16384, .root_seq = 12, .cursor = 60 }});
    var out: std.ArrayList(GuestRoot) = .empty;
    defer out.deinit(gpa);
    try foldGuestRoots(gpa, &kvs, &.{ p0, p5 }, .projected, 0, &out);
    try testing.expectEqual(@as(u64, 16384), out.items[0].root.addr);
    try testing.expectEqual(@as(u64, 60), out.items[0].cursor);
    try testing.expectEqual(@as(?u32, 5), out.items[0].lessee);
    // Page order does not decide: the generation does.
    out.clearRetainingCapacity();
    try foldGuestRoots(gpa, &kvs, &.{ p5, p0 }, .projected, 0, &out);
    try testing.expectEqual(@as(u64, 16384), out.items[0].root.addr);
    // The same entries against a REPLAYED tree 0: an Unleased record
    // makes both Live entries stale residue — tree 0's word stands.
    out.clearRetainingCapacity();
    try foldGuestRoots(gpa, &kvs, &.{ p0, p5 }, .replayed, 0, &out);
    try testing.expectEqual(@as(u64, 4096), out.items[0].root.addr);
    try testing.expectEqual(@as(?u32, null), out.items[0].lessee);
}
