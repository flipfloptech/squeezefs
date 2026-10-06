//! The per-lane timer heap (charter §9, amended 2026-10-06: "timers … for
//! lane-resident tasks live on the lane's own ring (a per-lane binary heap
//! …)"; `docs/porting/exec.md` §3.6; decision 0003 §D Q-B). Owned by ONE
//! lane thread, so it needs no lock, and bounded by the lane's fiber
//! population: a fiber holds at most one live timer (its park deadline — the
//! 2 s tick of `sync/tick.zig`, a `sleep`, a timed futex wait).
//!
//! Cancel is an INDEXED O(log n) removal (each node carries its heap slot,
//! maintained through every sift) rather than the reference's lazy generation
//! tombstone (`sqz_time.rs`, 0002 ledger): under the ticked-park churn — a
//! deadline armed now + 2 s and cancelled microseconds later on the wake —
//! lazy tombstones sit below the top until their deadline, so the heap would
//! grow by the wake rate × 2 s (200 k entries per lane at 100 k wakes/s)
//! while the indexed removal keeps it at ≤ the fiber population. A lane-local
//! heap has no cancel/fire race to be "cancel-safe" against; the cross-thread
//! `timer_thread.zig` is where tombstones remain.
//!
//! `Interval` carries the cadence semantics (§1.4(g) with the Q10 drift fix):
//! the next deadline is the PREVIOUS DEADLINE + period, never "return +
//! period", so a loop body's duration never slips the cadence; a tick missed
//! by more than a period is delayed (no catch-up burst).

const std = @import("std");
const assert = std.debug.assert;

/// A node's "no slot" marker.
pub const no_slot: u32 = std.math.maxInt(u32);

/// `Node` carries `timer_slot: u32` (this heap's index, `no_slot` when
/// unarmed) and `timer_deadline_ns: u64`.
pub fn TimerHeap(comptime Node: type) type {
    return struct {
        const Self = @This();

        entries: []*Node,
        len: u32 = 0,

        /// `capacity` = the owner's fiber population (one live timer each).
        pub fn init(gpa: std.mem.Allocator, capacity: u32) std.mem.Allocator.Error!Self {
            const entries = try gpa.alloc(*Node, capacity);
            return .{ .entries = entries };
        }

        pub fn deinit(self: *Self, gpa: std.mem.Allocator) void {
            gpa.free(self.entries);
            self.* = undefined;
        }

        pub fn count(self: *const Self) u32 {
            return self.len;
        }

        /// Arm `node` at `deadline_ns`. The node must be unarmed (one timer
        /// per fiber) and the heap has a slot for every fiber by
        /// construction, so neither can fail.
        pub fn arm(self: *Self, node: *Node, deadline_ns: u64) void {
            assert(node.timer_slot == no_slot);
            assert(self.len < self.entries.len);
            node.timer_deadline_ns = deadline_ns;
            const i = self.len;
            self.entries[i] = node;
            node.timer_slot = i;
            self.len += 1;
            self.siftUp(i);
        }

        /// Disarm `node` if armed: an indexed removal. True when it was armed.
        pub fn cancel(self: *Self, node: *Node) bool {
            const i = node.timer_slot;
            if (i == no_slot) return false;
            self.removeAt(i);
            return true;
        }

        /// The earliest deadline, or null when empty: the lane's park bound.
        pub fn peekDeadline(self: *const Self) ?u64 {
            if (self.len == 0) return null;
            return self.entries[0].timer_deadline_ns;
        }

        /// Pop the earliest node whose deadline has passed (`<= now_ns`), or
        /// null. The popped node is unarmed on return.
        pub fn popDue(self: *Self, now_ns: u64) ?*Node {
            if (self.len == 0) return null;
            const top = self.entries[0];
            if (top.timer_deadline_ns > now_ns) return null;
            self.removeAt(0);
            return top;
        }

        fn removeAt(self: *Self, i: u32) void {
            assert(i < self.len);
            const node = self.entries[i];
            assert(node.timer_slot == i);
            node.timer_slot = no_slot;
            self.len -= 1;
            if (i == self.len) return;
            const last = self.entries[self.len];
            self.entries[i] = last;
            last.timer_slot = i;
            // The moved node may belong above or below its new slot.
            if (i > 0 and last.timer_deadline_ns < self.entries[(i - 1) / 2].timer_deadline_ns) {
                self.siftUp(i);
            } else {
                self.siftDown(i);
            }
        }

        fn siftUp(self: *Self, start: u32) void {
            var i = start;
            while (i > 0) {
                const parent = (i - 1) / 2;
                if (self.entries[parent].timer_deadline_ns <= self.entries[i].timer_deadline_ns) return;
                self.swapSlots(parent, i);
                i = parent;
            }
        }

        fn siftDown(self: *Self, start: u32) void {
            var i = start;
            while (true) {
                const l = 2 * i + 1;
                const r = l + 1;
                var smallest = i;
                if (l < self.len and self.entries[l].timer_deadline_ns < self.entries[smallest].timer_deadline_ns) smallest = l;
                if (r < self.len and self.entries[r].timer_deadline_ns < self.entries[smallest].timer_deadline_ns) smallest = r;
                if (smallest == i) return;
                self.swapSlots(i, smallest);
                i = smallest;
            }
        }

        fn swapSlots(self: *Self, a: u32, b: u32) void {
            const na = self.entries[a];
            const nb = self.entries[b];
            self.entries[a] = nb;
            self.entries[b] = na;
            nb.timer_slot = a;
            na.timer_slot = b;
        }
    };
}

