//! The tree-0 `slot_state:{s}` record (charter §5.5 "Tree 0 … `slot_state:
//! {s} → Unleased{root, cursor, …}`"; §5.4 a guest slot tree's root lives
//! in the control tree, never in the fixed ledger — the ledger's 4 KiB
//! slot cannot hold 65 536 roots). PORTABLE codec: canonical, total, sized.
//!
//! At rung 5 (one writer, no leases) the record carries the UNLEASED shape
//! alone: the root pointer the mount opens the guest tree at, and the
//! slot's ino cursor as a FLOOR (the stamp's cursor is the other floor —
//! max wins at mount; rung 6's lease arm reads both the same way). Rung 6
//! grows the shape behind the version.

const std = @import("std");
const key = @import("../key.zig");

const assert = std.debug.assert;

pub const KEY_PREFIX = "slot_state:";
/// `slot_state:` + up to 5 decimal digits.
pub const KEY_MAX_LEN: usize = KEY_PREFIX.len + 5;
pub const VERSION: u16 = 1;

/// The control-tree key of slot `s` (decimal, the charter's spelling).
pub fn keyOf(buf: *[KEY_MAX_LEN]u8, slot: u16) []const u8 {
    return std.fmt.bufPrint(buf, KEY_PREFIX ++ "{d}", .{slot}) catch unreachable;
}

/// The inverse: null for a key that is not a `slot_state:` record.
pub fn slotOfKey(k: []const u8) ?u16 {
    if (!std.mem.startsWith(u8, k, KEY_PREFIX)) return null;
    const digits = k[KEY_PREFIX.len..];
    if (digits.len == 0 or digits.len > 5) return null;
    // Canonical decimal: no leading zero except "0" itself.
    if (digits.len > 1 and digits[0] == '0') return null;
    return std.fmt.parseInt(u16, digits, 10) catch null;
}

/// The inclusive key range every `slot_state:` record sorts inside
/// (`'/'` < digits < `':'` in ASCII).
pub const SCAN_START = KEY_PREFIX ++ "0";
pub const SCAN_END = KEY_PREFIX ++ ":";

pub const Value = extern struct {
    version_le: [2]u8,
    /// Reserved, zero (rung 6's lease flags).
    flags_le: [2]u8,
    root_addr_le: [8]u8,
    root_seq_le: [8]u8,
    cursor_le: [8]u8,

    pub const LEN: usize = 28;

    pub fn init(root_addr: u64, root_seq: u64, next_local_ino: u64) Value {
        var v: Value = undefined;
        std.mem.writeInt(u16, &v.version_le, VERSION, .little);
        std.mem.writeInt(u16, &v.flags_le, 0, .little);
        std.mem.writeInt(u64, &v.root_addr_le, root_addr, .little);
        std.mem.writeInt(u64, &v.root_seq_le, root_seq, .little);
        std.mem.writeInt(u64, &v.cursor_le, next_local_ino, .little);
        return v;
    }

    pub fn encode(self: Value) [LEN]u8 {
        return std.mem.toBytes(self);
    }

    /// Total: exactly the encoder's form decodes.
    pub fn decode(bytes: []const u8) error{Corrupt}!Value {
        if (bytes.len != LEN) return error.Corrupt;
        const v = std.mem.bytesToValue(Value, bytes[0..LEN]);
        if (std.mem.readInt(u16, &v.version_le, .little) != VERSION) return error.Corrupt;
        if (std.mem.readInt(u16, &v.flags_le, .little) != 0) return error.Corrupt;
        return v;
    }

    pub fn rootAddr(self: *const Value) u64 {
        return std.mem.readInt(u64, &self.root_addr_le, .little);
    }

    pub fn rootSeq(self: *const Value) u64 {
        return std.mem.readInt(u64, &self.root_seq_le, .little);
    }

    pub fn cursor(self: *const Value) u64 {
        return std.mem.readInt(u64, &self.cursor_le, .little);
    }
};

comptime {
    assert(@sizeOf(Value) == Value.LEN);
    assert(key.lessThan(SCAN_START, SCAN_END));
}

const testing = std.testing;

test "slot_state keys are canonical decimal under one prefix, inside the scan range; the value round-trips and refuses foreign shapes" {
    var buf: [KEY_MAX_LEN]u8 = undefined;
    try testing.expectEqualStrings("slot_state:0", keyOf(&buf, 0));
    try testing.expectEqualStrings("slot_state:65535", keyOf(&buf, 65535));
    try testing.expectEqual(@as(?u16, 7), slotOfKey("slot_state:7"));
    try testing.expectEqual(@as(?u16, null), slotOfKey("slot_state:07"));
    try testing.expectEqual(@as(?u16, null), slotOfKey("slot_state:"));
    try testing.expectEqual(@as(?u16, null), slotOfKey("slot_state:70000"));
    try testing.expectEqual(@as(?u16, null), slotOfKey("set_config"));
    for ([_]u16{ 0, 9, 10, 4095, 65535 }) |s| {
        const k = keyOf(&buf, s);
        try testing.expect(!key.lessThan(k, SCAN_START) and key.lessThan(k, SCAN_END));
    }
    const v = Value.init(0x1234_0000, 77, 9000);
    const enc = v.encode();
    const got = try Value.decode(&enc);
    try testing.expectEqual(@as(u64, 0x1234_0000), got.rootAddr());
    try testing.expectEqual(@as(u64, 77), got.rootSeq());
    try testing.expectEqual(@as(u64, 9000), got.cursor());
    try testing.expectError(error.Corrupt, Value.decode(enc[0..27]));
    var bad = enc;
    bad[0] = 2;
    try testing.expectError(error.Corrupt, Value.decode(&bad));
    var flags = enc;
    flags[2] = 1;
    try testing.expectError(error.Corrupt, Value.decode(&flags));
}
