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
/// open) must see it: tree 0's word overridden by appender 0's page entry
/// when the page is the live home (`lease_table.settleEntry`: a `Live` or
/// `Releasing` entry at the record's generation under the manager — the
/// two-homes law; a conflict is C14's). A slot leased but never minted has
/// no root yet (`root.addr == 0`).
pub const GuestRoot = struct { slot: u16, root: Root, cursor: u64, extents: u64 };

pub const FoldError = error{ Corrupt, Conflict, OutOfMemory };

/// Fold tree 0's `slot_state` records (`kvs`, the scan range's) with the
/// page's entries into `out` — every record, a never-minted slot with
/// `root.addr == 0` (its cursor is still a floor).
pub fn foldGuestRoots(gpa: std.mem.Allocator, kvs: []const tree.KeyValue, page0: ?page.Page, out: *std.ArrayList(GuestRoot)) FoldError!void {
    for (kvs) |kv| {
        const slot = slotOfKey(kv.key) orelse continue;
        const v = decode(kv.value) catch return error.Corrupt;
        var root = v.root;
        var cursor = v.cursor;
        var extents: u64 = v.slot_tree_extents;
        if (page0) |p0| for (p0.slotEntries()) |e| if (e.slot == slot) {
            switch (lease_table.settleEntry(v, e, journal.FIXED_RING_APPENDER)) {
                .adopt, .complete_release => |a| if (a.words.root.seq >= root.seq) {
                    root = a.words.root;
                    cursor = @max(cursor, a.words.cursor);
                    extents = a.words.extents;
                },
                .conflict => return error.Conflict,
                .drop_entry, .stale_entry => {},
            }
        };
        try out.append(gpa, .{ .slot = slot, .root = root, .cursor = cursor, .extents = extents });
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

test "the offline fold: tree 0's root stands alone; a Live page entry at the record's g under the manager overrides it when newer; a leased-never-minted slot folds with root 0; a Live entry under another lessee is the conflict" {
    const gpa = testing.allocator;
    const leased: cr.SlotState = .{ .variant = .leased, .appender_id = 0, .g = 2, .root = .{ .addr = 4096, .seq = 1 }, .cursor = 5 };
    const virgin: cr.SlotState = .{ .variant = .leased, .appender_id = 0, .g = 1, .root = .{ .addr = 0, .seq = 0 }, .cursor = 2 };
    const kvs = [_]tree.KeyValue{ .{ .key = @constCast(keyOf(3).slice()), .value = @constCast(&leased.encode()) }, .{ .key = @constCast(keyOf(4).slice()), .value = @constCast(&virgin.encode()) } };
    var p0 = page.Page.free(0, 1, page.SegmentTable.one(.{ .start = 0, .len = 4096 }));
    try p0.setSlots(&.{.{ .slot = 3, .state = .live, .g = 2, .slot_tree_extents = 7, .root_addr = 8192, .root_seq = 9, .cursor = 50 }});
    var out: std.ArrayList(GuestRoot) = .empty;
    defer out.deinit(gpa);
    try foldGuestRoots(gpa, &kvs, null, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    try testing.expectEqual(@as(u64, 4096), out.items[0].root.addr);
    try testing.expectEqual(@as(u64, 0), out.items[1].root.addr);
    try testing.expectEqual(@as(u64, 2), out.items[1].cursor);
    out.clearRetainingCapacity();
    try foldGuestRoots(gpa, &kvs, p0, &out);
    try testing.expectEqual(@as(u64, 8192), out.items[0].root.addr);
    try testing.expectEqual(@as(u64, 50), out.items[0].cursor);
    try testing.expectEqual(@as(u64, 7), out.items[0].extents);
    try p0.setSlots(&.{.{ .slot = 3, .state = .live, .g = 2, .slot_tree_extents = 7, .root_addr = 8192, .root_seq = 9, .cursor = 50 }});
    var other = leased;
    other.appender_id = 5;
    const kvs2 = [_]tree.KeyValue{.{ .key = @constCast(keyOf(3).slice()), .value = @constCast(&other.encode()) }};
    out.clearRetainingCapacity();
    try testing.expectError(error.Conflict, foldGuestRoots(gpa, &kvs2, p0, &out));
}
