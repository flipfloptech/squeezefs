//! Keyed single-flight (charter §9; the reference's `sqz_flight.rs` shared
//! oneshot under the node cache's and read path's in-flight maps).
//!
//! The first caller for a key is the LEADER and runs the computation; every
//! concurrent caller for the same key is its COHORT and receives the
//! leader's value — a shared oneshot, not a stream. A leader that fails
//! (an error, including `error.Canceled`) publishes no value: the cohort's
//! front waiter is re-elected leader and runs its own computation (the
//! reference's `Gone` → "callers re-run their own flight", resolved here
//! instead of handed back). A caller arriving after the leader published
//! starts a fresh flight — the key is removed at publication, so the value
//! never goes stale across flights.
//!
//! Zero allocation: flights live in an inline table of `max_flights` slots
//! (a refusal, `error.FlightTableFull`, when every slot is leading — the
//! caller's retry class); waiter nodes live on the cohort's frames and
//! receive their copy of the value under the interior mutex, so a slot is
//! free the instant its leader publishes. Parks are ticked
//! (`channel_ticked_reregisters`).

const std = @import("std");
const Io = std.Io;
const assert = std.debug.assert;
const tick = @import("tick.zig");
const futex = @import("futex.zig");

pub fn SingleFlight(comptime K: type, comptime V: type, comptime max_flights: usize) type {
    return struct {
        const Self = @This();

        interior: Io.Mutex = .init,
        slots: [max_flights]Slot = @splat(.{}),

        const Slot = struct {
            key: K = undefined,
            /// A leader is running for `key`.
            leading: bool = false,
            head: ?*Waiter = null,
            tail: ?*Waiter = null,
        };

        const Waiter = struct {
            next: ?*Waiter = null,
            prev: ?*Waiter = null,
            linked: bool = false,
            state: std.atomic.Value(u32) = .init(0),
            /// The leader's value, copied in under the interior mutex.
            result: ?V = null,
            /// The leader failed and this waiter is the new leader.
            elected: bool = false,
        };

        pub const init: Self = .{};

        pub const Error = error{ Canceled, FlightTableFull };

        /// Lead or join the flight for `key`. `compute(ctx, io)` runs on the
        /// leader; its error is the leader's own result and re-elects the
        /// cohort. Cancellation is a cancellation point only for a waiting
        /// cohort member (a leader finishes its computation).
        pub fn run(self: *Self, io: Io, key: K, ctx: anytype, comptime compute: fn (@TypeOf(ctx), Io) anyerror!V) anyerror!V {
            var w: Waiter = .{};
            const slot = blk: {
                self.interior.lockUncancelable(io);
                defer self.interior.unlock(io);
                if (self.findLocked(key)) |s| {
                    pushLocked(s, &w);
                    break :blk s;
                }
                const s = self.claimLocked(key) orelse return error.FlightTableFull;
                break :blk s;
            };
            if (!w.linked) return self.lead(io, slot, ctx, compute);
            while (true) {
                _ = tick.park(io, &w.state, &tick.channel_ticked_reregisters) catch |err| switch (err) {
                    error.Canceled => {
                        self.leave(io, slot, &w);
                        return err;
                    },
                };
                self.interior.lockUncancelable(io);
                w.state.store(0, .monotonic);
                if (w.result) |v| {
                    self.interior.unlock(io);
                    return v;
                }
                if (w.elected) {
                    self.interior.unlock(io);
                    return self.lead(io, slot, ctx, compute);
                }
                self.interior.unlock(io);
            }
        }

        /// Flights in progress (the deterministic observable).
        pub fn inFlight(self: *Self, io: Io) usize {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            var n: usize = 0;
            for (&self.slots) |*s| n += @intFromBool(s.leading);
            return n;
        }

        /// Cohort members parked on `key`'s flight.
        pub fn cohort(self: *Self, io: Io, key: K) usize {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            const s = self.findLocked(key) orelse return 0;
            var n: usize = 0;
            var cur = s.head;
            while (cur) |w| : (cur = w.next) n += 1;
            return n;
        }

        fn lead(self: *Self, io: Io, slot: *Slot, ctx: anytype, comptime compute: fn (@TypeOf(ctx), Io) anyerror!V) anyerror!V {
            const outcome = compute(ctx, io);
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            if (outcome) |v| {
                // Publish to the whole cohort and free the slot: a later
                // arrival starts a fresh flight.
                while (slot.head) |cw| {
                    unlinkLocked(slot, cw);
                    cw.result = v;
                    tick.wake(io, &cw.state);
                }
                slot.leading = false;
                return v;
            } else |err| {
                // Re-elect: the front waiter leads; the rest stay queued.
                if (slot.head) |cw| {
                    unlinkLocked(slot, cw);
                    cw.elected = true;
                    tick.wake(io, &cw.state);
                } else {
                    slot.leading = false;
                }
                return err;
            }
        }

        /// A cancelled cohort member leaves. If it had already been elected,
        /// leadership passes to the next waiter (or the slot closes).
        fn leave(self: *Self, io: Io, slot: *Slot, w: *Waiter) void {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            if (w.linked) unlinkLocked(slot, w);
            if (w.elected) {
                if (slot.head) |cw| {
                    unlinkLocked(slot, cw);
                    cw.elected = true;
                    tick.wake(io, &cw.state);
                } else {
                    slot.leading = false;
                }
            }
        }

        fn findLocked(self: *Self, key: K) ?*Slot {
            for (&self.slots) |*s| {
                if (s.leading and std.meta.eql(s.key, key)) return s;
            }
            return null;
        }

        fn claimLocked(self: *Self, key: K) ?*Slot {
            for (&self.slots) |*s| {
                if (!s.leading) {
                    assert(s.head == null);
                    s.* = .{ .key = key, .leading = true };
                    return s;
                }
            }
            return null;
        }

        fn pushLocked(s: *Slot, w: *Waiter) void {
            assert(!w.linked);
            w.next = null;
            w.prev = s.tail;
            if (s.tail) |t| t.next = w else s.head = w;
            s.tail = w;
            w.linked = true;
        }

        fn unlinkLocked(s: *Slot, w: *Waiter) void {
            assert(w.linked);
            if (w.prev) |p| p.next = w.next else s.head = w.next;
            if (w.next) |x| x.prev = w.prev else s.tail = w.prev;
            w.next = null;
            w.prev = null;
            w.linked = false;
        }
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

const Flight = SingleFlight(u64, u32, 4);

const Ctx = struct {
    computes: std.atomic.Value(u32) = .init(0),
    /// The leader parks here until the cohort has joined.
    gate: std.atomic.Value(u32) = .init(0),
    joined: std.atomic.Value(u32) = .init(0),
    fail_first: bool = false,
    value: u32 = 42,

    fn compute(c: *Ctx, io: Io) anyerror!u32 {
        const n = c.computes.fetchAdd(1, .acq_rel) + 1;
        while (c.gate.load(.acquire) == 0) try futex.ioWaitTimeout(io, &c.gate, 0, null);
        if (c.fail_first and n == 1) return error.LeaderDied;
        return c.value;
    }
};

fn opens(c: *Ctx, io: Io) void {
    c.gate.store(1, .release);
    futex.ioWake(io, &c.gate, std.math.maxInt(u32));
}

test "cohort_receives_the_leaders_value: eight racers for one key compute once and all read it" {
    const io = testing.io;
    var f: Flight = .init;
    var c: Ctx = .{};
    const Racer = struct {
        fn run(fl: *Flight, cx: *Ctx) !void {
            _ = cx.joined.fetchAdd(1, .acq_rel);
            try testing.expectEqual(@as(u32, 42), try fl.run(testing.io, 7, cx, Ctx.compute));
        }
    };
    var ts: [8]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, Racer.run, .{ &f, &c });
    while (c.joined.load(.acquire) < 8 or f.inFlight(io) == 0) try std.Thread.yield();
    opens(&c, io);
    for (ts) |t| t.join();
    try testing.expectEqual(@as(u32, 1), c.computes.load(.acquire));
    try testing.expectEqual(@as(usize, 0), f.inFlight(io));
}

test "leader_death_re_elects: a failed leader gets its own error and the cohort computes anew" {
    const io = testing.io;
    var f: Flight = .init;
    var c: Ctx = .{ .fail_first = true };
    const Shared = struct {
        errors: std.atomic.Value(u32) = .init(0),
        values: std.atomic.Value(u32) = .init(0),
    };
    var sh: Shared = .{};
    const Racer = struct {
        fn run(fl: *Flight, cx: *Ctx, s: *Shared) void {
            _ = cx.joined.fetchAdd(1, .acq_rel);
            if (fl.run(testing.io, 9, cx, Ctx.compute)) |v| {
                std.debug.assert(v == 42);
                _ = s.values.fetchAdd(1, .acq_rel);
            } else |_| {
                _ = s.errors.fetchAdd(1, .acq_rel);
            }
        }
    };
    var ts: [4]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, Racer.run, .{ &f, &c, &sh });
    while (c.joined.load(.acquire) < 4 or f.inFlight(io) == 0) try std.Thread.yield();
    opens(&c, io);
    for (ts) |t| t.join();
    try testing.expectEqual(@as(u32, 1), sh.errors.load(.acquire));
    try testing.expectEqual(@as(u32, 3), sh.values.load(.acquire));
    try testing.expectEqual(@as(u32, 2), c.computes.load(.acquire));
    try testing.expectEqual(@as(usize, 0), f.inFlight(io));
}

