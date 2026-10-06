//! The async once-cell with cancelled-leader re-election (charter §9; the
//! reference's `sqz_once.rs`): one leader runs the initializer, concurrent
//! callers park on a `Notify`, and a leader whose initializer fails or is
//! cancelled releases the leadership latch so a parked caller re-elects
//! (tokio parity). Losers park enable-then-recheck, so a completion landing
//! between the check and the park is never lost; parks are ticked.
//!
//! Reads after initialization are lock-free: `ready` is published with a
//! release store the readers' acquire load pairs with.

const std = @import("std");
const Io = std.Io;
const Notify = @import("notify.zig").Notify;
const futex = @import("futex.zig");

pub fn OnceCell(comptime T: type) type {
    return struct {
        const Self = @This();

        value: T = undefined,
        /// release-published by the initializing leader; acquire-read by
        /// `get`, so a reader that sees `true` sees `value`.
        ready: std.atomic.Value(bool) = .init(false),
        interior: Io.Mutex = .init,
        /// The leadership latch for the init race.
        busy: bool = false,
        /// Parked losers of the election.
        changed: Notify = .init,

        pub const init: Self = .{};

        pub fn get(self: *Self) ?*const T {
            if (!self.ready.load(.acquire)) return null;
            return &self.value;
        }

        /// Manual leadership: claim the latch synchronously. `true` ⇒ the
        /// caller owns initialization and MUST `completeInit` or
        /// `abandonInit`; `false` ⇒ the value is set or another leader runs.
        pub fn claimInit(self: *Self, io: Io) bool {
            if (self.ready.load(.acquire)) return false;
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            return self.electLocked();
        }

        /// The election, under the interior mutex. `ready` is re-checked
        /// here because a leader publishes BEFORE it releases the latch: a
        /// caller that read `ready == false` before that publish and takes
        /// the latch after the release would otherwise run a second
        /// initializer against a set value.
        fn electLocked(self: *Self) bool {
            if (self.ready.load(.acquire) or self.busy) return false;
            self.busy = true;
            return true;
        }

        /// Set the value and release a manual leader's latch, in that order:
        /// parked callers wake and read it.
        pub fn completeInit(self: *Self, io: Io, v: T) void {
            self.publish(v);
            self.unbusy(io);
        }

        /// Release a manual leader's latch WITHOUT a value: a parked caller
        /// re-elects and runs its own initializer.
        pub fn abandonInit(self: *Self, io: Io) void {
            self.unbusy(io);
        }

        /// Get, or run `initializer(ctx, io)` exactly once across every
        /// concurrent caller. A leader's error (including `error.Canceled`)
        /// is its own result and releases leadership; losers re-elect.
        pub fn getOrInit(self: *Self, io: Io, ctx: anytype, comptime initializer: fn (@TypeOf(ctx), Io) anyerror!T) anyerror!*const T {
            while (true) {
                if (self.get()) |v| return v;
                const leader = blk: {
                    self.interior.lockUncancelable(io);
                    defer self.interior.unlock(io);
                    break :blk self.electLocked();
                };
                if (leader) {
                    const v = initializer(ctx, io) catch |err| {
                        self.unbusy(io);
                        return err;
                    };
                    self.publish(v);
                    self.unbusy(io);
                    return &self.value;
                }
                // Loser: enable-then-recheck, then park for a change (init
                // done or leader gone).
                var park = self.changed.notified();
                _ = park.enable(io);
                const still_busy = blk: {
                    self.interior.lockUncancelable(io);
                    defer self.interior.unlock(io);
                    break :blk self.busy;
                };
                if (self.get() != null or !still_busy) {
                    park.cancel(io);
                    continue;
                }
                try park.wait(io);
            }
        }

        fn publish(self: *Self, v: T) void {
            self.value = v;
            self.ready.store(true, .release);
        }

        fn unbusy(self: *Self, io: Io) void {
            self.interior.lockUncancelable(io);
            self.busy = false;
            self.interior.unlock(io);
            self.changed.notifyWaiters(io);
        }
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

const Cell = OnceCell(u64);

const Ctx = struct {
    inits: std.atomic.Value(u32) = .init(0),
    gate: std.atomic.Value(u32) = .init(0),
    fail_first: bool = false,
    value: u64 = 42,

    fn initializer(c: *Ctx, io: Io) anyerror!u64 {
        const n = c.inits.fetchAdd(1, .acq_rel) + 1;
        while (c.gate.load(.acquire) == 0) try futex.ioWaitTimeout(io, &c.gate, 0, null);
        if (c.fail_first and n == 1) return error.LeaderDied;
        return c.value;
    }

    fn open(c: *Ctx, io: Io) void {
        c.gate.store(1, .release);
        futex.ioWake(io, &c.gate, std.math.maxInt(u32));
    }
};

test "init_runs_exactly_once_across_racers" {
    const io = testing.io;
    var cell: Cell = .init;
    var c: Ctx = .{};
    const Racer = struct {
        fn run(cl: *Cell, cx: *Ctx) !void {
            try testing.expectEqual(@as(u64, 42), (try cl.getOrInit(testing.io, cx, Ctx.initializer)).*);
        }
    };
    var ts: [8]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, Racer.run, .{ &cell, &c });
    while (cell.changed.waiters(io) < 7) try std.Thread.yield();
    c.open(io);
    for (ts) |t| t.join();
    try testing.expectEqual(@as(u32, 1), c.inits.load(.acquire));
    try testing.expectEqual(@as(u64, 42), cell.get().?.*);
}

test "a racer that read an unset cell and elects after the leader published never runs a second initializer (the election re-checks under the latch)" {
    const Shared = struct {
        cells: [200]Cell = @splat(.init),
        inits: std.atomic.Value(u32) = .init(0),
        fn instant(s: *@This(), _: Io) anyerror!u64 {
            return s.inits.fetchAdd(1, .acq_rel);
        }
        fn run(s: *@This()) !void {
            for (&s.cells) |*c| _ = try c.getOrInit(testing.io, s, instant);
        }
    };
    var s: Shared = .{};
    var ts: [4]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, Shared.run, .{&s});
    for (ts) |t| t.join();
    try testing.expectEqual(@as(u32, s.cells.len), s.inits.load(.acquire));
}

