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
