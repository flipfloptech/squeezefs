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

const explorer = @import("../model/explorer.zig");

test "the save visibility model holds; unbinding before the save and basing on the map both lose acked bytes (the recorded weakenings)" {
    try explorer.expectHolds(CloseVisibilityModel(.none), .{});
    try explorer.expectFails(CloseVisibilityModel(.unbind_before_save), .{}, error.InvariantViolated);
    try explorer.expectFails(CloseVisibilityModel(.base_from_map), .{}, error.InvariantViolated);
}
