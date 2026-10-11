//! The ONE bounded MPMC queue (Vyukov's cell-sequence ring; decision 0003
//! WP-C row 15): the caller → lane channel of every ring-owning lane
//! (`uring_fs`, `nvme_dev`, `journal_lane`) and their pool free-lists.
//!
//! Producers `tryPush` and consumers `tryPop`; neither ever blocks. Parking
//! is NOT the queue's business: a lane parks in its ring (`io/ring_park.zig`,
//! woken through the eventfd), a pool worker on its own futex word — the
//! queue is the same either way. Closing is the owner's protocol too (a
//! SeqCst flag plus an in-flight census above the queue), so a producer that
//! observed "open" and pushed after the last drain is always answered by the
//! owner's post-join sweep.

const std = @import("std");
const derive = @import("../core/derive.zig");

pub fn BoundedMpmc(comptime T: type) type {
    return struct {
        const Self = @This();

        const Cell = struct {
            /// Vyukov sequence: `== pos` ⇒ free for the producer at `pos`;
            /// `== pos + 1` ⇒ filled for the consumer at `pos`.
            seq: std.atomic.Value(usize),
            data: T,
        };

        cells: []Cell,
        mask: usize,
        enqueue_pos: std.atomic.Value(usize) align(std.atomic.cache_line) = .init(0),
        dequeue_pos: std.atomic.Value(usize) align(std.atomic.cache_line) = .init(0),

        /// `cap` is rounded up to a power of two (the Vyukov mask); 2 is the
        /// smallest ring whose sequence arithmetic distinguishes full from empty.
        pub fn init(gpa: std.mem.Allocator, cap: usize) std.mem.Allocator.Error!Self {
            const n: usize = @intCast(derive.nextPow2(@max(cap, 2)));
            const cells = try gpa.alloc(Cell, n);
            for (cells, 0..) |*c, i| c.* = .{ .seq = .init(i), .data = undefined };
            return .{ .cells = cells, .mask = n - 1 };
        }

        pub fn deinit(self: *Self, gpa: std.mem.Allocator) void {
            gpa.free(self.cells);
        }

        pub fn capacity(self: *const Self) usize {
            return self.cells.len;
        }

        /// False when full. The data store is published by the release
        /// store of `seq`, paired with the consumer's acquire load.
        pub fn tryPush(self: *Self, item: T) bool {
            var pos = self.enqueue_pos.load(.monotonic);
            while (true) {
                const cell = &self.cells[pos & self.mask];
                const seq = cell.seq.load(.acquire);
                const diff = @as(isize, @bitCast(seq)) -% @as(isize, @bitCast(pos));
                if (diff == 0) {
                    if (self.enqueue_pos.cmpxchgWeak(pos, pos +% 1, .monotonic, .monotonic)) |actual| {
                        pos = actual;
                        continue;
                    }
                    cell.data = item;
                    cell.seq.store(pos +% 1, .release);
                    return true;
                } else if (diff < 0) {
                    return false;
                } else {
                    pos = self.enqueue_pos.load(.monotonic);
                }
            }
        }

        /// A push that MUST land: the owner's ring holds its whole population
        /// (a pool free list), so a refusal is never "full" — it is the
        /// wrap-around cell of a consumer that CAS'd `dequeue_pos` and has
        /// not yet stored the cell's new sequence (Vyukov's producer reads
        /// `seq < pos` and answers full). A second consumer on such a ring
        /// (the idle trimmer beside the spawner) makes that window real:
        /// 2,874 refusals in 400 k pop→push cycles on an 8-slot ring in the
        /// test below. The popper finishes within nanoseconds; spin.
        pub fn pushSpin(self: *Self, item: T) void {
            while (!self.tryPush(item)) std.atomic.spinLoopHint();
        }

        /// Null when empty. The release store of `seq` hands the cell back
        /// to the producer at `pos + cap`, paired with its acquire load.
        pub fn tryPop(self: *Self) ?T {
            var pos = self.dequeue_pos.load(.monotonic);
            while (true) {
                const cell = &self.cells[pos & self.mask];
                const seq = cell.seq.load(.acquire);
                const diff = @as(isize, @bitCast(seq)) -% @as(isize, @bitCast(pos +% 1));
                if (diff == 0) {
                    if (self.dequeue_pos.cmpxchgWeak(pos, pos +% 1, .monotonic, .monotonic)) |actual| {
                        pos = actual;
                        continue;
                    }
                    const item = cell.data;
                    cell.seq.store(pos +% self.mask +% 1, .release);
                    return item;
                } else if (diff < 0) {
                    return null;
                } else {
                    pos = self.dequeue_pos.load(.monotonic);
                }
            }
        }
    };
}

const testing = std.testing;

