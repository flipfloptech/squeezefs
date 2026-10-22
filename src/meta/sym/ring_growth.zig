//! A joined appender's RING GROWTH — the appender's half of `GrowRing`
//! (charter §5.5 "Appenders": "growth = replace-when-drained (`GrowRing`,
//! one doubling step)"; `docs/porting/sym_appenders_manager.md` §3.3
//! "Joiner side (`joined_grow_ring_if_due`, run at the cadence)").
//! PORTABLE: the grow decision as a pure function with its ties, the
//! commit-rate fold, and the growth GATE — the Dekker pair a growth closes
//! on the conveyor's passes while it drains the ring. The device-bound
//! sequence (the drain cycles, the verb, the page, the swap) is
//! `joined.zig`'s `growRingIfDue`.
//!
//! WHO ASKS: the JOINER, at its checkpoint cadence — never the manager
//! (which carves on request and judges the ring budget, `heap/16 −
//! rings_in_use`), never the conveyor pass that stalled (a stall is the
//! trigger the cadence reads). The joiner asks when its measured commit
//! rate derives a ring above the current one, or when a fresh ring-
//! admission stall arrived since the last decision; a decline the manager
//! gave at THIS ring size stands until the ring changes or a fresh stall
//! arrives (never a verb per cycle against a limit the manager named).
//! One step is at least a DOUBLING: the page's table holds eight
//! segments, and a ring that followed a converging EWMA by its increments
//! would spend them on a storm's first second.
//!
//! THE GATE: a growth swaps the ring's geometry — every logical position's
//! page map — so no pass may be between its ring admission and its
//! window's terminal outcome while the swap runs. The pass does
//! `passes_inside += 1 ; fence ; load closed` and is INSIDE iff the gate
//! is open (else it withdraws and parks on `opened`); the growth does
//! `closed = true ; fence ; load passes_inside` and waits the count — and
//! the conveyor's in-flight windows — to zero. Never (a pass inside ∧ the
//! growth read 0): that pass would reserve on a ring whose map changes
//! under it. The pair is `membership/park_core_model.zig`'s invariant 1
//! under other names (`inflight`/`state` there; the model and its
//! weakening — either fence removed — are explored there), restated by the
//! test below.

const std = @import("std");
const Io = std.Io;
const notify = @import("../../sync/notify.zig");
const region = @import("region.zig");
const page = @import("appender_page.zig");
const explorer = @import("../../model/explorer.zig");
const park_model = @import("membership/park_core_model.zig");

// ---------------------------------------------------------------------------
// The decision (pure).
// ---------------------------------------------------------------------------

/// The words the cadence reads before deciding.
pub const Inputs = struct {
    /// The ring's bytes now (the segment table's total).
    current: u64,
    /// `region.ringBytesDerived` of the measured commit rate.
    derived: u64,
    /// `region.ringCeiling(volume_len)`.
    ceiling: u64,
    /// Ring-admission stalls on this ring so far, and the count as of the
    /// last decision — growth fires when they moved.
    stalls: u64,
    stalls_at_last_grow: u64,
    /// The ring size at which the manager last declined (`maxInt` = never).
    declined_at: u64,
    /// The page's segment count (the table holds `RING_SEGMENTS_MAX`).
    segments: u8,
    node_size: u64,
};

pub const NEVER_DECLINED: u64 = std.math.maxInt(u64);

pub const Verdict = union(enum) {
    /// Nothing is due (derived ≤ current and no fresh stall), or the
    /// manager's decline at this size stands.
    idle,
    /// Due, but the step rounds to nothing (at the ceiling): the stall
    /// mark advances, no verb.
    at_ceiling,
    /// Due, but the table is full: a decline on the joiner's own counter.
    table_full,
    /// Ask the manager for `step` bytes (whole nodes, ≥ one doubling).
    ask: u64,
};

