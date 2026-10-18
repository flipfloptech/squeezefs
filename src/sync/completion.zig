//! The ONE lane-class completion cell (decision 0003 WP-C row 14): the
//! oneshot a caller parks on while a ring-owning lane runs its request. It
//! lives INSIDE a preallocated request cell (the hot path allocates nothing
//! per op — charter §12) and holds no `Io`: the completer is a lane thread
//! parked in its ring. Two wait faces over the one word:
//!   - raw (`wait`, `complete`): the caller thread parks on the raw futex
//!     (`sync/futex.zig`) — a lane thread's own cells, bare devices;
//!   - routed (`waitIo`, `completeIo`): both sides go through an `Io`'s
//!     futex ops — under `SqzIo` a fiber waiter parks in the executor's
//!     futex table (its lane thread runs other fibers meanwhile) and a raw
//!     waiter takes the counted kernel wait (decision 0001 amendment
//!     2026-10-18: a device DMA parks only the fiber that issued it). The
//!     `Io` is the caller's, passed per call, never stored here; a cell's
//!     two sides must use the same face.
//! `sync.Oneshot` (`channel.zig`) is the task-class channel for `Io` code;
//! it is not this.
//!
//! The three-state word is the protocol a timed-out or dropped waiter needs:
//!
//!     pending ──complete──▶ done        (waiter owns the cell; wake it)
//!     pending ──abandon───▶ abandoned   (completer frees the cell at the CQE)
//!
//! Exactly one side wins the CAS out of `pending`, so a cell is freed by
//! exactly one party and a value is never read after its owner left.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const futex = @import("futex.zig");
const clock = @import("../core/clock.zig");

pub fn Completion(comptime T: type) type {
    return struct {
        const Self = @This();

        pub const pending: u32 = 0;
        pub const done: u32 = 1;
        pub const abandoned: u32 = 2;

        state: futex.Word = .init(pending),
        value: T = undefined,

        /// Back to `pending` — the owner's reset before the cell is reused.
        /// Never racy: a cell is reset only while no party references it.
        pub fn reset(self: *Self) void {
            self.state.store(pending, .monotonic);
        }

        /// Completer side. True ⇒ the waiter is still there and now owns the
        /// cell (it was woken); false ⇒ the waiter abandoned: the completer
        /// owns the cell and must release what it holds. The release CAS
        /// publishes `value`; the acquire on failure sees the abandonment
        /// before the completer frees the waiter's resources.
        pub fn complete(self: *Self, value: T) bool {
            self.value = value;
            if (self.state.cmpxchgStrong(pending, done, .release, .acquire) == null) {
                futex.wakeAll(&self.state);
                return true;
            }
            return false;
        }

        /// Non-blocking: the value once published (acquire pairs with the
        /// completer's release CAS), else null.
        pub fn poll(self: *const Self) ?T {
            if (self.state.load(.acquire) != done) return null;
            return self.value;
        }

        pub fn isDone(self: *const Self) bool {
            return self.state.load(.acquire) == done;
        }

        /// Park until the value is published.
        pub fn wait(self: *Self) T {
            while (self.state.load(.acquire) == pending) futex.wait(&self.state, pending);
            return self.value;
        }

        /// The routed completer: the same release CAS, the wake through
        /// `io`'s futex face (under `SqzIo`: the fiber waiters of the
        /// word's bucket, then the kernel wake iff a raw waiter is
        /// counted — `exec/futex_table.zig`'s Dekker). Pairs with `waitIo`.
        pub fn completeIo(self: *Self, io: std.Io, value: T) bool {
            self.value = value;
            if (self.state.cmpxchgStrong(pending, done, .release, .acquire) == null) {
                futex.ioWake(io, &self.state, std.math.maxInt(i32));
                return true;
            }
            return false;
        }

        /// The routed waiter: park through `io`'s futex face until the
        /// value is published — a fiber parks alone, a raw thread is
        /// counted for the completer's kernel wake. Uncancelable: the
        /// completer owns what the cell names until it publishes, so the
        /// waiter never leaves early.
        pub fn waitIo(self: *Self, io: std.Io) T {
            while (self.state.load(.acquire) == pending) io.futexWaitUncancelable(u32, &self.state.raw, pending);
            return self.value;
        }

        /// `wait` bounded by `timeout_ns`. Null ⇒ the deadline passed and the
        /// cell is ABANDONED to the completer (the waiter must not touch it
        /// again); a completion that raced the deadline is still taken.
        pub fn waitTimeout(self: *Self, timeout_ns: u64) ?T {
            const deadline = clock.monotonicNs() + timeout_ns;
            while (true) {
                if (self.state.load(.acquire) == done) return self.value;
                const now = clock.monotonicNs();
                if (now >= deadline) return if (self.abandon()) null else self.value;
                _ = futex.waitTimeout(&self.state, pending, deadline - now);
            }
        }

        /// Waiter side, leaving without the value. True ⇒ abandoned (the
        /// completer frees the cell when it completes); false ⇒ the value was
        /// already published: the waiter still owns the cell and frees it.
        pub fn abandon(self: *Self) bool {
            return self.state.cmpxchgStrong(pending, abandoned, .acq_rel, .acquire) == null;
        }
    };
}

