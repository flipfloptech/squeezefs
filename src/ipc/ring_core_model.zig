//! Exhaustive model of the submission ring (`ring_core.zig`): two producers
//! racing into a capacity-2 ring with one consumer, across the cell-0 lap
//! boundary. Holds: nothing lost, nothing duplicated, FIFO per producer,
//! never more than `capacity` reserved. The recorded WEAKENING: the value
//! store lands AFTER the publishing seq store (the publishing Release →
//! Relaxed shape, modelled as the two stores swapped) — the consumer pops
//! the seeded phantom 0 on the first lap, the previous lap's value after.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const CAP: u8 = 2;
const MASK: u8 = CAP - 1;

pub fn Ring(comptime value_after_publish: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const consumer = 2;

        tail: u8 = 0,
        seq: [CAP]u8 = .{ 0, 1 },
        value: [CAP]u8 = .{ 0, 0 },
        head: u8 = 0,
        /// Per-producer: the position it reserved; the loaded seq.
        pos: [2]u8 = @splat(0),
        seen_seq: [2]u8 = @splat(0),
        /// Each producer pushes values 1..=2 (producer 0: 1,2; producer 1: 3,4).
        pushed: [2]u8 = @splat(0),
        popped: [4]u8 = @splat(0),
        popped_n: u8 = 0,
        /// Model bookkeeping: the value a producer stored for position p.
        stored_for_pos: [4]u8 = @splat(0),
        /// A pop that read a value nobody stored for its position (the
        /// seeded 0 on lap 0, the previous lap's value after).
        phantom: bool = false,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t == consumer) self.popped_n == 4 else self.pushed[t] == 2;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == consumer) self.stepConsumer() else self.stepProducer(t);
        }
        fn stepProducer(self: *Self, t: usize) void {
            switch (self.pc[t]) {
                0 => {
                    // tail load + cell seq load (the seq is the atomic action;
                    // the tail read is folded in: a stale tail only retries).
                    self.pos[t] = self.tail;
                    self.seen_seq[t] = self.seq[self.pos[t] & MASK];
                    self.pc[t] = 1;
                },
                1 => {
                    const dif: i8 = @bitCast(self.seen_seq[t] -% self.pos[t]);
                    if (dif == 0) {
                        // CAS tail → tail + 1.
                        if (self.tail == self.pos[t]) {
                            self.tail +%= 1;
                            self.pc[t] = 2;
                        } else self.pc[t] = 0;
                    } else if (dif < 0) {
                        // Full: retry later (a bounded producer re-reads).
                        self.pc[t] = 0;
                    } else self.pc[t] = 0;
                },
                2 => {
                    if (!value_after_publish) self.storeValue(t) else self.publish(t);
                    self.pc[t] = 3;
                },
                3 => {
                    if (!value_after_publish) self.publish(t) else self.storeValue(t);
                    self.pushed[t] += 1;
                    self.pc[t] = 0;
                },
                else => unreachable,
            }
        }
        fn storeValue(self: *Self, t: usize) void {
            const v = @as(u8, @intCast(t)) * 2 + self.pushed[t] + 1;
            self.value[self.pos[t] & MASK] = v;
            self.stored_for_pos[self.pos[t]] = v;
        }
        fn publish(self: *Self, t: usize) void {
            self.seq[self.pos[t] & MASK] = self.pos[t] +% 1;
        }
        fn stepConsumer(self: *Self) void {
            // pop: seq load; if published, value load + free (one action
            // each — the value load is the observable).
            switch (self.pc[consumer]) {
                0 => {
                    if (self.seq[self.head & MASK] == self.head +% 1) self.pc[consumer] = 1;
                },
                1 => {
                    const v = self.value[self.head & MASK];
                    if (v != self.stored_for_pos[self.head]) self.phantom = true;
                    self.popped[self.popped_n] = v;
                    self.popped_n += 1;
                    self.pc[consumer] = 2;
                },
                2 => {
                    self.seq[self.head & MASK] = self.head +% CAP;
                    self.head +%= 1;
                    self.pc[consumer] = 0;
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            // Never more than CAP reserved ahead of the consumer.
            if (self.tail -% self.head > CAP) return error.ReservedPastCapacity;
            // No phantom: every popped value is the one stored for its position.
            if (self.phantom) return error.PhantomPopped;
        }
        pub fn final(self: *const Self) !void {
            // Exactly once, and each producer's two values in its order.
            var seen: [5]bool = @splat(false);
            var last: [2]u8 = @splat(0);
            for (self.popped) |v| {
                if (seen[v]) return error.Duplicated;
                seen[v] = true;
                const p: usize = (v - 1) / 2;
                if (v <= last[p]) return error.FifoBroken;
                last[p] = v;
            }
            for (seen[1..]) |s| if (!s) return error.Lost;
        }
    };
}

test "ring: two producers + one consumer through capacity 2 across the lap boundary — never lost, duplicated or over-reserved, FIFO per producer; the value-after-publish weakening pops a phantom" {
    try explorer.expectHolds(Ring(false), .{ .max_depth = 512 });
    try explorer.expectFails(Ring(true), .{ .max_depth = 512 }, error.InvariantViolated);
}
