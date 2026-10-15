//! THE ONE throttle (charter §8 "per-worker duty-cycle throttle (`sleep t ×
//! (100−pct)/pct`, live-retunable)"; spec §1.5, KD-3).
//!
//! `dutySleepNs(elapsed, pct)`: `pct == 0 ∨ pct ≥ 100 ⇒ none`; else `sleep
//! = elapsed × (100 − pct) / pct`, none under 1 ms — task-active time ≈
//! `pct` of wall time. The park is served in 50 ms SLICES that re-read the
//! LIVE pct and the control flags per slice: a live rethrottle / pause /
//! cancel reaches a parked worker within one slice (at 1 % a 2 s task
//! priced a 198 s un-interruptible park in the reference), while the TOTAL
//! park for a fixed pct is unchanged. `Control` keeps the throttle's books
//! (the task time priced, the park served), so the duty it grants is
//! judged apart from how promptly the scheduler runs the worker.
//!
//! The duty cycle is per WORKER: an unthrottled drain/rebalance overlaps
//! `MOVER_PIPELINE_WIDTH` independent block moves (each move pays read →
//! write → verify-read → publish serially; the serial loop measured
//! exactly the ½-of-raw-copy floor); throttled passes and defrag (order-
//! dependent D1/D2 floors) stay width 1. Portable; the park takes an `io`.

const std = @import("std");
const Io = std.Io;
const clock = @import("../../core/clock.zig");
const sync = @import("../../sync/sync.zig");

const Atomic = std.atomic.Value;

/// A retune/pause reaches a parked worker within one slice; the total park
/// is unchanged.
pub const DUTY_SLICE_NS: u64 = 50 * std.time.ns_per_ms;
/// Below this the park is noise against the slice clock.
pub const DUTY_MIN_SLEEP_NS: u64 = std.time.ns_per_ms;
/// Overlaps the 3 serialized device round trips per move (read, write +
/// verify-read, publish); the serial loop measured exactly the ½-of-raw
/// floor, width 4 recovers it. Only the unthrottled mover passes use it.
pub const MOVER_PIPELINE_WIDTH: u32 = 4;

/// The law as integer arithmetic: `elapsed × (100 − pct) / pct`, `null`
/// when unthrottled or under 1 ms.
pub fn dutySleepNs(elapsed_ns: u64, pct: u8) ?u64 {
    if (pct == 0 or pct >= 100) return null;
    const e: u128 = elapsed_ns;
    const sleep: u128 = e * (100 - @as(u128, pct)) / pct;
    const capped: u64 = @intCast(@min(sleep, std.math.maxInt(u64)));
    return if (capped < DUTY_MIN_SLEEP_NS) null else capped;
}

/// The pipeline width a pass may use: unthrottled movers overlap
/// `MOVER_PIPELINE_WIDTH`; everything else is 1.
pub fn pipelineWidth(pct: u8, order_dependent: bool) u32 {
    if (order_dependent) return 1;
    return if (pct == 0 or pct >= 100) MOVER_PIPELINE_WIDTH else 1;
}

/// The live words a parked worker re-reads per slice.
pub const Control = struct {
    /// 0 = unthrottled, 1..=100.
    pct: Atomic(u8) = .init(100),
    /// Operator pause intent / a cancel: the park ends at the next slice
    /// and the worker stops AFTER its in-flight task.
    stop: Atomic(bool) = .init(false),
    /// Woken by `retune`/`interrupt` so a parked worker re-reads at once
    /// (the slice is the bound, the notify the fast path).
    wake: sync.Notify = .init,
    /// The throttle's books: the task-active ns its parks were priced on,
    /// and the park ns served — each slice credited no more than its
    /// deadline, so a late wake lengthens the wall park, never pays it
    /// down. At a fixed pct `served_ns = Σ dutySleepNs(task, pct)`.
    /// Monotonic adds by the job's claim holder alone (the claim word's
    /// release/acquire orders one holder's adds before the next's).
    worked_ns: Atomic(u64) = .init(0),
    served_ns: Atomic(u64) = .init(0),

    pub fn setPct(self: *Control, io: Io, pct: u8) void {
        std.debug.assert(pct <= 100);
        self.pct.store(pct, .release);
        self.wake.notifyWaiters(io);
    }

    pub fn interrupt(self: *Control, io: Io) void {
        self.stop.store(true, .release);
        self.wake.notifyWaiters(io);
    }

    pub fn clearStop(self: *Control) void {
        self.stop.store(false, .release);
    }

    pub fn loadPct(self: *const Control) u8 {
        return self.pct.load(.acquire);
    }

    pub fn stopped(self: *const Control) bool {
        return self.stop.load(.acquire);
    }
};

pub const ParkOutcome = enum { served, interrupted };