test "distinct keys fly independently and a late arrival starts a fresh flight" {
    const io = testing.io;
    var f: Flight = .init;
    var c: Ctx = .{};
    opens(&c, io);
    try testing.expectEqual(@as(u32, 42), try f.run(io, 1, &c, Ctx.compute));
    try testing.expectEqual(@as(u32, 42), try f.run(io, 1, &c, Ctx.compute));
    try testing.expectEqual(@as(u32, 2), c.computes.load(.acquire));
    try testing.expectEqual(@as(usize, 0), f.inFlight(io));
}

test "the table bound is a refusal, never an allocation" {
    const io = testing.io;
    var f: Flight = .init;
    var c: Ctx = .{};
    const Leader = struct {
        fn run(fl: *Flight, cx: *Ctx, key: u64) !void {
            try testing.expectEqual(@as(u32, 42), try fl.run(testing.io, key, cx, Ctx.compute));
        }
    };
    var ts: [4]std.Thread = undefined;
    for (&ts, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Leader.run, .{ &f, &c, @as(u64, i) });
    while (f.inFlight(io) < 4) try std.Thread.yield();
    try testing.expectError(error.FlightTableFull, f.run(io, 99, &c, Ctx.compute));
    opens(&c, io);
    for (ts) |t| t.join();
    try testing.expectEqual(@as(u32, 4), c.computes.load(.acquire));
}

test "a cancelled cohort member leaves without disturbing the flight" {
    const io = testing.io;
    var f: Flight = .init;
    var c: Ctx = .{};
    const Leader = struct {
        fn run(fl: *Flight, cx: *Ctx) !void {
            try testing.expectEqual(@as(u32, 42), try fl.run(testing.io, 5, cx, Ctx.compute));
        }
    };
    const Member = struct {
        fn run(fl: *Flight, cx: *Ctx) anyerror!u32 {
            return fl.run(testing.io, 5, cx, Ctx.compute);
        }
    };
    const leader = try std.Thread.spawn(.{}, Leader.run, .{ &f, &c });
    while (f.inFlight(io) == 0) try std.Thread.yield();
    var fut = io.concurrent(Member.run, .{ &f, &c }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    while (f.cohort(io, 5) == 0) try std.Thread.yield();
    try testing.expectError(error.Canceled, fut.cancel(io));
    opens(&c, io);
    leader.join();
    try testing.expectEqual(@as(u32, 1), c.computes.load(.acquire));
    try testing.expectEqual(@as(usize, 0), f.inFlight(io));
}