/// `joined_grow_ring_if_due`'s decision (spec §3.3): due when `derived >
/// current` or `stalls` moved; a decline at this size stands until either
/// changes; target = `growTarget` (at least a doubling, at most the
/// ceiling); step = `(target − current) ↓ node_size`.
pub fn judge(in: Inputs) Verdict {
    const stalled = in.stalls != in.stalls_at_last_grow;
    if (in.derived <= in.current and !stalled) return .idle;
    if (!stalled and in.declined_at == in.current) return .idle;
    const target = region.growTarget(in.derived, in.current, in.ceiling);
    const step = (target -| in.current) / in.node_size * in.node_size;
    if (step == 0) return .at_ceiling;
    if (in.segments >= page.RING_SEGMENTS_MAX) return .table_full;
    return .{ .ask = step };
}

/// The commit-rate fold at a cadence: the ring head's advance since the
/// last fold over the elapsed time, folded `7/8 · ewma + 1/8 · rate`
/// (`region.ewmaFold`). Positions are bytes, so the head's advance IS the
/// bytes journaled (entries, pads and page headers alike — what the ring
/// must hold). A fold under one millisecond or with no prior mark measures
/// nothing and answers the EWMA unchanged.
pub fn foldCommitRate(ewma_bytes_per_s: u64, head: u64, head_at_last_fold: ?u64, elapsed_ns: u64) u64 {
    const last = head_at_last_fold orelse return ewma_bytes_per_s;
    if (elapsed_ns < std.time.ns_per_ms) return ewma_bytes_per_s;
    const rate = (head -| last) *| std.time.ns_per_s / elapsed_ns;
    return region.ewmaFold(ewma_bytes_per_s, rate);
}

// ---------------------------------------------------------------------------
// The gate.
// ---------------------------------------------------------------------------