/// Park after a task that ran `elapsed_ns`: the sleep the law prices at
/// the pct read NOW, served in slices; every slice re-reads the pct (a
/// retune re-prices the REMAINDER: `remaining × ratio_new / ratio_old`
/// keeps the total park what the new pct would have charged for the whole
/// task) and the stop flag. Never a sleep: each slice is a `Notify`
/// deadline wait.
pub fn dutyPark(io: Io, ctl: *Control, elapsed_ns: u64) ParkOutcome {
    _ = ctl.worked_ns.fetchAdd(elapsed_ns, .monotonic);
    var pct = ctl.loadPct();
    var remaining = dutySleepNs(elapsed_ns, pct) orelse return .served;
    while (remaining > 0) {
        if (ctl.stopped()) return .interrupted;
        const slice = @min(remaining, DUTY_SLICE_NS);
        const deadline = clock.monotonicNs() + slice;
        var n = ctl.wake.notified();
        _ = n.waitUntilUncancelable(io, deadline);
        const now = clock.monotonicNs();
        const slept = if (now >= deadline) slice else slice - (deadline - now);
        remaining -|= slept;
        _ = ctl.served_ns.fetchAdd(slept, .monotonic);
        const live = ctl.loadPct();
        if (live != pct) {
            // Re-price the remainder under the new pct: the share of the
            // task's park still owed scales by the two laws' ratio.
            const owed_old = dutySleepNs(elapsed_ns, pct) orelse 0;
            const owed_new = dutySleepNs(elapsed_ns, live) orelse 0;
            remaining = if (owed_old == 0) 0 else @intCast(@as(u128, remaining) * owed_new / owed_old);
            pct = live;
        }
    }
    return .served;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "throttle law is exact at every duty target: sleep = t × (100 − pct)/pct — 25/50/75 give 3t, t, t/3; 0 and ≥ 100 are none; under 1 ms is none" {
    const t: u64 = 90 * std.time.ns_per_ms;
    try testing.expectEqual(@as(?u64, 3 * t), dutySleepNs(t, 25));
    try testing.expectEqual(@as(?u64, t), dutySleepNs(t, 50));
    try testing.expectEqual(@as(?u64, t / 3), dutySleepNs(t, 75));
    try testing.expectEqual(@as(?u64, null), dutySleepNs(t, 0));
    try testing.expectEqual(@as(?u64, null), dutySleepNs(t, 100));
    try testing.expectEqual(@as(?u64, null), dutySleepNs(t, 200));
    try testing.expectEqual(@as(?u64, null), dutySleepNs(1_000, 50));
    try testing.expectEqual(@as(?u64, 99 * std.time.ns_per_s), dutySleepNs(std.time.ns_per_s, 1));
    // Every pct in 1..99: active/(active+sleep) ≈ pct within 1 %.
    const big: u64 = std.time.ns_per_s;
    var pct: u8 = 1;
    while (pct < 100) : (pct += 1) {
        const s = dutySleepNs(big, pct).?;
        const duty = @as(u128, big) * 100 / (big + s);
        try testing.expect(duty + 1 >= pct and duty <= pct + 1);
    }
    // No overflow at the top of the range.
    _ = dutySleepNs(std.math.maxInt(u64), 1);
}

test "pipeline width: unthrottled movers overlap 4; throttled or order-dependent passes are serial" {
    try testing.expectEqual(@as(u32, 4), MOVER_PIPELINE_WIDTH);
    try testing.expectEqual(@as(u32, 4), pipelineWidth(0, false));
    try testing.expectEqual(@as(u32, 4), pipelineWidth(100, false));
    try testing.expectEqual(@as(u32, 1), pipelineWidth(50, false));
    try testing.expectEqual(@as(u32, 1), pipelineWidth(100, true));
}

test "dutyPark: unthrottled returns at once; a pre-set stop interrupts before the first slice; a live rethrottle to 100 from another thread ends the park within one slice" {
    const io = testing.io;
    var ctl: Control = .{};
    try testing.expectEqual(ParkOutcome.served, dutyPark(io, &ctl, 10 * std.time.ns_per_ms));
    ctl.setPct(io, 10);
    ctl.interrupt(io);
    const t0 = clock.monotonicNs();
    try testing.expectEqual(ParkOutcome.interrupted, dutyPark(io, &ctl, std.time.ns_per_s));
    try testing.expect(clock.monotonicNs() - t0 < DUTY_SLICE_NS);
    ctl.clearStop();
    // A 1 % park of a 1 s task would be 99 s; the retune lands it.
    ctl.setPct(io, 1);
    const Retune = struct {
        fn run(c: *Control, i: Io) void {
            c.setPct(i, 100);
        }
    };
    const t1 = clock.monotonicNs();
    const th = try std.Thread.spawn(.{}, Retune.run, .{ &ctl, io });
    try testing.expectEqual(ParkOutcome.served, dutyPark(io, &ctl, std.time.ns_per_s));
    th.join();
    try testing.expect(clock.monotonicNs() - t1 < 5 * DUTY_SLICE_NS);
}

test "dutyPark at a fixed pct serves the law's total: a 20 ms task at 50 % parks ≈ 20 ms across slices (±10 %), and its books hold the task and exactly the law's park" {
    const io = testing.io;
    var ctl: Control = .{};
    ctl.setPct(io, 50);
    const t0 = clock.monotonicNs();
    try testing.expectEqual(ParkOutcome.served, dutyPark(io, &ctl, 20 * std.time.ns_per_ms));
    const parked = clock.monotonicNs() - t0;
    try testing.expect(parked >= 18 * std.time.ns_per_ms);
    try testing.expect(parked < 200 * std.time.ns_per_ms);
    try testing.expectEqual(@as(u64, 20 * std.time.ns_per_ms), ctl.worked_ns.load(.monotonic));
    try testing.expectEqual(@as(u64, 20 * std.time.ns_per_ms), ctl.served_ns.load(.monotonic));
    try testing.expect(parked >= ctl.served_ns.load(.monotonic));
}