/// The cadence: `next` is the deadline of the next tick, advanced by
/// deadline arithmetic (the Q10 fix). Pure over a caller-supplied `now`.
pub const Interval = struct {
    period_ns: u64,
    /// The deadline the next `tick` waits for.
    next_ns: u64,

    /// The first tick fires one full period after `now_ns` (the reference's
    /// `interval` semantics, §1.4(g)).
    pub fn init(now_ns: u64, period_ns: u64) Interval {
        assert(period_ns > 0);
        return .{ .period_ns = period_ns, .next_ns = now_ns + period_ns };
    }

    /// The deadline the caller sleeps until.
    pub fn deadline(self: *const Interval) u64 {
        return self.next_ns;
    }

    /// Advance after a tick landed at `now_ns`: the next deadline is the
    /// PREVIOUS deadline + period (no drift from the loop body). A tick
    /// missed by a whole period or more is delayed: the next deadline is one
    /// period from now, never a catch-up burst of back-to-back ticks.
    pub fn advance(self: *Interval, now_ns: u64) void {
        const next = self.next_ns + self.period_ns;
        self.next_ns = if (next <= now_ns) now_ns + self.period_ns else next;
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

const TestNode = struct {
    id: u32,
    timer_slot: u32 = no_slot,
    timer_deadline_ns: u64 = 0,
};

test "order: popDue yields deadlines in order and only those due; peekDeadline is the park bound" {
    const H = TimerHeap(TestNode);
    var h = try H.init(testing.allocator, 8);
    defer h.deinit(testing.allocator);
    var nodes = [_]TestNode{ .{ .id = 0 }, .{ .id = 1 }, .{ .id = 2 }, .{ .id = 3 }, .{ .id = 4 } };
    const deadlines = [_]u64{ 500, 100, 900, 300, 700 };
    for (&nodes, deadlines) |*n, d| h.arm(n, d);
    try testing.expectEqual(@as(u32, 5), h.count());
    try testing.expectEqual(@as(?u64, 100), h.peekDeadline());
    try testing.expectEqual(@as(?*TestNode, null), h.popDue(99));
    try testing.expectEqual(@as(u32, 1), h.popDue(100).?.id);
    try testing.expectEqual(no_slot, nodes[1].timer_slot);
    try testing.expectEqual(@as(u32, 3), h.popDue(1000).?.id);
    try testing.expectEqual(@as(u32, 0), h.popDue(1000).?.id);
    try testing.expectEqual(@as(u32, 4), h.popDue(1000).?.id);
    try testing.expectEqual(@as(u32, 2), h.popDue(1000).?.id);
    try testing.expectEqual(@as(?*TestNode, null), h.popDue(1000));
    try testing.expectEqual(@as(?u64, null), h.peekDeadline());
    // Every popped node is unarmed and re-armable.
    for (&nodes) |*n| try testing.expectEqual(no_slot, n.timer_slot);
    h.arm(&nodes[2], 5);
    try testing.expectEqual(@as(u32, 2), h.popDue(5).?.id);
}

test "cancel: an indexed removal from the middle keeps the heap ordered; a cancelled timer never fires; cancel of an unarmed node is false" {
    const H = TimerHeap(TestNode);
    var h = try H.init(testing.allocator, 16);
    defer h.deinit(testing.allocator);
    var nodes: [16]TestNode = undefined;
    for (&nodes, 0..) |*n, i| n.* = .{ .id = @intCast(i) };
    // A deliberately unsorted arm order.
    const order = [_]u64{ 1600, 200, 1400, 400, 1200, 600, 1000, 800, 100, 300, 500, 700, 900, 1100, 1300, 1500 };
    for (&nodes, order) |*n, d| h.arm(n, d);
    // Cancel from the middle and the tail; each removal is reflected in the slots.
    try testing.expect(h.cancel(&nodes[4]));
    try testing.expect(h.cancel(&nodes[9]));
    try testing.expect(h.cancel(&nodes[15]));
    try testing.expect(!h.cancel(&nodes[4]));
    try testing.expectEqual(@as(u32, 13), h.count());
    var prev: u64 = 0;
    var popped: u32 = 0;
    while (h.popDue(std.math.maxInt(u64))) |n| {
        try testing.expect(n.timer_deadline_ns > prev);
        try testing.expect(n.id != 4 and n.id != 9 and n.id != 15);
        prev = n.timer_deadline_ns;
        popped += 1;
    }
    try testing.expectEqual(@as(u32, 13), popped);
    // Slots are consistent after every operation.
    for (&nodes) |*n| try testing.expectEqual(no_slot, n.timer_slot);
}

test "cancel-then-rearm storm: random arms and cancels keep every slot consistent and pops ordered" {
    const H = TimerHeap(TestNode);
    var h = try H.init(testing.allocator, 64);
    defer h.deinit(testing.allocator);
    var nodes: [64]TestNode = undefined;
    for (&nodes, 0..) |*n, i| n.* = .{ .id = @intCast(i) };
    var prng = std.Random.DefaultPrng.init(7);
    const rnd = prng.random();
    var round: u32 = 0;
    while (round < 5000) : (round += 1) {
        const n = &nodes[rnd.uintLessThan(usize, nodes.len)];
        if (n.timer_slot == no_slot) h.arm(n, rnd.uintLessThan(u64, 1 << 20)) else _ = h.cancel(n);
        // Every armed node's slot points back at it.
        for (h.entries[0..h.len], 0..) |e, i| try testing.expectEqual(@as(u32, @intCast(i)), e.timer_slot);
    }
    var prev: u64 = 0;
    while (h.popDue(std.math.maxInt(u64))) |n| {
        try testing.expect(n.timer_deadline_ns >= prev);
        prev = n.timer_deadline_ns;
    }
}

test "Interval: deadline arithmetic never drifts with the body; a tick missed by a full period is delayed, not burst" {
    var iv = Interval.init(1_000, 100);
    try testing.expectEqual(@as(u64, 1_100), iv.deadline());
    // The body took 30 of the 100: the next deadline is 1200, not 1230.
    iv.advance(1_130);
    try testing.expectEqual(@as(u64, 1_200), iv.deadline());
    // Late by less than a period: still on the grid.
    iv.advance(1_250);
    try testing.expectEqual(@as(u64, 1_300), iv.deadline());
    // Late by more than a period (a 350-long stall): one period from now,
    // no catch-up burst of 1400/1500.
    iv.advance(1_650);
    try testing.expectEqual(@as(u64, 1_750), iv.deadline());
}