const testing = std.testing;
const builtin = @import("builtin");

test "complete wakes a parked waiter and hands it the value; poll sees it; reset rearms" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "completion: Linux only");
    const C = Completion(u32);
    var c: C = .{};
    try testing.expectEqual(@as(?u32, null), c.poll());
    const Completer = struct {
        fn run(cell: *C) void {
            std.debug.assert(cell.complete(42));
        }
    };
    const t = try std.Thread.spawn(.{}, Completer.run, .{&c});
    try testing.expectEqual(@as(u32, 42), c.wait());
    t.join();
    try testing.expectEqual(@as(?u32, 42), c.poll());
    try testing.expect(c.isDone());
    c.reset();
    try testing.expect(!c.isDone());
    // A value published before the wait returns at once.
    try testing.expect(c.complete(7));
    try testing.expectEqual(@as(u32, 7), c.wait());
    try testing.expectEqual(@as(u32, 7), c.waitTimeout(0).?);
}

test "abandon: exactly one side owns the cell — the completer when the waiter left first, the waiter otherwise" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "completion: Linux only");
    const C = Completion(u32);
    var a: C = .{};
    try testing.expect(a.abandon());
    // The completer finds the cell abandoned: it owns the cleanup.
    try testing.expect(!a.complete(1));
    var b: C = .{};
    try testing.expect(b.complete(2));
    // The waiter arrives late: the value is there, the cell is still its own.
    try testing.expect(!b.abandon());
    try testing.expectEqual(@as(?u32, 2), b.poll());
    // A timed wait that expires abandons; one that is answered in time does not.
    var d: C = .{};
    try testing.expectEqual(@as(?u32, null), d.waitTimeout(2 * std.time.ns_per_ms));
    try testing.expect(!d.complete(3));
    var e: C = .{};
    const Late = struct {
        fn run(cell: *C) void {
            std.debug.assert(cell.complete(4));
        }
    };
    const t = try std.Thread.spawn(.{}, Late.run, .{&e});
    try testing.expectEqual(@as(?u32, 4), e.waitTimeout(10 * std.time.ns_per_s));
    t.join();
}

fn routedFace(io: std.Io) !void {
    const C = Completion(u32);
    // A raw waiter (the test thread) through the routed face: the
    // completer's wake reaches the counted kernel wait.
    var a: C = .{};
    const Completer = struct {
        fn run(io_: std.Io, cell: *C, v: u32) void {
            std.debug.assert(cell.completeIo(io_, v));
        }
    };
    const t = try std.Thread.spawn(.{}, Completer.run, .{ io, &a, 11 });
    try testing.expectEqual(@as(u32, 11), a.waitIo(io));
    t.join();
    // A task waiter (under SqzIo a fiber on a meta lane: it parks in the
    // futex table and its lane thread is free) completed from a raw thread.
    var b: C = .{};
    const Waiter = struct {
        fn run(io_: std.Io, cell: *C) u32 {
            return cell.waitIo(io_);
        }
    };
    var fut = io.concurrent(Waiter.run, .{ io, &b }) catch return testkit.skip(@src(), .no_concurrency, "completion: no concurrency");
    const t2 = try std.Thread.spawn(.{}, Completer.run, .{ io, &b, 12 });
    try testing.expectEqual(@as(u32, 12), fut.await(io));
    t2.join();
    // A value published before the routed wait returns at once; the
    // abandon arbitration is the same word.
    var c: C = .{};
    try testing.expect(c.completeIo(io, 13));
    try testing.expectEqual(@as(u32, 13), c.waitIo(io));
    try testing.expect(!c.abandon());
    var d: C = .{};
    try testing.expect(d.abandon());
    try testing.expect(!d.completeIo(io, 14));
}

test "routed face: completeIo wakes a waitIo waiter under both executors — a raw thread and a task (a lane fiber under SqzIo)" {
    try @import("testing.zig").underBoth(routedFace);
}

test "a parked waiter and a racing completer never both free: 64 rounds of CAS arbitration" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "completion: Linux only");
    const C = Completion(u32);
    var cells: [64]C = @splat(.{});
    var completer_owns: std.atomic.Value(u32) = .init(0);
    var waiter_owns: std.atomic.Value(u32) = .init(0);
    const Completer = struct {
        fn run(cs: *[64]C, owns: *std.atomic.Value(u32)) void {
            for (cs) |*c| if (!c.complete(1)) {
                _ = owns.fetchAdd(1, .monotonic);
            };
        }
    };
    const t = try std.Thread.spawn(.{}, Completer.run, .{ &cells, &completer_owns });
    for (&cells) |*c| if (!c.abandon()) {
        _ = waiter_owns.fetchAdd(1, .monotonic);
    };
    t.join();
    try testing.expectEqual(@as(u32, 64), completer_owns.load(.monotonic) + waiter_owns.load(.monotonic));
}