test "bounded MPMC queue: FIFO, full refuses, wraps, empty is null" {
    const Q = BoundedMpmc(u32);
    var q = try Q.init(testing.allocator, 4);
    defer q.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 4), q.capacity());
    try testing.expect(q.tryPush(1));
    try testing.expect(q.tryPush(2));
    try testing.expect(q.tryPush(3));
    try testing.expect(q.tryPush(4));
    try testing.expect(!q.tryPush(5));
    try testing.expectEqual(@as(?u32, 1), q.tryPop());
    try testing.expect(q.tryPush(5));
    try testing.expectEqual(@as(?u32, 2), q.tryPop());
    try testing.expectEqual(@as(?u32, 3), q.tryPop());
    try testing.expectEqual(@as(?u32, 4), q.tryPop());
    try testing.expectEqual(@as(?u32, 5), q.tryPop());
    try testing.expectEqual(@as(?u32, null), q.tryPop());
    // Many wraps keep FIFO exact.
    var i: u32 = 0;
    while (i < 1000) : (i += 1) {
        try testing.expect(q.tryPush(i));
        try testing.expectEqual(@as(?u32, i), q.tryPop());
    }
    // A sub-2 capacity rounds up to the smallest Vyukov ring.
    var tiny = try Q.init(testing.allocator, 1);
    defer tiny.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 2), tiny.capacity());
}

test "bounded MPMC queue: 8 producers × 1 consumer deliver every item exactly once" {
    const Q = BoundedMpmc(u32);
    const producers = 8;
    const per = 2000;
    var q = try Q.init(testing.allocator, 256);
    defer q.deinit(testing.allocator);
    const Producer = struct {
        fn run(queue: *Q, base: u32) void {
            var i: u32 = 0;
            while (i < per) {
                if (queue.tryPush(base + i)) i += 1 else std.atomic.spinLoopHint();
            }
        }
    };
    var threads: [producers]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Producer.run, .{ &q, @as(u32, @intCast(i * per)) });
    var seen = try testing.allocator.alloc(bool, producers * per);
    defer testing.allocator.free(seen);
    @memset(seen, false);
    var got: usize = 0;
    while (got < producers * per) {
        if (q.tryPop()) |v| {
            try testing.expect(!seen[v]);
            seen[v] = true;
            got += 1;
        } else std.atomic.spinLoopHint();
    }
    for (threads) |t| t.join();
    for (seen) |s| try testing.expect(s);
    try testing.expectEqual(@as(?u32, null), q.tryPop());
}

test "bounded MPMC queue: 4 producers × 4 consumers deliver every item exactly once" {
    const Q = BoundedMpmc(u32);
    const producers = 4;
    const consumers = 4;
    const per = 2000;
    var q = try Q.init(testing.allocator, 64);
    defer q.deinit(testing.allocator);
    const seen = try testing.allocator.alloc(std.atomic.Value(u8), producers * per);
    defer testing.allocator.free(seen);
    for (seen) |*s| s.* = .init(0);
    var consumed: std.atomic.Value(usize) = .init(0);
    const Producer = struct {
        fn run(queue: *Q, base: u32) void {
            var i: u32 = 0;
            while (i < per) {
                if (queue.tryPush(base + i)) i += 1 else std.atomic.spinLoopHint();
            }
        }
    };
    const Consumer = struct {
        fn run(queue: *Q, marks: []std.atomic.Value(u8), total: *std.atomic.Value(usize)) void {
            while (total.load(.acquire) < producers * per) {
                if (queue.tryPop()) |v| {
                    // Exactly once: a second delivery of `v` would read 1 here.
                    std.debug.assert(marks[v].swap(1, .acq_rel) == 0);
                    _ = total.fetchAdd(1, .acq_rel);
                } else std.atomic.spinLoopHint();
            }
        }
    };
    var ps: [producers]std.Thread = undefined;
    var cs: [consumers]std.Thread = undefined;
    for (&cs) |*t| t.* = try std.Thread.spawn(.{}, Consumer.run, .{ &q, seen, &consumed });
    for (&ps, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Producer.run, .{ &q, @as(u32, @intCast(i * per)) });
    for (ps) |t| t.join();
    for (cs) |t| t.join();
    for (seen) |*s| try testing.expectEqual(@as(u8, 1), s.load(.acquire));
    try testing.expectEqual(@as(?u32, null), q.tryPop());
}

test "a pool free list at its population: two threads cycling pop → push over a ring that holds EVERY slot — the push back of a popped slot can meet the ring's own cell still mid-pop and be refused; a free-list owner must retry, never assert" {
    // 8 slots in an 8-cell ring (the exec lanes' and the fill pools' shape).
    const Q = BoundedMpmc(u32);
    var q = try Q.init(testing.allocator, 8);
    defer q.deinit(testing.allocator);
    var i: u32 = 0;
    while (i < 8) : (i += 1) try testing.expect(q.tryPush(i));
    const Cycler = struct {
        refused: std.atomic.Value(u64) = .init(0),
        fn run(self: *@This(), queue: *Q, rounds: u32) void {
            var r: u32 = 0;
            while (r < rounds) : (r += 1) {
                const v = queue.tryPop() orelse continue;
                if (!queue.tryPush(v)) {
                    _ = self.refused.fetchAdd(1, .monotonic);
                    queue.pushSpin(v);
                }
            }
        }
    };
    var a: Cycler = .{};
    var b: Cycler = .{};
    const ta = try std.Thread.spawn(.{}, Cycler.run, .{ &a, &q, 200_000 });
    const tb = try std.Thread.spawn(.{}, Cycler.run, .{ &b, &q, 200_000 });
    ta.join();
    tb.join();
    // Nothing lost: all eight slots are back.
    var n: u32 = 0;
    while (q.tryPop()) |_| n += 1;
    try testing.expectEqual(@as(u32, 8), n);
    // The refusals are the point of the test, not a flake: they must occur.
    try testing.expect(a.refused.load(.monotonic) + b.refused.load(.monotonic) > 0);
}
