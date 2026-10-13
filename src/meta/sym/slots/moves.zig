//! THE SLOT-MOVE EPOCH (decision 0003 §X.1): one per mount, bumped by
//! every completed lease transition this mount can observe — a grant
//! installed, a release landed, a foreign view loaded, learnt or dropped,
//! a recovery's slots released, an abort reopening a door — and the PARK
//! the move-retry law's caller takes on it. The caller observes the epoch
//! BEFORE its attempt; a refused attempt registers on the waiters, re-reads
//! the epoch (a transition between the attempt and the registration is
//! seen here) and only then parks, bounded by its re-poll: a REMOTE move's
//! completion (another member's grant, a holder's recovery elsewhere) is
//! observed by the next attempt's re-resolution, never by a wake from
//! another process. `move_epoch_model.zig` proves the order and that each
//! of its two weakenings loses the wake. PORTABLE: one atomic word and a
//! `sync.Notify`.

const std = @import("std");
const Io = std.Io;
const sync = @import("../../../sync/sync.zig");

/// The re-poll's first interval: a move completes no faster than one
/// device barrier (the release's tree-0 entry and the grant's are each
/// barriered) — `plane.COLD_BARRIER_NS`'s order.
pub const REPOLL_MIN_NS: u64 = std.time.ns_per_ms;
/// The re-poll's ceiling, doubling from the floor: two of the measured
/// upper handover (`acquire.HANDOVER_BOUND_FLOOR_NS`, spec §4.4's idle-arm
/// band) — a remote move has completed within it, so a caller learns of it
/// at most one ceiling late.
pub const REPOLL_MAX_NS: u64 = 40 * std.time.ns_per_ms;

/// The law's bound: `T_park_max` — the fail-stop lattice's longest
/// legitimate wait on an unresolved holder (a dead holder's eviction at
/// `T_owner`, the successor's arm and replay, its grace window:
/// `park_reclaim.tParkMaxMs`). Past it the holder is gone for this op.
pub fn deadlineNs(t_park_max_ms: u64) u64 {
    return t_park_max_ms *| std.time.ns_per_ms;
}

/// The next re-poll interval: doubling, capped.
pub fn nextRepollNs(cur_ns: u64) u64 {
    return @min(cur_ns *| 2, REPOLL_MAX_NS);
}

pub const Parked = enum { moved, repoll, closed };

pub const Moves = struct {
    io: Io,
    epoch: std.atomic.Value(u64) = .init(0),
    waiters: sync.Notify = .init,
    closed: std.atomic.Value(bool) = .init(false),

    pub fn init(io: Io) Moves {
        return .{ .io = io };
    }

    /// A transition completed (its state already published): the epoch
    /// moves — the release pairs with `observe`'s acquire, so a caller that
    /// reads the new epoch reads the transition — then every parked caller
    /// wakes to re-resolve.
    pub fn bump(self: *Moves) void {
        _ = self.epoch.fetchAdd(1, .release);
        self.waiters.notifyWaiters(self.io);
    }

    /// The epoch a caller samples BEFORE its attempt.
    pub fn observe(self: *const Moves) u64 {
        return self.epoch.load(.acquire);
    }

    /// Park after an attempt that began at `seen` was refused: register,
    /// re-read, wait until a bump, `until_ns`, or the mount's close.
    pub fn park(self: *Moves, seen: u64, until_ns: u64) Parked {
        var n = self.waiters.notified();
        _ = n.enable(self.io);
        if (self.closed.load(.acquire)) {
            n.cancel(self.io);
            return .closed;
        }
        if (self.epoch.load(.acquire) != seen) {
            n.cancel(self.io);
            return .moved;
        }
        return switch (n.waitUntilUncancelable(self.io, until_ns)) {
            .notified => if (self.closed.load(.acquire)) .closed else .moved,
            .deadline => .repoll,
        };
    }

    /// The mount's teardown: every park answers `closed` at once and no
    /// later one waits (the release pairs with `park`'s acquire).
    pub fn close(self: *Moves) void {
        self.closed.store(true, .release);
        self.waiters.notifyWaiters(self.io);
    }

    pub fn isClosed(self: *const Moves) bool {
        return self.closed.load(.acquire);
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const clock = @import("../../../core/clock.zig");

test "derivations: the re-poll doubles from one barrier to two measured upper handovers and stays there; the deadline is T_park_max in ns (saturating)" {
    try testing.expectEqual(@as(u64, 2 * std.time.ns_per_ms), nextRepollNs(REPOLL_MIN_NS));
    var r: u64 = REPOLL_MIN_NS;
    var steps: u32 = 0;
    while (r < REPOLL_MAX_NS) : (steps += 1) r = nextRepollNs(r);
    try testing.expectEqual(REPOLL_MAX_NS, r);
    try testing.expectEqual(@as(u32, 6), steps);
    try testing.expectEqual(REPOLL_MAX_NS, nextRepollNs(REPOLL_MAX_NS));
    try testing.expectEqual(@as(u64, 48_000) * std.time.ns_per_ms, deadlineNs(48_000));
    try testing.expectEqual(std.math.maxInt(u64), deadlineNs(std.math.maxInt(u64)));
}

test "the park: a bump between the attempt's sample and the park is seen at the recheck (moved, no wait); a bump while parked wakes it; nothing moving is a re-poll at its bound; the close answers every park at once" {
    var m = Moves.init(testing.io);
    const seen = m.observe();
    m.bump();
    try testing.expectEqual(Parked.moved, m.park(seen, std.math.maxInt(u64)));
    const seen2 = m.observe();
    try testing.expectEqual(Parked.repoll, m.park(seen2, clock.monotonicNs() + std.time.ns_per_ms));
    const Bumper = struct {
        fn run(mv: *Moves) void {
            // Bump once the caller is registered (a parked waiter).
            while (mv.waiters.waiters(mv.io) == 0) std.Thread.yield() catch {};
            mv.bump();
        }
    };
    const th = try std.Thread.spawn(.{}, Bumper.run, .{&m});
    try testing.expectEqual(Parked.moved, m.park(seen2, std.math.maxInt(u64)));
    th.join();
    m.close();
    try testing.expect(m.isClosed());
    try testing.expectEqual(Parked.closed, m.park(m.observe(), std.math.maxInt(u64)));
}