pub const Gate = struct {
    io: Io,
    /// SeqCst on both sides (module doc: the Dekker pair with
    /// `passes_inside`; `park_core_model.DekkerModel` is the proof).
    closed: std.atomic.Value(bool) = .init(false),
    passes_inside: std.atomic.Value(u32) = .init(0),
    /// Woken when the gate opens (the pass side parks on it).
    opened: notify.Notify = .init,
    /// Woken when a pass leaves or a window settles while the gate is
    /// closed (the growth's drain wait).
    drained: notify.Notify = .init,

    pub fn init(io: Io) Gate {
        return .{ .io = io };
    }

    pub fn isClosed(self: *const Gate) bool {
        return self.closed.load(.seq_cst);
    }

    pub fn passesInside(self: *const Gate) u32 {
        return self.passes_inside.load(.seq_cst);
    }

    /// The pass's half: inside iff the gate is open; a closed gate parks
    /// the pass (register-recheck-await on `opened`) and it tries again.
    /// Returns once inside — the caller MUST `leave`.
    pub fn enter(self: *Gate) void {
        while (true) {
            var n = self.opened.notified();
            // The RMW then the SeqCst load: the pair's fence.
            _ = self.passes_inside.fetchAdd(1, .seq_cst);
            if (!self.closed.load(.seq_cst)) {
                n.cancel(self.io);
                return;
            }
            // Withdraw (the growth may be waiting on exactly this count).
            _ = self.passes_inside.fetchSub(1, .seq_cst);
            self.drained.notifyWaiters(self.io);
            if (n.enable(self.io)) {
                n.waitUncancelable(self.io);
                continue;
            }
            if (!self.closed.load(.seq_cst)) {
                n.cancel(self.io);
                continue;
            }
            n.waitUncancelable(self.io);
        }
    }

    /// The pass left its admit→handoff window.
    pub fn leave(self: *Gate) void {
        _ = self.passes_inside.fetchSub(1, .seq_cst);
        self.noteWindowLeft();
    }

    /// A stage-B window reached its terminal outcome (the conveyor's
    /// in-flight count moved): a growth waiting on it is woken. One SeqCst
    /// load when nothing grows.
    pub fn noteWindowLeft(self: *Gate) void {
        if (self.closed.load(.seq_cst)) self.drained.notifyWaiters(self.io);
    }

    /// The growth's half: close, then read the count (SeqCst store + load
    /// — the pair's other fence). Answers the passes seen inside.
    pub fn close(self: *Gate) u32 {
        self.closed.store(true, .seq_cst);
        return self.passes_inside.load(.seq_cst);
    }

    /// Open the gate and wake every pass parked on it.
    pub fn open(self: *Gate) void {
        self.closed.store(false, .seq_cst);
        self.opened.notifyWaiters(self.io);
    }

    /// Wait until `windowEmpty()` holds or `deadline_ns` passes, waking on
    /// `drained` (a pass left, a window settled, a pass withdrew). True =
    /// empty. The caller holds the gate closed.
    pub fn awaitDrained(self: *Gate, deadline_ns: u64, ctx: anytype, comptime windowEmpty: fn (@TypeOf(ctx)) bool) bool {
        while (true) {
            if (windowEmpty(ctx)) return true;
            var n = self.drained.notified();
            if (n.enable(self.io)) {
                n.waitUncancelable(self.io);
                continue;
            }
            if (windowEmpty(ctx)) {
                n.cancel(self.io);
                return true;
            }
            switch (n.waitUntilUncancelable(self.io, deadline_ns)) {
                .notified => {},
                .deadline => return windowEmpty(ctx),
            }
        }
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const MiB = 1 << 20;
const FLOOR = region.RING_FLOOR_BYTES;

fn inputs(current: u64, derived: u64, stalls: u64, mark: u64, declined: u64, segments: u8) Inputs {
    return .{ .current = current, .derived = derived, .ceiling = 16 * MiB, .stalls = stalls, .stalls_at_last_grow = mark, .declined_at = declined, .segments = segments, .node_size = 256 * 1024 };
}

test "the decision ties (spec §3.3): idle below the derived size with no stall; a fresh stall asks at least a doubling; derived above current asks up to the derived size; a decline at this size stands until the ring changes or a stall arrives; the ceiling rounds the step to nothing; a full table declines" {
    // Quiet at the floor: nothing due.
    try testing.expectEqual(Verdict.idle, judge(inputs(FLOOR, FLOOR, 0, 0, NEVER_DECLINED, 1)));
    // A fresh stall: one doubling (512 KiB → +512 KiB).
    try testing.expectEqual(Verdict{ .ask = FLOOR }, judge(inputs(FLOOR, FLOOR, 1, 0, NEVER_DECLINED, 1)));
    // Derived 5 MiB at a 1 MiB ring, no stall: the step is 4 MiB (the derived size, above the doubling).
    try testing.expectEqual(Verdict{ .ask = 4 * MiB }, judge(inputs(1 * MiB, 5 * MiB, 3, 3, NEVER_DECLINED, 2)));
    // Derived 1.5 MiB at 1 MiB: at least a doubling (+1 MiB).
    try testing.expectEqual(Verdict{ .ask = 1 * MiB }, judge(inputs(1 * MiB, 3 * MiB / 2, 3, 3, NEVER_DECLINED, 2)));
    // The step rounds down to whole nodes.
    try testing.expectEqual(Verdict{ .ask = 1 * MiB }, judge(inputs(1 * MiB, 2 * MiB + 100, 3, 3, NEVER_DECLINED, 2)));
    // Declined at this size, no fresh stall: idle even with derived above current.
    try testing.expectEqual(Verdict.idle, judge(inputs(1 * MiB, 5 * MiB, 3, 3, 1 * MiB, 2)));
    // The same decline with a fresh stall: asked again.
    try testing.expectEqual(Verdict{ .ask = 4 * MiB }, judge(inputs(1 * MiB, 5 * MiB, 4, 3, 1 * MiB, 2)));
    // Declined at ANOTHER size: the ring changed since, so the decline is void.
    try testing.expectEqual(Verdict{ .ask = 4 * MiB }, judge(inputs(1 * MiB, 5 * MiB, 3, 3, FLOOR, 2)));
    // At the ceiling with a stall: the step is 0 — the mark advances, no verb.
    try testing.expectEqual(Verdict.at_ceiling, judge(inputs(16 * MiB, 40 * MiB, 9, 8, NEVER_DECLINED, 6)));
    // A full table below the ceiling: the joiner's own decline.
    try testing.expectEqual(Verdict.table_full, judge(inputs(2 * MiB, 8 * MiB, 9, 8, NEVER_DECLINED, page.RING_SEGMENTS_MAX)));
}

test "the commit-rate fold: no mark or under a millisecond measures nothing; the head's advance per second folds 7/8 + 1/8; a converging fold reaches the rate" {
    try testing.expectEqual(@as(u64, 1000), foldCommitRate(1000, 5000, null, std.time.ns_per_s));
    try testing.expectEqual(@as(u64, 1000), foldCommitRate(1000, 5000, 0, 500_000));
    // 8 MiB in one second from an EWMA of 0: one eighth of it.
    try testing.expectEqual(@as(u64, MiB), foldCommitRate(0, 8 * MiB, 0, std.time.ns_per_s));
    // 4 MiB in half a second = 8 MiB/s; from 8 MiB/s the fold is a fixpoint.
    try testing.expectEqual(@as(u64, 8 * MiB), foldCommitRate(8 * MiB, 12 * MiB, 8 * MiB, std.time.ns_per_s / 2));
    var ewma: u64 = 0;
    var head: u64 = 0;
    for (0..64) |_| {
        head += 8 * MiB;
        ewma = foldCommitRate(ewma, head, head - 8 * MiB, std.time.ns_per_s);
    }
    try testing.expect(ewma > 8 * MiB - 8 * MiB / 1000 and ewma <= 8 * MiB);
}

test "the gate's Dekker pair is park_core_model's invariant 1 (passes_inside ≡ inflight, closed ≡ PARKED): it holds with both fences and fails with either removed" {
    try explorer.expectHolds(park_model.DekkerModel(.{}), .{});
    try explorer.expectFails(park_model.DekkerModel(.{ .enter_fence = false }), .{}, error.InvariantViolated);
    try explorer.expectFails(park_model.DekkerModel(.{ .park_fence = false }), .{}, error.InvariantViolated);
}

test "the gate: an open gate admits and counts; a closed gate parks the pass until `open`; `close` answers the passes inside; `awaitDrained` returns when the window empties and false at the deadline" {
    const io = testing.io;
    var g = Gate.init(io);
    g.enter();
    try testing.expectEqual(@as(u32, 1), g.passesInside());
    try testing.expectEqual(@as(u32, 1), g.close());
    const Empty = struct {
        fn windowEmpty(gate: *Gate) bool {
            return gate.passesInside() == 0;
        }
    };
    // Not drained inside a short deadline while the pass is inside.
    try testing.expect(!g.awaitDrained(@import("../../core/clock.zig").monotonicNs() + 2 * std.time.ns_per_ms, &g, Empty.windowEmpty));
    const Parked = struct {
        fn run(gate: *Gate, entered: *std.atomic.Value(bool)) void {
            gate.enter();
            entered.store(true, .release);
            gate.leave();
        }
    };
    var entered: std.atomic.Value(bool) = .init(false);
    const th = try std.Thread.spawn(.{}, Parked.run, .{ &g, &entered });
    // The pass inside leaves: the drain wait completes on the wake.
    g.leave();
    try testing.expect(g.awaitDrained(std.math.maxInt(u64), &g, Empty.windowEmpty));
    try testing.expect(!entered.load(.acquire));
    g.open();
    th.join();
    try testing.expect(entered.load(.acquire));
    try testing.expectEqual(@as(u32, 0), g.passesInside());
    try testing.expect(!g.isClosed());
}
