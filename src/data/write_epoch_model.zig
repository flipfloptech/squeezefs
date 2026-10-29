//! The rewrite epoch's SAVE VISIBILITY model (charter §6.2 item 3; the
//! 2026-10-07 lost overwrite): a CLOSER running an epoch's ONE save against
//! a WRITER landing on the block the save publishes. Standalone and
//! dependency-free (`model/explorer.zig`), one atomic action per step.
//!
//! Shared words: `map` (the block's durable binding: A, then B once the save
//! lands), `visible` (the epoch binds the block — the W1/overlay gate and the
//! read redirect), `a_live` (A not yet freed). The closer's fixed order is
//! save → unbind → free A; the weakening `unbind_before_save` is the pre-fix
//! `table.remove` ahead of the merge. The writer, under its block guard (3):
//! read the gate; derive the BASE — the shadow B while the gate holds, else
//! the map (the weakening `base_from_map` reads the map regardless: the
//! pre-fix `old_binding` snapshot of the durable map); act — a park's next
//! flush seeds its complement from the base, an in-place patch lands in the
//! base's block. The acked bytes are LOST iff their home is A while the map
//! binds B (A is displaced and freed under them), or A was already freed.

pub const Weakening = enum { none, unbind_before_save, base_from_map };

pub fn CloseVisibilityModel(comptime weak: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        const Key = enum(u8) { a, b };
        const CloserAction = enum(u8) { save, unbind, free };
        const closer_order: [3]CloserAction = if (weak == .unbind_before_save) .{ .unbind, .save, .free } else .{ .save, .unbind, .free };

        map: Key = .a,
        visible: bool = true,
        a_live: bool = true,
        pc: [2]u8 = @splat(0),
        /// The writer's registers.
        gate: bool = false,
        base: Key = .a,
        /// Where the acked bytes ended up (null = not acted yet).
        home: ?Key = null,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 3;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == 0) {
                switch (closer_order[self.pc[0]]) {
                    .save => self.map = .b,
                    .unbind => self.visible = false,
                    .free => self.a_live = false,
                }
            } else {
                switch (self.pc[1]) {
                    // `bindsBlock` under (3).
                    0 => self.gate = self.visible,
                    // `baseBinding`: the shadow outranks the map.
                    1 => self.base = if (weak != .base_from_map and self.gate) .b else self.map,
                    // Park (seeded from the base) or patch in place into it.
                    2 => self.home = self.base,
                    else => unreachable,
                }
            }
            self.pc[t] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            const home = self.home orelse return;
            if (home == .a and !self.a_live) return error.AckedBytesInFreedKey;
            if (home == .a and self.map == .b) return error.AckedBytesLost;
        }

        pub fn final(_: *const Self) !void {}
    };
}

/// The law's READER half (2026-10-28, the perf matrix's lay-down;
/// `write_epoch.Base`): the same closer (save → unbind → free A, under no
/// block guard) against a writer under (3) that pairs a map read with the
/// epoch probe and RECORDS a base — the key its own save will displace (an
/// overlay record's classification, a record's parked key). The writer:
/// probe (`seen` = the gate), read the map (`prev`), record — the shadow B
/// while the gate holds now, else `prev`; a probe that saw the gate the
/// record no longer finds starts over (`base_moved`). The base is WRONG
/// iff it is not what the map binds once the closer is done (B): the next
/// save displaces B unparked (`rewrite_shadow_displaced_drift`) and frees
/// the stale key a second time — or, for the overlay, a Hole record seeds
/// zeros over B's bytes. Weakenings: `map_before_probe` (the dev order:
/// the map read first — the overlay's `overwrite` and the record's `live`)
/// and `no_moved_check` (probe first, the landing between probe and record
/// unchecked).
pub const ReaderWeakening = enum { none, map_before_probe, no_moved_check };

pub fn ReaderOrderModel(comptime weak: ReaderWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        const Key = enum(u8) { a, b };
        const CloserAction = enum(u8) { save, unbind, free };
        const closer_order: [3]CloserAction = .{ .save, .unbind, .free };

        map: Key = .a,
        visible: bool = true,
        a_live: bool = true,
        pc: [2]u8 = @splat(0),
        /// The writer's registers.
        seen: bool = false,
        prev: Key = .a,
        /// The recorded base (null = not recorded yet).
        base: ?Key = null,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 3;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == 0) {
                switch (closer_order[self.pc[0]]) {
                    .save => self.map = .b,
                    .unbind => self.visible = false,
                    .free => self.a_live = false,
                }
                self.pc[0] += 1;
                return;
            }
            const probe_at: u8 = if (weak == .map_before_probe) 1 else 0;
            switch (self.pc[1]) {
                2 => {
                    // `record` under the epochs' lock.
                    if (weak == .none and self.seen and !self.visible) {
                        self.pc[1] = 0;
                        return;
                    }
                    self.base = if (self.visible) .b else self.prev;
                },
                else => if (self.pc[1] == probe_at) {
                    self.seen = self.visible;
                } else {
                    self.prev = self.map;
                },
            }
            self.pc[1] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            const base = self.base orelse return;
            if (base == .a and !self.a_live) return error.BaseFreed;
        }

        pub fn final(self: *const Self) !void {
            if (self.base.? != self.map) return error.BaseNotTheMaps;
        }
    };
}

const explorer = @import("../model/explorer.zig");

test "the save visibility model holds; unbinding before the save and basing on the map both lose acked bytes (the recorded weakenings)" {
    try explorer.expectHolds(CloseVisibilityModel(.none), .{});
    try explorer.expectFails(CloseVisibilityModel(.unbind_before_save), .{}, error.InvariantViolated);
    try explorer.expectFails(CloseVisibilityModel(.base_from_map), .{}, error.InvariantViolated);
}

test "the reader order model holds: probe, then map, then a record that starts over when the probed epoch left sight; the map read before the probe and the unchecked landing both record a freed base (the recorded weakenings)" {
    try explorer.expectHolds(ReaderOrderModel(.none), .{});
    try explorer.expectFails(ReaderOrderModel(.map_before_probe), .{}, error.InvariantViolated);
    try explorer.expectFails(ReaderOrderModel(.no_moved_check), .{}, error.InvariantViolated);
}
