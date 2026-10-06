//! An owned set of `Io` tasks with genuine cancel-on-drop (charter §9; the
//! reference's `sqz_taskset.rs`), over `std.Io.Group`.
//!
//! A plain stop latch is NOT equivalent: a latch is observed only where a
//! loop chooses to look, while a cancelled task never resumes past its
//! current cancellation point. `Io.Group.cancel` delivers exactly that: it
//! requests cancellation on every member and waits for all of them — a
//! member parked in a cancellable `src/sync/` wait (or any `Io` cancellation
//! point) returns `error.Canceled` from that point, its frame unwinds, and
//! `cancel` returns only once every member has returned. The set adds the
//! abort-at-start semantic a group lacks: a task that begins after the set
//! was cancelled never runs its body (the gate checks the set's flag and
//! the task's own cancellation status first), and the accounting
//! (`spawned`, `finished`, `cancelled_tasks`) the owners' quiesce logic
//! reads.
//!
//! What cancellation does NOT interrupt, by design: an uncancelable park
//! (`lockUncancelable` — committers are never cancelled) or CPU-bound work
//! without a cancellation point; such a task is joined at its next point.
//!
//! Member functions return `Io.Cancelable!void`; `error.Canceled` is the
//! propagation boundary (`Io.Group` discards it).

const std = @import("std");
const Io = std.Io;

pub const TaskSet = struct {
    group: Io.Group = .init,
    cancelled: std.atomic.Value(bool) = .init(false),
    spawned: std.atomic.Value(u64) = .init(0),
    finished: std.atomic.Value(u64) = .init(0),
    /// Members that observed cancellation: refused at the gate, or returned
    /// `error.Canceled`.
    cancelled_tasks: std.atomic.Value(u64) = .init(0),

    pub const init: TaskSet = .{};

    /// Spawn with its own unit of concurrency (`Io.Group.concurrent`).
    pub fn spawn(self: *TaskSet, io: Io, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) Io.ConcurrentError!void {
        _ = self.spawned.fetchAdd(1, .monotonic);
        errdefer _ = self.spawned.fetchSub(1, .monotonic);
        try self.group.concurrent(io, Gate(function).run, .{ self, io, args });
    }

    /// Spawn where the implementation may run it inline (`Io.Group.async`).
    pub fn spawnAsync(self: *TaskSet, io: Io, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) void {
        _ = self.spawned.fetchAdd(1, .monotonic);
        self.group.async(io, Gate(function).run, .{ self, io, args });
    }

    /// Request cancellation on every member and wait for all of them. The
    /// set stays cancelled: a later spawn is refused at its gate.
    pub fn cancel(self: *TaskSet, io: Io) void {
        // release pairs with the gate's acquire load: a member that sees the
        // flag sees every write made before the cancel.
        self.cancelled.store(true, .release);
        self.group.cancel(io);
    }

    /// Wait for every member to finish normally.
    pub fn wait(self: *TaskSet, io: Io) Io.Cancelable!void {
        return self.group.await(io);
    }

    /// Members spawned and not yet finished.
    pub fn live(self: *const TaskSet) u64 {
        return self.spawned.load(.acquire) -| self.finished.load(.acquire);
    }

    /// Cancel-on-drop.
    pub fn deinit(self: *TaskSet, io: Io) void {
        self.cancel(io);
    }

    fn Gate(comptime function: anytype) type {
        const Args = std.meta.ArgsTuple(@TypeOf(function));
        return struct {
            fn run(set: *TaskSet, io: Io, args: Args) Io.Cancelable!void {
                // release pairs with `live`'s acquire: a quiescer that reads
                // the count sees the member's effects.
                defer _ = set.finished.fetchAdd(1, .release);
                // Abort-at-start: a member that begins after the cancel never
                // runs its body (its captures are not touched).
                if (set.cancelled.load(.acquire)) {
                    _ = set.cancelled_tasks.fetchAdd(1, .monotonic);
                    return error.Canceled;
                }
                io.checkCancel() catch |err| {
                    _ = set.cancelled_tasks.fetchAdd(1, .monotonic);
                    return err;
                };
                const result: Io.Cancelable!void = @call(.auto, function, args);
                result catch |err| switch (err) {
                    error.Canceled => {
                        _ = set.cancelled_tasks.fetchAdd(1, .monotonic);
                        return err;
                    },
                };
            }
        };
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;
const sync_testing = @import("testing.zig");
const Notify = @import("notify.zig").Notify;

const Probe = struct {
    never: Notify = .init,
    parked: std.atomic.Value(u32) = .init(0),
    resumed: std.atomic.Value(u32) = .init(0),
    ran: std.atomic.Value(u32) = .init(0),

    /// Parks on a notify nobody ever fires; the code after the park must
    /// never run under cancellation.
    fn parkForever(io: Io, p: *Probe) Io.Cancelable!void {
        _ = p.ran.fetchAdd(1, .acq_rel);
        var w = p.never.notified();
        _ = w.enable(io);
        _ = p.parked.fetchAdd(1, .acq_rel);
        try w.wait(io);
        _ = p.resumed.fetchAdd(1, .acq_rel);
    }

    fn quick(p: *Probe) Io.Cancelable!void {
        _ = p.ran.fetchAdd(1, .acq_rel);
    }
};

fn cancelWakesParked(io: Io) !void {
    var p: Probe = .{};
    var set: TaskSet = .init;
    const n = 4;
    for (0..n) |_| set.spawn(io, Probe.parkForever, .{ io, &p }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    while (p.parked.load(.acquire) < n) try std.Thread.yield();
    try testing.expectEqual(@as(u64, n), set.live());
    set.deinit(io);
    try testing.expectEqual(@as(u64, 0), set.live());
    try testing.expectEqual(@as(u32, n), p.ran.load(.acquire));
    try testing.expectEqual(@as(u32, 0), p.resumed.load(.acquire));
    try testing.expectEqual(@as(u64, n), set.cancelled_tasks.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), p.never.waiters(io));
}

test "cancel_wakes_parked_tasks_that_would_never_wake: deinit returns only after every member unwound at its park" {
    try sync_testing.underBoth(cancelWakesParked);
}

fn cancelledBeforeFirstPoll(io: Io) !void {
    var p: Probe = .{};
    var set: TaskSet = .init;
    set.cancel(io);
    set.spawn(io, Probe.quick, .{&p}) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    set.spawnAsync(io, Probe.quick, .{&p});
    set.cancel(io);
    try testing.expectEqual(@as(u32, 0), p.ran.load(.acquire));
    try testing.expectEqual(@as(u64, 2), set.cancelled_tasks.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), set.live());
}

test "cancelled_before_first_poll_never_runs: a member spawned into a cancelled set is refused at its gate" {
    try sync_testing.underBoth(cancelledBeforeFirstPoll);
}

fn waitJoins(io: Io) !void {
    var p: Probe = .{};
    var set: TaskSet = .init;
    for (0..3) |_| set.spawn(io, Probe.quick, .{&p}) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    try set.wait(io);
    try testing.expectEqual(@as(u32, 3), p.ran.load(.acquire));
    try testing.expectEqual(@as(u64, 0), set.live());
    try testing.expectEqual(@as(u64, 0), set.cancelled_tasks.load(.monotonic));
    set.deinit(io);
    try testing.expectEqual(@as(u64, 3), set.spawned.load(.monotonic));
}

test "wait joins normal completion; deinit on a quiesced set is immediate; live tracks spawned − finished" {
    try sync_testing.underBoth(waitJoins);
}