test "cancelled_leader_releases_leadership: a failed leader's error is its own; a parked racer re-elects and initializes" {
    const io = testing.io;
    var cell: Cell = .init;
    var c: Ctx = .{ .fail_first = true };
    const Shared = struct {
        errors: std.atomic.Value(u32) = .init(0),
        fn run(cl: *Cell, cx: *Ctx, s: *@This()) void {
            if (cl.getOrInit(testing.io, cx, Ctx.initializer)) |v| {
                std.debug.assert(v.* == 42);
            } else |_| {
                _ = s.errors.fetchAdd(1, .acq_rel);
            }
        }
    };
    var sh: Shared = .{};
    var ts: [3]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, Shared.run, .{ &cell, &c, &sh });
    while (cell.changed.waiters(io) < 2) try std.Thread.yield();
    c.open(io);
    for (ts) |t| t.join();
    try testing.expectEqual(@as(u32, 1), sh.errors.load(.acquire));
    try testing.expectEqual(@as(u32, 2), c.inits.load(.acquire));
    try testing.expectEqual(@as(u64, 42), cell.get().?.*);

    // A leader cancelled inside its initializer releases the latch the same way.
    var cell2: Cell = .init;
    var c2: Ctx = .{};
    const Leader = struct {
        fn run(cl: *Cell, cx: *Ctx) anyerror!u64 {
            return (try cl.getOrInit(testing.io, cx, Ctx.initializer)).*;
        }
    };
    var fut = io.concurrent(Leader.run, .{ &cell2, &c2 }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    while (c2.inits.load(.acquire) == 0) try std.Thread.yield();
    try testing.expectError(error.Canceled, fut.cancel(io));
    try testing.expect(cell2.get() == null);
    c2.open(io);
    try testing.expectEqual(@as(u64, 42), (try cell2.getOrInit(io, &c2, Ctx.initializer)).*);
}

test "manual_leader_parks_racers_and_completes_them: no racer runs its initializer; a second claim is refused" {
    const io = testing.io;
    var cell: Cell = .init;
    var c: Ctx = .{};
    try testing.expect(cell.claimInit(io));
    try testing.expect(!cell.claimInit(io));
    const Racer = struct {
        fn run(cl: *Cell, cx: *Ctx) !void {
            try testing.expectEqual(@as(u64, 7), (try cl.getOrInit(testing.io, cx, Ctx.initializer)).*);
        }
    };
    var ts: [4]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, Racer.run, .{ &cell, &c });
    while (cell.changed.waiters(io) < 4) try std.Thread.yield();
    cell.completeInit(io, 7);
    for (ts) |t| t.join();
    try testing.expectEqual(@as(u32, 0), c.inits.load(.acquire));
    try testing.expect(!cell.claimInit(io));
}

test "abandoned_manual_leader_lets_a_racer_reelect" {
    const io = testing.io;
    var cell: Cell = .init;
    var c: Ctx = .{};
    c.open(io);
    try testing.expect(cell.claimInit(io));
    const Racer = struct {
        fn run(cl: *Cell, cx: *Ctx) !void {
            try testing.expectEqual(@as(u64, 42), (try cl.getOrInit(testing.io, cx, Ctx.initializer)).*);
        }
    };
    const t = try std.Thread.spawn(.{}, Racer.run, .{ &cell, &c });
    while (cell.changed.waiters(io) == 0) try std.Thread.yield();
    cell.abandonInit(io);
    t.join();
    try testing.expectEqual(@as(u32, 1), c.inits.load(.acquire));
    try testing.expectEqual(@as(u64, 42), cell.get().?.*);
}
