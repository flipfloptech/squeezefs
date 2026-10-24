//! The exhaustive MODEL of `reclaim.zig`'s durability hold (AGENTS.md
//! "Model checks"; charter §6.2; decision 0003 §BT). Standalone,
//! dependency-free value type; every field a byte so the explorer's content
//! hash sees no padding; ONE atomic action per `step` (a step under the
//! hold's lock is one action: the lock serializes it against the others).
//!
//! The law: a block freed by a metadata change is handed to a new owner —
//! whose DMA then writes it — only once the record that freed it is
//! DURABLE: landed, and covered by a barrier that completed after it
//! landed. The commit's window is reserved before its apply and the free is
//! pushed after the commit's answer, so the reservation high-water mark the
//! seal reads, under the same lock the push took, covers every free on the
//! list it seals; the release compares the lane's DURABLE floor (the landed
//! floor a completed barrier started from) against that mark.
//!
//! | model | threads | invariant | weakening that fails it |
//! |---|---|---|---|
//! | `a_held_free_is_reallocated_only_once_its_record_is_durable` | two freers (reserve a window → push their block on the open list); the lane's lander (the landed prefix); two barriers (start: snapshot the landed floor; done: publish it as the durable floor); the releaser (three rounds: lock{seal: open → sealed, mark := reservation high-water mark}, lock{release the sealed blocks when the durable floor ≥ the mark}); a new owner taking free blocks (its DMA) | no DMA ever writes a block whose freeing window is not yet durable | `no_hold` (the shipped ladder: the free list at the push — the power-cut row's red), `seal_outside_lock` (the mark read before the lock that swaps the lists: a free pushed between them rides an older mark), `landed_floor` (the release on the LANDED floor: a window in the device's volatile cache is not durable) |

const std = @import("std");
const explorer = @import("../model/explorer.zig");

pub const Weakening = enum {
    none,
    /// The shipped ladder: a terminal free reaches the free list at once.
    no_hold,
    /// The seal's mark read in its own step before the lock that moves the
    /// open list to the sealed one.
    seal_outside_lock,
    /// The release compares the lane's landed floor, not its durable one.
    landed_floor,
};

pub fn HoldModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 6;
        const F0 = 0;
        const F1 = 1;
        const L = 2;
        const BAR = 3;
        const R = 4;
        const A = 5;
        const ROUND_STEPS: u8 = if (weakening == .seal_outside_lock) 3 else 2;
        const R_STEPS: u8 = 3 * ROUND_STEPS;

        /// The reservation core's next position and the conveyor's
        /// reservation high-water mark (a window's END).
        next_pos: u8 = 0,
        hwm: u8 = 0,
        /// Each freer's window end (0 = not reserved); freer i frees block i.
        end: [2]u8 = .{ 0, 0 },
        /// The landed prefix, a barrier's snapshot, the durable floor.
        floor: u8 = 0,
        bar_snap: u8 = 0,
        durable: u8 = 0,
        /// The hold: block bitmasks of the open and sealed lists, the
        /// sealed mark, the weakened seal's early read.
        open: u8 = 0,
        sealed: u8 = 0,
        mark: u8 = 0,
        r_snap: u8 = 0,
        /// The free list and the new owner's DMAs (block bitmasks).
        free: u8 = 0,
        dma: u8 = 0,
        violated: u8 = 0,
        pc: [thread_count]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                F0, F1 => self.pc[t] == 2,
                L => self.pc[t] == 2,
                BAR => self.pc[t] == 4,
                R => self.pc[t] == R_STEPS,
                A => self.pc[t] == 2,
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                F0, F1 => self.stepFreer(t),
                L => self.stepLander(),
                BAR => self.stepBarrier(),
                R => self.stepReleaser(),
                A => self.stepOwner(),
                else => unreachable,
            }
        }

        /// Reserve the window (its apply and its answer follow), then push
        /// the freed block (after the answer).
        fn stepFreer(self: *Self, t: usize) void {
            switch (self.pc[t]) {
                0 => {
                    self.next_pos += 1;
                    self.end[t] = self.next_pos;
                    self.hwm = @max(self.hwm, self.next_pos);
                },
                1 => {
                    const b: u8 = @as(u8, 1) << @intCast(t);
                    if (weakening == .no_hold) self.free |= b else self.open |= b;
                },
                else => unreachable,
            }
            self.pc[t] += 1;
        }

        /// The landed prefix advances over reserved windows in order.
        fn stepLander(self: *Self) void {
            const want = self.pc[L] + 1;
            if (self.next_pos < want) return;
            self.floor = want;
            self.pc[L] += 1;
        }

        /// Two barriers: each starts from the landed floor of its start and
        /// publishes it as durable when it completes.
        fn stepBarrier(self: *Self) void {
            switch (self.pc[BAR] % 2) {
                0 => self.bar_snap = self.floor,
                else => self.durable = @max(self.durable, self.bar_snap),
            }
            self.pc[BAR] += 1;
        }

        fn stepReleaser(self: *Self) void {
            const at = self.pc[R] % ROUND_STEPS;
            switch (weakening) {
                .seal_outside_lock => switch (at) {
                    0 => self.r_snap = self.hwm,
                    1 => self.seal(self.r_snap),
                    else => self.release(),
                },
                else => switch (at) {
                    0 => self.seal(self.hwm),
                    else => self.release(),
                },
            }
            self.pc[R] += 1;
        }

        fn seal(self: *Self, mark: u8) void {
            if (self.sealed != 0 or self.open == 0) return;
            self.sealed = self.open;
            self.open = 0;
            self.mark = mark;
        }

        fn release(self: *Self) void {
            const floor = if (weakening == .landed_floor) self.floor else self.durable;
            if (self.sealed == 0 or floor < self.mark) return;
            self.free |= self.sealed;
            self.sealed = 0;
        }

        /// A new owner takes a free block and DMAs into it: the power cut's
        /// hazard instant is the DMA — the old map must be gone by then.
        fn stepOwner(self: *Self) void {
            if (self.free == 0) return;
            const i: u3 = @intCast(@ctz(self.free));
            const b: u8 = @as(u8, 1) << i;
            self.free &= ~b;
            self.dma |= b;
            if (self.durable < self.end[i]) self.violated = 1;
            self.pc[A] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            if (self.violated != 0) return error.ReusedBeforeDurable;
        }

        pub fn final(_: *const Self) !void {}
    };
}

const testing = std.testing;

test "a_held_free_is_reallocated_only_once_its_record_is_durable — holds; the shipped ladder (no hold), a seal outside the hold's lock and a release on the landed floor each fail" {
    try explorer.expectHolds(HoldModel(.none), .{});
    try explorer.expectFails(HoldModel(.no_hold), .{}, error.InvariantViolated);
    try explorer.expectFails(HoldModel(.seal_outside_lock), .{}, error.InvariantViolated);
    try explorer.expectFails(HoldModel(.landed_floor), .{}, error.InvariantViolated);
}

test "state counts: the hold model is small enough to re-run on every gate" {
    var report: explorer.Report = undefined;
    try explorer.explore(HoldModel(.none), testing.allocator, .{}, &report);
    try testing.expect(report.states > 100 and report.states < 200_000);
}
