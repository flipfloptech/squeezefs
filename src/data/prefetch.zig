//! Prefetch: AIMD per lane, K = 4 offset lanes per file, streaming
//! classification, the pipeline touch and the probe-governed ahead lane
//! (charter §6.3 "a per-lane AIMD prefetch pipeline (start 2, ×2 on
//! foreground wait, ÷2 on evicted-before-consumed; K = 4 offset lanes per
//! file; ≥ 4 contiguous requests classify streaming) and a probe-governed
//! ahead lane (both open-loop depth derivations were falsified by counted
//! brackets)"; `docs/porting/data_read_path.md` §5, §13 Q7, Q11). PORTABLE:
//! pure window functions, the lane state machine and the governor; the
//! fills themselves are issued through the `Env` seam the read path binds.
//!
//! THE TWO FALSIFIED OPEN-LOOP DERIVATIONS — recorded so nobody re-derives
//! them (`.benchmarks/2026-08-01-read-lane.md:76–101`; `read_lane.rs:149–158`):
//!   1. PURE BDP (measured lane bandwidth × measured latency floor): the
//!      write campaign's self-fulfilling-equilibrium lesson reproduced on
//!      reads — the target converges to sustaining the CURRENT plateau;
//!      `read_lane_depth_target` floor-locked at 2 with 32+ streams; a
//!      depth-8 pin changed nothing (25.72 vs 26.28 GB/s).
//!   2. THE AIMD WINDOW AS THE AHEAD DEPTH (engaged at depth 2/4/16 alike):
//!      every engaged row LOST to hold-only — 21.07/20.87/20.79/19.33 vs
//!      25.98 GB/s at qd8, `read_amp` 1.15–1.27 vs 0.71; ~650 GB/row of
//!      ahead deposits died FIFO-unconsumed because the client's own qd ×
//!      streams (256 × 1 MiB) already covered the fabric BDP.
//! Both were DELETED; the closed-loop probe (`ProbeCore`) is what shipped.
//! Under a demand-covered venue the governor probes and correctly
//! retreats — `backoffs ≈ ups ∧ read_lane_fetches ≈ 0` is the designed
//! shape, not a bug.
//!
//! Every stamp is `clock.monotonicNs` (Q11). The parked-task screen is a
//! lane-local bitmap over `[issued_base, issued_base + 256)` (Q7: no heap
//! set per block) — the issue sets a block's bit, the task's first poll
//! clears it, and a block whose bit is set expresses NO verdict in the
//! evicted-unconsumed detector.

const std = @import("std");
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const budget = @import("../mem/budget.zig");
const jc = @import("../meta/journal_core.zig");

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// K lanes per file: concurrent sequential readers of one file must not
/// reset each other; > K degrade to random (a later pipeline start, never
/// wrongness).
pub const K_LANES: usize = 4;
/// The earliest classification: block 0's 4th 1 MiB request.
pub const RUN_THRESHOLD: u32 = 4;
/// Classified-membership tolerance `±64 × len`: a qd-64 reorder span of
/// the stream's own requests (4 KiB ⇒ ±256 KiB keeps random traffic
/// foreign — 0 % member on the random harm shapes, 88–100 % on seq reorder).
pub const MEMBERSHIP_TOLERANCE_MULT: u64 = 64;
/// A foreign run of 16 clears every lane: a spurious stream would veto the
/// ranged path file-wide for 2 s (20–40 GiB/20 s of ungoverned fetches).
pub const FOREIGN_DECLASSIFY_RUN: u32 = 16;
/// The house 2 s staleness: lane claim, freshness, epoch length, issue-
/// owner rotation, dest-lease stamp.
pub const STALE_NS: u64 = 2 * std.time.ns_per_s;
/// The AIMD start.
pub const R2_WINDOW_START: u32 = 2;
/// Cap rails: the start must double once (4); 4096 bounds a pathological
/// budget/block ratio.
pub const WINDOW_CAP_FLOOR: u32 = 4;
pub const WINDOW_CAP_CEIL: u32 = 4096;
/// The quiescence span `2 << min(streak, 5)`: geometric, cap 64 blocks.
pub const QUIESCENCE_STREAK_CAP: u8 = 5;
/// The parked-task screen's span.
pub const PENDING_SCREEN_BITS: usize = 256;

/// ProbeCore: two governor windows per epoch (long enough for a windowed
/// rate, short enough that a ramp to ×32 at +¼/epoch completes in seconds).
pub const PROBE_EPOCH_NS: u64 = 500 * std.time.ns_per_ms;
/// Q6 fixed point.
pub const PROBE_MUL_ONE: u32 = 64;
/// ×32: a runaway guard — the R5 cap is the operative bound.
pub const PROBE_MUL_MAX: u32 = 32 * PROBE_MUL_ONE;
/// The dead-gain latency tax ≈ 1/(N+1) of epochs (the BBR duty cycle).
pub const PROBE_COOLDOWN_EPOCHS: u8 = 8;

pub const stats = struct {
    pub var prefetch_issued: counters.ShardedCounter = .{};
    pub var prefetch_completed: counters.ShardedCounter = .{};
    pub var prefetch_wasted: counters.ShardedCounter = .{};
    pub var prefetch_inflight_bytes: counters.Gauge = .{};
    pub var prefetch_window_hwm: counters.Gauge = .{};
    pub var prefetch_foreground_waits: counters.ShardedCounter = .{};
    pub var prefetch_evicted_unconsumed: counters.ShardedCounter = .{};
    pub var prefetch_active_streams: counters.Gauge = .{};
    pub var prefetch_pending_screen_overflow: counters.ShardedCounter = .{};
    pub var read_streams_classified: counters.ShardedCounter = .{};
    pub var read_lane_fetches: counters.ShardedCounter = .{};
    pub var read_lane_fetch_bytes: counters.ShardedCounter = .{};
    pub var read_lane_depth_target: counters.Gauge = .{};
    pub var read_lane_depth_probe_ups: counters.ShardedCounter = .{};
    pub var read_lane_depth_probe_backoffs: counters.ShardedCounter = .{};
    pub var read_lane_covered_skips: counters.ShardedCounter = .{};
    pub var read_lane_wasted: counters.ShardedCounter = .{};
    pub var read_lane_inflight_bytes: counters.Gauge = .{};
    pub var read_lane_armed: counters.Gauge = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"prefetch_issued\":{d},\"prefetch_completed\":{d},\"prefetch_wasted\":{d},\"prefetch_inflight_bytes\":{d},\"prefetch_window_hwm\":{d},\"prefetch_foreground_waits\":{d},\"prefetch_evicted_unconsumed\":{d},\"prefetch_active_streams\":{d},\"prefetch_pending_screen_overflow\":{d},\"read_streams_classified\":{d},\"read_lane_fetches\":{d},\"read_lane_fetch_bytes\":{d},\"read_lane_depth_target\":{d},\"read_lane_depth_probe_ups\":{d},\"read_lane_depth_probe_backoffs\":{d},\"read_lane_covered_skips\":{d},\"read_lane_wasted\":{d},\"read_lane_inflight_bytes\":{d},\"read_lane_armed\":{d}}}", .{
        stats.prefetch_issued.load(),
        stats.prefetch_completed.load(),
        stats.prefetch_wasted.load(),
        stats.prefetch_inflight_bytes.load(),
        stats.prefetch_window_hwm.load(),
        stats.prefetch_foreground_waits.load(),
        stats.prefetch_evicted_unconsumed.load(),
        stats.prefetch_active_streams.load(),
        stats.prefetch_pending_screen_overflow.load(),
        stats.read_streams_classified.load(),
        stats.read_lane_fetches.load(),
        stats.read_lane_fetch_bytes.load(),
        stats.read_lane_depth_target.load(),
        stats.read_lane_depth_probe_ups.load(),
        stats.read_lane_depth_probe_backoffs.load(),
        stats.read_lane_covered_skips.load(),
        stats.read_lane_wasted.load(),
        stats.read_lane_inflight_bytes.load(),
        stats.read_lane_armed.load(),
    });
}

// ---------------------------------------------------------------------------
// §5.2 Window economy — pure functions.
// ---------------------------------------------------------------------------

/// `bs == 0 ? 4 : clamp(share% × hot_budget / bs, 4, 4096)`.
pub fn derivedPrefetchWindowCap(share_pct: u64, hot_budget: u64, bs: u64) u32 {
    if (bs == 0) return WINDOW_CAP_FLOOR;
    return @intCast(derive.clamp(hot_budget * share_pct / 100 / bs, WINDOW_CAP_FLOOR, WINDOW_CAP_CEIL));
}

/// Explicit wins verbatim ≤ 4096 (0 = kill switch); else the derivation.
pub fn resolveWindowCap(explicit: ?u64, share_pct: u64, hot_budget: u64, bs: u64) u32 {
    if (explicit) |e| return @intCast(@min(e, WINDOW_CAP_CEIL));
    return derivedPrefetchWindowCap(share_pct, hot_budget, bs);
}

/// `resident_share > 0 ∧ unconsumed < resident_share ∧ in_flight +
/// unconsumed < min(window, cap)`: in-flight DMA bytes are R5-charged
/// transients, not residents (charging them against the share made the
/// 16-stream default a depth-1 pipeline).
pub fn prefetchIssueAdmits(window: u32, cap: u32, resident_share: u64, in_flight: u32, unconsumed: u32) bool {
    return resident_share > 0 and unconsumed < resident_share and @as(u64, in_flight) + unconsumed < @min(window, cap);
}

/// `green ∧ (will_wait ∨ (overran ∧ streak == 0))`.
pub fn prefetchWindowGrows(will_wait: bool, overran: bool, streak: u8, green: bool) bool {
    return green and (will_wait or (overran and streak == 0));
}

/// `share% × hot_budget / 100 / bs / active_streams`, NO floor (a `.max(1)`
/// kept 4 contention lanes thrashing a 2-block budget).
pub fn residentShare(share_pct: u64, hot_budget: u64, bs: u64, active_streams: u64) u64 {
    if (bs == 0 or active_streams == 0) return 0;
    return hot_budget * share_pct / 100 / bs / active_streams;
}

/// `2 << min(streak, 5)`.
pub fn quiescenceSpan(streak: u8) u64 {
    return @as(u64, 2) << @intCast(@min(streak, QUIESCENCE_STREAK_CAP));
}

// ---------------------------------------------------------------------------
// §5.5 Depth derivations — pure functions.
// ---------------------------------------------------------------------------

/// `red ⇒ 0; min(pin ?? governed, cap / bs / streams)`.
pub fn readLaneDepthBlocks(pin: ?u32, governed: u32, bs: u64, streams: u64, red: bool, budget_cap: u64) u32 {
    if (red or bs == 0) return 0;
    const want: u64 = pin orelse governed;
    const room = budget_cap / bs / @max(streams, 1);
    return @intCast(@min(want, room));
}

/// `rl_inflight < depth ∧ inflight_bytes + bs ≤ cap`.
pub fn laneIssueAdmits(rl_inflight: u32, depth: u32, inflight_bytes: u64, bs: u64, cap: u64) bool {
    return rl_inflight < depth and inflight_bytes + bs <= cap;
}

/// `(mul − 64) × 4 / 64`: 64→0, 80→1, 100→2, 125→3, 156→5, 195→8.
pub fn probeGovernedDepth(mul: u32) u32 {
    if (mul <= PROBE_MUL_ONE) return 0;
    return (mul - PROBE_MUL_ONE) * 4 / PROBE_MUL_ONE;
}

// ---------------------------------------------------------------------------
// §5.1 Lanes and classification.
// ---------------------------------------------------------------------------

pub const StreamLane = struct {
    next_expected_offset: u64 = 0,
    run_reads: u32 = 0,
    last_seen_ns: u64 = 0,
    classified: bool = false,
    next_prefetch_block: u64 = 0,
    window: u32 = R2_WINDOW_START,
    inflight: u32 = 0,
    rl_inflight: u32 = 0,
    rl_inflight_bytes: u64 = 0,
    unconsumed: u32 = 0,
    consumed_edge: u64 = 0,
    issued_base: u64 = 0,
    generation: u32 = 0,
    last_counted_epoch: u64 = 0,
    suppress_until_edge: u64 = 0,
    detect_streak: u8 = 0,
    /// The parked-task screen over `[issued_base, issued_base + 256)`.
    pending: [PENDING_SCREEN_BITS / 64]u64 = @splat(0),

    fn pendingBit(self: *const StreamLane, block: u64) ?struct { word: usize, bit: u6 } {
        if (block < self.issued_base) return null;
        const rel = block - self.issued_base;
        if (rel >= PENDING_SCREEN_BITS) return null;
        return .{ .word = @intCast(rel / 64), .bit = @intCast(rel % 64) };
    }

    pub fn markPending(self: *StreamLane, block: u64) void {
        const p = self.pendingBit(block) orelse {
            stats.prefetch_pending_screen_overflow.inc();
            return;
        };
        self.pending[p.word] |= @as(u64, 1) << p.bit;
    }

    pub fn clearPending(self: *StreamLane, block: u64) void {
        const p = self.pendingBit(block) orelse return;
        self.pending[p.word] &= ~(@as(u64, 1) << p.bit);
    }

    pub fn isPending(self: *const StreamLane, block: u64) bool {
        const p = self.pendingBit(block) orelse return false;
        return self.pending[p.word] & (@as(u64, 1) << p.bit) != 0;
    }

    /// The claim: `generation += 1`, run 1, window 2, plan/edges/streak
    /// reset; `inflight`/`rl_inflight` NOT reset (tasks decrement at settle;
    /// the generation bump lands their completions as wasted).
    fn claim(self: *StreamLane, offset: u64, len: u64, now: u64) void {
        self.generation +%= 1;
        self.next_expected_offset = offset + len;
        self.run_reads = 1;
        self.last_seen_ns = now;
        self.classified = false;
        self.window = R2_WINDOW_START;
        self.next_prefetch_block = 0;
        self.issued_base = 0;
        self.unconsumed = 0;
        self.consumed_edge = 0;
        self.suppress_until_edge = 0;
        self.detect_streak = 0;
        self.pending = @splat(0);
    }
};

pub const LaneRef = struct { idx: u8, streaming: bool, generation: u32 };

pub const StreamLanes = struct {
    lock: jc.SpinLock = .{},
    lanes: [K_LANES]StreamLane = @splat(.{}),
    issue_owner: ?u8 = null,
    issue_owner_ns: u64 = 0,
    foreign_since_match: u32 = 0,
    /// A leaseable request's stamp: both speculative arms decline while fresh.
    dest_lease_ns: u64 = 0,

    pub const init: StreamLanes = .{};

    /// `observe`: exact contiguity ⇒ advance (classify at the 4th); a
    /// fresh classified lane accepts `|offset − expected| ≤ 64 × len`
    /// advancing max-forward; `!claim ⇒ null` (warm serves may continue a
    /// lane but never mint one); 16 foreign ⇒ declassify every lane; else
    /// claim the stalest lane iff idle ≥ 2 s. Called under the lock.
    pub fn observeLocked(self: *StreamLanes, offset: u64, len: u64, claim: bool, now: u64) ?LaneRef {
        // 1. Exact contiguity.
        for (&self.lanes, 0..) |*l, i| {
            if (l.run_reads == 0 or l.next_expected_offset != offset) continue;
            self.foreign_since_match = 0;
            l.next_expected_offset = offset + len;
            l.run_reads += 1;
            l.last_seen_ns = now;
            if (!l.classified and l.run_reads >= RUN_THRESHOLD) {
                l.classified = true;
                stats.read_streams_classified.inc();
            }
            return .{ .idx = @intCast(i), .streaming = l.classified, .generation = l.generation };
        }
        // 2. Classified-membership tolerance.
        const tol = MEMBERSHIP_TOLERANCE_MULT * len;
        for (&self.lanes, 0..) |*l, i| {
            if (!l.classified or now -| l.last_seen_ns >= STALE_NS) continue;
            const d = if (offset > l.next_expected_offset) offset - l.next_expected_offset else l.next_expected_offset - offset;
            if (d > tol) continue;
            self.foreign_since_match = 0;
            l.next_expected_offset = @max(l.next_expected_offset, offset + len);
            l.run_reads += 1;
            l.last_seen_ns = now;
            return .{ .idx = @intCast(i), .streaming = true, .generation = l.generation };
        }
        // 3. Warm serves never mint.
        if (!claim) return null;
        // 4. The foreign run.
        self.foreign_since_match += 1;
        if (self.foreign_since_match >= FOREIGN_DECLASSIFY_RUN) {
            for (&self.lanes) |*l| {
                l.classified = false;
                l.run_reads = 0;
            }
            self.foreign_since_match = 0;
        }
        // 5. Claim the stalest lane iff idle ≥ 2 s (an empty lane is stalest).
        var stalest: ?usize = null;
        for (&self.lanes, 0..) |*l, i| {
            if (l.run_reads == 0) {
                stalest = i;
                break;
            }
            if (stalest == null or l.last_seen_ns < self.lanes[stalest.?].last_seen_ns) stalest = i;
        }
        const s = &self.lanes[stalest.?];
        if (s.run_reads != 0 and now -| s.last_seen_ns < STALE_NS) return null;
        s.claim(offset, len, now);
        return .{ .idx = @intCast(stalest.?), .streaming = false, .generation = s.generation };
    }

    pub fn observe(self: *StreamLanes, offset: u64, len: u64, claim: bool, now: u64) ?LaneRef {
        self.lock.lock();
        defer self.lock.unlock();
        return self.observeLocked(offset, len, claim, now);
    }

    /// Any lane `classified ∧ now − last_seen < 2 s`: the ranged veto and
    /// the `DemandStream` class.
    pub fn anyStreamingFresh(self: *StreamLanes, now: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.anyStreamingFreshLocked(now);
    }

    fn anyStreamingFreshLocked(self: *StreamLanes, now: u64) bool {
        for (&self.lanes) |*l| if (l.classified and now -| l.last_seen_ns < STALE_NS) return true;
        return false;
    }

    pub fn stampDestLease(self: *StreamLanes, now: u64) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.dest_lease_ns = now;
    }

    pub fn destLeaseFresh(self: *StreamLanes, now: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.destLeaseFreshLocked(now);
    }

    fn destLeaseFreshLocked(self: *const StreamLanes, now: u64) bool {
        return self.dest_lease_ns != 0 and now -| self.dest_lease_ns < STALE_NS;
    }

    /// The R5 Red shed of `prefetch_inflight`: plans cleared.
    pub fn invalidateAll(self: *StreamLanes) void {
        self.lock.lock();
        defer self.lock.unlock();
        for (&self.lanes) |*l| {
            l.generation +%= 1;
            l.classified = false;
            l.run_reads = 0;
            l.next_prefetch_block = 0;
            l.issued_base = 0;
            l.window = R2_WINDOW_START;
            l.pending = @splat(0);
        }
    }
};

/// Two 2 s epochs; each lane counted once per epoch; `active = max(cur,
/// prev).max(1)`; NO decrement path (an inc/dec counter leaks upward when
/// entries vanish silently).
pub const StreamActivityGauge = struct {
    epoch_start_ns: u64 = 0,
    cur: u32 = 0,
    prev: u32 = 0,
    epoch: u64 = 1,

    pub fn touch(self: *StreamActivityGauge, lane: *StreamLane, now: u64) u32 {
        if (now -| self.epoch_start_ns >= STALE_NS) {
            self.prev = self.cur;
            self.cur = 0;
            self.epoch_start_ns = now;
            self.epoch += 1;
        }
        if (lane.last_counted_epoch != self.epoch) {
            lane.last_counted_epoch = self.epoch;
            self.cur += 1;
        }
        const active = @max(@max(self.cur, self.prev), 1);
        stats.prefetch_active_streams.set(active);
        return active;
    }
};

// ---------------------------------------------------------------------------
// §5.5 The engage governor — ProbeCore (the write pipeline's, verbatim).
// ---------------------------------------------------------------------------

pub const ProbeCore = struct {
    const Phase = enum(u8) { hold, probing };

    mul: u32 = PROBE_MUL_ONE,
    prev_mul: u32 = PROBE_MUL_ONE,
    phase: Phase = .hold,
    adopted_bps: u64 = 0,
    baseline_bps: u64 = 0,
    cooldown: u8 = 0,
    epoch_start_ns: u64 = 0,
    epoch_bytes: Atomic(u64) = .init(0),
    sat_marks: Atomic(u64) = .init(0),
    sat_seen: u64 = 0,
    roll_latch: Atomic(bool) = .init(false),

    /// Delivery = TOTAL completed fill bytes (demand + lane): an ahead lane
    /// that merely displaces demand fetches reads as dead gain.
    pub fn noteDelivered(self: *ProbeCore, bytes: u64) void {
        _ = self.epoch_bytes.fetchAdd(bytes, .monotonic);
    }

    pub fn noteSaturation(self: *ProbeCore) void {
        _ = self.sat_marks.fetchAdd(1, .monotonic);
    }

    pub fn governedDepth(self: *const ProbeCore) u32 {
        return probeGovernedDepth(self.mul);
    }

    /// The epoch tick: `saturated = sat_marks moved since the last roll`,
    /// `roll(now, saturated, headroom ∧ !pressure)`.
    pub fn tick(self: *ProbeCore, now: u64, headroom: bool, pressure: bool) void {
        if (now -| self.epoch_start_ns < PROBE_EPOCH_NS) return;
        if (self.roll_latch.cmpxchgStrong(false, true, .acquire, .monotonic) != null) return;
        defer self.roll_latch.store(false, .release);
        const marks = self.sat_marks.load(.monotonic);
        const saturated = marks != self.sat_seen;
        self.sat_seen = marks;
        self.roll(now, saturated, headroom and !pressure);
    }

    /// The CAS winner rolls: `bps = epoch_bytes × 1000 / elapsed_ms`.
    /// PROBING: `saturated ∧ bps ≥ baseline + max(baseline/16, 1)` ⇒ ADOPT
    /// (re-arm at once); else RETREAT to `prev_mul` (+ cooldown iff
    /// saturated), `backoffs += 1`. HOLD: `!saturated` ⇒ decay `mul − mul/8`
    /// toward 1.0; a collapse (`bps < adopted − adopted/8`) steps back
    /// `mul − mul/5` (senior to launching); cooldown ticks; `headroom ∧ mul
    /// < max ∧ bps > 0` ⇒ LAUNCH `mul + mul/4`, `ups += 1`.
    pub fn roll(self: *ProbeCore, now: u64, saturated: bool, headroom: bool) void {
        const elapsed_ms = @max((now -| self.epoch_start_ns) / std.time.ns_per_ms, 1);
        const bytes = self.epoch_bytes.swap(0, .monotonic);
        const bps = bytes * 1000 / elapsed_ms;
        self.epoch_start_ns = now;
        switch (self.phase) {
            .probing => {
                if (saturated and bps >= self.baseline_bps + @max(self.baseline_bps / 16, 1)) {
                    self.adopted_bps = bps;
                    self.baseline_bps = bps;
                    self.prev_mul = self.mul;
                    // Re-arm immediately: keep probing from the adopted level.
                    if (headroom and self.mul < PROBE_MUL_MAX) {
                        self.mul = @min(self.mul + self.mul / 4, PROBE_MUL_MAX);
                        stats.read_lane_depth_probe_ups.inc();
                    } else {
                        self.phase = .hold;
                    }
                } else {
                    self.mul = self.prev_mul;
                    self.phase = .hold;
                    if (saturated) self.cooldown = PROBE_COOLDOWN_EPOCHS;
                    stats.read_lane_depth_probe_backoffs.inc();
                }
            },
            .hold => {
                if (!saturated and self.mul > PROBE_MUL_ONE) self.mul = @max(self.mul - self.mul / 8, PROBE_MUL_ONE);
                const collapsed = self.mul > PROBE_MUL_ONE and self.adopted_bps > 0 and bps < self.adopted_bps - self.adopted_bps / 8;
                if (collapsed) {
                    self.mul = @max(self.mul - self.mul / 5, PROBE_MUL_ONE);
                    self.adopted_bps = bps;
                } else {
                    if (self.cooldown > 0) self.cooldown -= 1;
                    if (self.cooldown == 0 and headroom and self.mul < PROBE_MUL_MAX and bps > 0) {
                        self.prev_mul = self.mul;
                        self.baseline_bps = bps;
                        self.mul = @min(self.mul + @max(self.mul / 4, 1), PROBE_MUL_MAX);
                        self.phase = .probing;
                        stats.read_lane_depth_probe_ups.inc();
                    }
                }
            },
        }
        stats.read_lane_depth_target.set(self.governedDepth());
    }
};

// ---------------------------------------------------------------------------
// §5.3 / §5.5 The pipeline touch and the lane top-up over the Env seam.
// ---------------------------------------------------------------------------

/// What the issue paths need from the read path. Every call is a RAM probe
/// or a non-blocking spawn; nothing here awaits.
pub const Env = struct {
    ctx: *anyopaque,
    /// Resident in any tier, the hold, or in flight.
    resident: *const fn (ctx: *anyopaque, block: u64) bool,
    /// Spawn the prefetch fill of `block` for `lane_idx/generation`; false
    /// = refused (the caller rolls back).
    issue_prefetch: *const fn (ctx: *anyopaque, lane_idx: u8, generation: u32, block: u64) bool,
    /// Spawn the ahead-lane fill (ledger-invisible); false = refused.
    issue_lane: *const fn (ctx: *anyopaque, lane_idx: u8, generation: u32, block: u64) bool,
};

/// The per-request inputs of the touch.
pub const Touch = struct {
    offset: u64,
    len: u64,
    block_size: u64,
    start_block: u64,
    end_block: u64,
    total_blocks: u64,
    /// The reader caught the pipeline (its first key is in flight).
    will_wait: bool,
    lane_claim: bool,
    dest_leaseable: bool,
    level: budget.Level,
    now: u64,
};

pub const Config = struct {
    /// `SQUEEZEFS_READ_PREFETCH_WINDOW` resolved (0 = kill switch).
    window_cap: u32,
    share_pct: u64,
    hot_budget: u64,
    /// `SQUEEZEFS_READ_LANE`.
    lane_armed: bool,
    /// `SQUEEZEFS_READ_LANE_DEPTH` pin.
    depth_pin: ?u32,
    /// `effective_mem_budget / 8`.
    lane_budget_cap: u64,

    pub fn fromEnv(env: *const std.process.Environ.Map, hot_budget: u64, bs: u64, mem_budget: u64) Config {
        const share = knobs.intKnob("SQUEEZEFS_READ_PREFETCH_SHARE_PCT", u64, env);
        return .{
            .window_cap = resolveWindowCap(knobs.optIntKnob("SQUEEZEFS_READ_PREFETCH_WINDOW", u64, env), share, hot_budget, bs),
            .share_pct = share,
            .hot_budget = hot_budget,
            .lane_armed = knobs.boolKnob("SQUEEZEFS_READ_LANE", env),
            .depth_pin = knobs.optIntKnob("SQUEEZEFS_READ_LANE_DEPTH", u32, env),
            .lane_budget_cap = mem_budget / 8,
        };
    }
};

/// Per-file pipeline state: the lanes, the activity gauge, the governor.
pub const Pipeline = struct {
    lanes: StreamLanes = .init,
    gauge: StreamActivityGauge = .{},
    probe: ProbeCore = .{},
    cfg: Config,
    /// The hold's ahead-eviction count at the last probe tick (under the lanes lock).
    ahead_evictions_seen: u64 = 0,

    pub fn init(cfg: Config) Pipeline {
        return .{ .cfg = cfg };
    }

    /// `pipeline_touch`: once per striped request, BEFORE the serve probes.
    /// Returns the lane observed (null = no lane, nothing issued).
    pub fn touch(self: *Pipeline, env: Env, t: Touch) ?LaneRef {
        const cfg = self.cfg;
        self.lanes.lock.lock();
        defer self.lanes.lock.unlock();
        const ref = self.lanes.observeLocked(t.offset, t.len, t.lane_claim, t.now) orelse return null;
        const lane = &self.lanes.lanes[ref.idx];
        const generation = lane.generation;
        // Consume bookkeeping — per BLOCK (a per-request drain was 4× too fast).
        const issued_edge = lane.next_prefetch_block;
        const issued_base = lane.issued_base;
        const ce = lane.consumed_edge;
        if (t.end_block >= ce) {
            lane.consumed_edge = t.end_block + 1;
            var b = @max(ce, issued_base);
            while (b <= t.end_block and b < issued_edge) : (b += 1) {
                if (lane.unconsumed > 0) lane.unconsumed -= 1;
            }
            if (issued_base <= t.start_block and t.start_block < issued_edge) {
                const resident = env.resident(env.ctx, t.start_block);
                if (!resident and lane.isPending(t.start_block)) {
                    // The parked-task screen: NO VERDICT.
                } else if (!resident) {
                    stats.prefetch_evicted_unconsumed.inc();
                    lane.window = @max(lane.window / 2, R2_WINDOW_START);
                    const streak = @min(lane.detect_streak, QUIESCENCE_STREAK_CAP);
                    lane.detect_streak = streak + 1;
                    lane.suppress_until_edge = t.end_block + 1 + quiescenceSpan(streak);
                } else {
                    lane.detect_streak = 0;
                }
            }
        }
        if (t.dest_leaseable) self.lanes.dest_lease_ns = t.now;
        if (!ref.streaming or cfg.window_cap == 0) return ref;
        if (t.dest_leaseable or self.lanes.destLeaseFreshLocked(t.now)) return ref;
        if (t.level == .red) return ref;
        const overran = issued_edge > issued_base and t.end_block >= issued_edge;
        if (prefetchWindowGrows(t.will_wait, overran, lane.detect_streak, t.level == .green)) {
            if (t.will_wait) stats.prefetch_foreground_waits.inc();
            lane.window = @min(lane.window * 2, cfg.window_cap);
        }
        const active = self.gauge.touch(lane, t.now);
        const share = residentShare(cfg.share_pct, cfg.hot_budget, t.block_size, active);
        const plan_bound = @min(lane.window, cfg.window_cap);
        if (plan_bound > stats.prefetch_window_hwm.load()) stats.prefetch_window_hwm.set(plan_bound);
        const r2_declines = if (cfg.lane_armed) share < R2_WINDOW_START else share == 0;
        if (r2_declines) {
            self.laneTopUpLocked(env, lane, ref.idx, generation, active, t);
            return ref;
        }
        if (lane.next_prefetch_block <= t.end_block) {
            lane.next_prefetch_block = t.end_block + 1;
            lane.issued_base = t.end_block + 1;
            lane.pending = @splat(0);
        }
        if (lane.consumed_edge < lane.suppress_until_edge) return ref;
        while (prefetchIssueAdmits(lane.window, cfg.window_cap, share, lane.inflight, lane.unconsumed)) {
            const next = lane.next_prefetch_block;
            if (next >= t.total_blocks) break;
            lane.next_prefetch_block = next + 1;
            lane.inflight += 1;
            stats.prefetch_issued.inc();
            stats.prefetch_inflight_bytes.add(t.block_size);
            lane.markPending(next);
            if (!env.issue_prefetch(env.ctx, ref.idx, generation, next)) {
                lane.clearPending(next);
                lane.inflight -= 1;
                stats.prefetch_inflight_bytes.sub(t.block_size);
                stats.prefetch_wasted.inc();
                break;
            }
        }
        return ref;
    }

    /// The task's first poll: the screen bit clears (the block has a
    /// verdict from here on).
    pub fn taskStarted(self: *Pipeline, lane_idx: u8, block: u64) void {
        self.lanes.lock.lock();
        defer self.lanes.lock.unlock();
        self.lanes.lanes[lane_idx].clearPending(block);
    }

    /// The prefetch task's settle: `completed ∧ live ⇒ completed += 1;
    /// block ≥ consumed_edge ⇒ unconsumed += 1`; else wasted. `issued ≡
    /// completed + wasted`.
    pub fn settlePrefetch(self: *Pipeline, lane_idx: u8, generation: u32, block: u64, completed: bool, bs: u64) void {
        self.lanes.lock.lock();
        defer self.lanes.lock.unlock();
        const lane = &self.lanes.lanes[lane_idx];
        lane.clearPending(block);
        if (lane.inflight > 0) lane.inflight -= 1;
        stats.prefetch_inflight_bytes.sub(bs);
        const live = lane.generation == generation;
        if (completed and live) {
            stats.prefetch_completed.inc();
            if (block >= lane.consumed_edge) lane.unconsumed += 1;
        } else {
            stats.prefetch_wasted.inc();
        }
    }

    /// The lane task's settle; a LIVE completed settle re-drives the top-up
    /// from `consumed_edge − 1` (completion-driven refill).
    pub fn settleLane(self: *Pipeline, env: Env, lane_idx: u8, generation: u32, block: u64, completed: bool, bs: u64, total_blocks: u64, level: budget.Level, now: u64) void {
        self.lanes.lock.lock();
        defer self.lanes.lock.unlock();
        const lane = &self.lanes.lanes[lane_idx];
        lane.clearPending(block);
        if (lane.rl_inflight > 0) lane.rl_inflight -= 1;
        lane.rl_inflight_bytes -|= bs;
        stats.read_lane_inflight_bytes.sub(bs);
        const live = lane.generation == generation;
        if (!(completed and live)) {
            stats.read_lane_wasted.inc();
            return;
        }
        const active = self.gauge.touch(lane, now);
        const t: Touch = .{ .offset = 0, .len = 0, .block_size = bs, .start_block = lane.consumed_edge -| 1, .end_block = lane.consumed_edge -| 1, .total_blocks = total_blocks, .will_wait = false, .lane_claim = false, .dest_leaseable = false, .level = level, .now = now };
        self.laneTopUpLocked(env, lane, lane_idx, generation, active, t);
    }

    /// `read_lane_top_up`: the marginal-issue walk.
    fn laneTopUpLocked(self: *Pipeline, env: Env, lane: *StreamLane, lane_idx: u8, generation: u32, active: u32, t: Touch) void {
        const cfg = self.cfg;
        if (!cfg.lane_armed or t.block_size <= 256 * derive.KiB) return;
        if (self.lanes.destLeaseFreshLocked(t.now)) return;
        // File-level single issuer.
        if (self.lanes.issue_owner) |o| {
            if (o != lane_idx and t.now -| self.lanes.issue_owner_ns < STALE_NS) return;
        }
        self.lanes.issue_owner = lane_idx;
        self.lanes.issue_owner_ns = t.now;
        const red = t.level == .red;
        const depth = readLaneDepthBlocks(cfg.depth_pin, self.probe.governedDepth(), t.block_size, active, red, cfg.lane_budget_cap);
        if (cfg.depth_pin == null) {
            if (t.will_wait or depth == 0) self.probe.noteSaturation();
            const headroom = !red and depth < cfg.lane_budget_cap / t.block_size / @max(active, 1);
            // Pressure = the hold's ahead-eviction counter moved since the
            // last tick (deposits dying FIFO-unconsumed: zero headroom).
            const ahead = @import("read_hold.zig").stats.read_lane_hold_ahead_evictions.load();
            const pressure = ahead != self.ahead_evictions_seen;
            self.ahead_evictions_seen = ahead;
            self.probe.tick(t.now, headroom, pressure);
        }
        stats.read_lane_depth_target.set(depth);
        if (depth == 0) return;
        if (lane.next_prefetch_block <= t.end_block) {
            lane.next_prefetch_block = t.end_block + 1;
            lane.issued_base = t.end_block + 1;
            lane.pending = @splat(0);
        }
        if (lane.consumed_edge < lane.suppress_until_edge) return;
        const derived_cap = derivedPrefetchWindowCap(cfg.share_pct, cfg.hot_budget, t.block_size);
        const examine_cap: u64 = @max(derived_cap, depth);
        const horizon = t.end_block + 1 + examine_cap;
        var examined: u64 = 0;
        while (laneIssueAdmits(lane.rl_inflight, depth, lane.rl_inflight_bytes, t.block_size, cfg.lane_budget_cap)) {
            if (examined >= examine_cap) break;
            const next = lane.next_prefetch_block;
            if (next >= t.total_blocks or next > horizon) break;
            if (env.resident(env.ctx, next)) {
                lane.next_prefetch_block = next + 1;
                examined += 1;
                stats.read_lane_covered_skips.inc();
                continue;
            }
            lane.next_prefetch_block = next + 1;
            lane.rl_inflight += 1;
            lane.rl_inflight_bytes += t.block_size;
            stats.read_lane_inflight_bytes.add(t.block_size);
            lane.markPending(next);
            if (!env.issue_lane(env.ctx, lane_idx, generation, next)) {
                lane.clearPending(next);
                lane.rl_inflight -= 1;
                lane.rl_inflight_bytes -= t.block_size;
                stats.read_lane_inflight_bytes.sub(t.block_size);
                stats.read_lane_wasted.inc();
                break;
            }
        }
        if (lane.rl_inflight >= depth and cfg.depth_pin == null) self.probe.noteSaturation();
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "tie: window tables — cap derivation and rails, issue admission, growth gate, resident share without floor, quiescence span" {
    const MiB = derive.MiB;
    try testing.expectEqual(@as(u32, 16), derivedPrefetchWindowCap(50, 128 * MiB, 4 * MiB));
    try testing.expectEqual(@as(u32, 4), derivedPrefetchWindowCap(50, 8 * MiB, 4 * MiB));
    try testing.expectEqual(@as(u32, 4096), derivedPrefetchWindowCap(100, 64 * derive.GiB, 4096));
    try testing.expectEqual(@as(u32, 4), derivedPrefetchWindowCap(50, 128 * MiB, 0));
    try testing.expectEqual(@as(u32, 0), resolveWindowCap(0, 50, 128 * MiB, 4 * MiB));
    try testing.expectEqual(@as(u32, 4096), resolveWindowCap(1 << 20, 50, 128 * MiB, 4 * MiB));
    try testing.expectEqual(@as(u32, 7), resolveWindowCap(7, 50, 128 * MiB, 4 * MiB));
    try testing.expect(prefetchIssueAdmits(4, 16, 8, 1, 2));
    try testing.expect(!prefetchIssueAdmits(4, 16, 0, 0, 0));
    try testing.expect(!prefetchIssueAdmits(4, 16, 2, 0, 2));
    try testing.expect(!prefetchIssueAdmits(4, 16, 8, 2, 2));
    try testing.expect(prefetchIssueAdmits(8, 4, 8, 1, 2) and !prefetchIssueAdmits(8, 4, 8, 2, 2));
    try testing.expect(prefetchWindowGrows(true, false, 0, true));
    try testing.expect(prefetchWindowGrows(false, true, 0, true));
    try testing.expect(!prefetchWindowGrows(false, true, 1, true));
    try testing.expect(!prefetchWindowGrows(true, true, 0, false));
    try testing.expectEqual(@as(u64, 8), residentShare(50, 128 * MiB, 4 * MiB, 2));
    try testing.expectEqual(@as(u64, 0), residentShare(50, 8 * MiB, 4 * MiB, 4));
    try testing.expectEqual(@as(u64, 2), quiescenceSpan(0));
    try testing.expectEqual(@as(u64, 64), quiescenceSpan(5));
    try testing.expectEqual(@as(u64, 64), quiescenceSpan(9));
}

test "tie: depth derivations — Red ⇒ 0, cap/bs/streams; lane issue admits; the probe depth table 64→0 … 195→8" {
    const MiB = derive.MiB;
    try testing.expectEqual(@as(u32, 0), readLaneDepthBlocks(8, 2, 4 * MiB, 1, true, derive.GiB));
    try testing.expectEqual(@as(u32, 8), readLaneDepthBlocks(8, 2, 4 * MiB, 1, false, derive.GiB));
    try testing.expectEqual(@as(u32, 2), readLaneDepthBlocks(null, 2, 4 * MiB, 1, false, derive.GiB));
    try testing.expectEqual(@as(u32, 4), readLaneDepthBlocks(64, 2, 4 * MiB, 16, false, 256 * MiB));
    try testing.expect(laneIssueAdmits(1, 2, 4 * MiB, 4 * MiB, 16 * MiB));
    try testing.expect(!laneIssueAdmits(2, 2, 0, 4 * MiB, 16 * MiB));
    try testing.expect(!laneIssueAdmits(0, 2, 13 * MiB, 4 * MiB, 16 * MiB));
    const table = [_][2]u32{ .{ 64, 0 }, .{ 80, 1 }, .{ 100, 2 }, .{ 125, 3 }, .{ 156, 5 }, .{ 195, 8 } };
    for (table) |row| try testing.expectEqual(row[1], probeGovernedDepth(row[0]));
}

test "lanes: 4 contiguous requests classify (counted once); two interleaved readers classify in two lanes; a warm serve never mints; 16 foreign declassify; a stale lane is reclaimed" {
    var lanes: StreamLanes = .init;
    const MiB = derive.MiB;
    const t0: u64 = 10 * STALE_NS;
    const before = stats.read_streams_classified.load();
    // Reader A at 0, reader B at 64 MiB, interleaved 1 MiB requests.
    var i: u64 = 0;
    var a_ref: ?LaneRef = null;
    var b_ref: ?LaneRef = null;
    while (i < 4) : (i += 1) {
        a_ref = lanes.observe(i * MiB, MiB, true, t0);
        b_ref = lanes.observe(64 * MiB + i * MiB, MiB, true, t0);
    }
    try testing.expect(a_ref.?.streaming and b_ref.?.streaming);
    try testing.expect(a_ref.?.idx != b_ref.?.idx);
    try testing.expectEqual(before + 2, stats.read_streams_classified.load());
    try testing.expect(lanes.anyStreamingFresh(t0));
    try testing.expect(!lanes.anyStreamingFresh(t0 + STALE_NS));
    // Membership tolerance: a reordered request within ±64 × len joins.
    const r = lanes.observe(4 * MiB + 3 * MiB, MiB, true, t0).?;
    try testing.expectEqual(a_ref.?.idx, r.idx);
    try testing.expect(r.streaming);
    // A random 4 KiB read far away does not join; `claim = false` mints nothing.
    try testing.expect(lanes.observe(900 * MiB, 4096, false, t0) == null);
    // 16 foreign claims (two free lanes first, then stale-refused) declassify every lane.
    i = 0;
    while (i < FOREIGN_DECLASSIFY_RUN) : (i += 1) _ = lanes.observe(500 * MiB + i * 7 * MiB, 4096, true, t0);
    try testing.expect(!lanes.anyStreamingFresh(t0));
    // A stale lane (idle ≥ 2 s) is reclaimed by a new claim.
    const late = lanes.observe(700 * MiB, MiB, true, t0 + 3 * STALE_NS).?;
    try testing.expect(!late.streaming);
    try testing.expectEqual(@as(u32, 1), lanes.lanes[late.idx].run_reads);
}

test "the activity gauge counts a lane once per epoch and never decrements below max(cur, prev, 1)" {
    var g: StreamActivityGauge = .{};
    var l0: StreamLane = .{};
    var l1: StreamLane = .{};
    const t0: u64 = STALE_NS;
    try testing.expectEqual(@as(u32, 1), g.touch(&l0, t0));
    try testing.expectEqual(@as(u32, 1), g.touch(&l0, t0));
    try testing.expectEqual(@as(u32, 2), g.touch(&l1, t0));
    // Next epoch: prev = 2 holds the count up while cur rebuilds.
    try testing.expectEqual(@as(u32, 2), g.touch(&l0, t0 + STALE_NS));
    try testing.expectEqual(@as(u32, 1), g.touch(&l0, t0 + 3 * STALE_NS));
}

test "ProbeCore: launch → adopt → retreat with cooldown; collapse steps back; decay toward 1.0 when unsaturated" {
    var p: ProbeCore = .{};
    var now: u64 = PROBE_EPOCH_NS;
    const ups0 = stats.read_lane_depth_probe_ups.load();
    const backs0 = stats.read_lane_depth_probe_backoffs.load();
    // Epoch 1: delivery with headroom ⇒ LAUNCH (+¼ ⇒ 80 ⇒ depth 1).
    p.noteDelivered(1000);
    p.roll(now, false, true);
    try testing.expectEqual(@as(u32, 80), p.mul);
    try testing.expectEqual(@as(u32, 1), p.governedDepth());
    try testing.expectEqual(ups0 + 1, stats.read_lane_depth_probe_ups.load());
    // Epoch 2: saturated and delivery grew ≥ 1/16 ⇒ ADOPT and re-arm (100).
    now += PROBE_EPOCH_NS;
    p.noteDelivered(1200);
    p.roll(now, true, true);
    try testing.expectEqual(@as(u32, 100), p.mul);
    try testing.expectEqual(@as(u32, 2), p.governedDepth());
    // Epoch 3: saturated but no gain ⇒ RETREAT to 80 + cooldown.
    now += PROBE_EPOCH_NS;
    p.noteDelivered(1200);
    p.roll(now, true, true);
    try testing.expectEqual(@as(u32, 80), p.mul);
    try testing.expectEqual(PROBE_COOLDOWN_EPOCHS, p.cooldown);
    try testing.expectEqual(backs0 + 1, stats.read_lane_depth_probe_backoffs.load());
    // Cooldown epochs: no launch even with headroom; saturated holds the
    // level for N − 1 epochs (the tick that reaches 0 may launch).
    var k: u8 = 0;
    while (k < PROBE_COOLDOWN_EPOCHS - 1) : (k += 1) {
        now += PROBE_EPOCH_NS;
        p.noteDelivered(1200);
        p.roll(now, true, true);
        try testing.expectEqual(@as(u32, 80), p.mul);
    }
    // Cooldown over: a launch fires.
    now += PROBE_EPOCH_NS;
    p.noteDelivered(1200);
    p.roll(now, true, true);
    try testing.expectEqual(@as(u32, 100), p.mul);
    // A collapse (bps < 7/8 of adopted) in HOLD steps back ×0.8.
    now += PROBE_EPOCH_NS;
    p.noteDelivered(100);
    p.roll(now, true, true); // probing: no gain ⇒ retreat to 80, cooldown
    try testing.expectEqual(@as(u32, 80), p.mul);
    now += PROBE_EPOCH_NS;
    p.noteDelivered(100);
    p.roll(now, true, true);
    try testing.expectEqual(@as(u32, 64), p.mul);
    // Unsaturated decay toward 1.0.
    p.mul = 128;
    p.phase = .hold;
    p.adopted_bps = 0;
    now += PROBE_EPOCH_NS;
    p.roll(now, false, false);
    try testing.expectEqual(@as(u32, 112), p.mul);
    // The tick is a no-op inside the epoch and rolls past it.
    p.epoch_start_ns = now;
    p.tick(now + 1, true, false);
    try testing.expectEqual(@as(u32, 112), p.mul);
    p.tick(now + PROBE_EPOCH_NS, false, false);
    try testing.expectEqual(@as(u32, 98), p.mul);
}

const FakeEnv = struct {
    issued: std.ArrayList(u64) = .empty,
    lane_issued: std.ArrayList(u64) = .empty,
    resident_blocks: std.ArrayList(u64) = .empty,
    refuse: bool = false,

    fn resident(ctx: *anyopaque, block: u64) bool {
        const self: *FakeEnv = @ptrCast(@alignCast(ctx));
        for (self.resident_blocks.items) |b| if (b == block) return true;
        return false;
    }
    fn issuePrefetch(ctx: *anyopaque, _: u8, _: u32, block: u64) bool {
        const self: *FakeEnv = @ptrCast(@alignCast(ctx));
        if (self.refuse) return false;
        self.issued.append(testing.allocator, block) catch unreachable;
        return true;
    }
    fn issueLane(ctx: *anyopaque, _: u8, _: u32, block: u64) bool {
        const self: *FakeEnv = @ptrCast(@alignCast(ctx));
        if (self.refuse) return false;
        self.lane_issued.append(testing.allocator, block) catch unreachable;
        return true;
    }
    fn env(self: *FakeEnv) Env {
        return .{ .ctx = self, .resident = &resident, .issue_prefetch = &issuePrefetch, .issue_lane = &issueLane };
    }
    fn deinit(self: *FakeEnv) void {
        self.issued.deinit(testing.allocator);
        self.lane_issued.deinit(testing.allocator);
        self.resident_blocks.deinit(testing.allocator);
    }
};

fn touchAt(block: u64, bs: u64, will_wait: bool, now: u64) Touch {
    return .{ .offset = block * bs, .len = bs, .block_size = bs, .start_block = block, .end_block = block, .total_blocks = 1000, .will_wait = will_wait, .lane_claim = true, .dest_leaseable = false, .level = .green, .now = now };
}

test "AIMD: classification issues the start window of 2; a foreground wait doubles it; an evicted-before-consumed halves it and arms quiescence; a pending block expresses no verdict; issued ≡ completed + wasted" {
    const bs: u64 = 4 * derive.MiB;
    var fe: FakeEnv = .{};
    defer fe.deinit();
    const cfg: Config = .{ .window_cap = 16, .share_pct = 50, .hot_budget = 256 * derive.MiB, .lane_armed = false, .depth_pin = null, .lane_budget_cap = derive.GiB / 8 };
    var p = Pipeline.init(cfg);
    const t0: u64 = 10 * STALE_NS;
    const issued0 = stats.prefetch_issued.load();
    const completed0 = stats.prefetch_completed.load();
    const wasted0 = stats.prefetch_wasted.load();
    // Blocks 0..2: not yet classified, nothing issued.
    var b: u64 = 0;
    while (b < 3) : (b += 1) _ = p.touch(fe.env(), touchAt(b, bs, false, t0));
    try testing.expectEqual(@as(usize, 0), fe.issued.items.len);
    // Block 3 classifies: the plan rebases to 4 and issues the start window (2): blocks 4, 5.
    const ref = p.touch(fe.env(), touchAt(3, bs, false, t0)).?;
    try testing.expect(ref.streaming);
    try testing.expectEqualSlices(u64, &.{ 4, 5 }, fe.issued.items);
    const lane = &p.lanes.lanes[ref.idx];
    try testing.expectEqual(@as(u32, 2), lane.window);
    try testing.expect(lane.isPending(4) and lane.isPending(5));
    // The tasks complete: both land unconsumed (ahead of the consume edge).
    p.taskStarted(ref.idx, 4);
    p.settlePrefetch(ref.idx, ref.generation, 4, true, bs);
    p.settlePrefetch(ref.idx, ref.generation, 5, true, bs);
    try testing.expectEqual(@as(u32, 2), lane.unconsumed);
    try fe.resident_blocks.append(testing.allocator, 4);
    try fe.resident_blocks.append(testing.allocator, 5);
    // Block 4 with a foreground wait: window ×2 ⇒ 4; the consume drains one
    // (unconsumed 1); `in_flight + unconsumed < 4` admits three: 6, 7, 8.
    _ = p.touch(fe.env(), touchAt(4, bs, true, t0));
    try testing.expectEqual(@as(u32, 4), lane.window);
    try testing.expectEqual(@as(u32, 1), lane.unconsumed);
    try testing.expectEqualSlices(u64, &.{ 4, 5, 6, 7, 8 }, fe.issued.items);
    // Block 6 was issued but is NOT resident and still pending (the task
    // never polled): no verdict, the window holds.
    const evicted0 = stats.prefetch_evicted_unconsumed.load();
    _ = p.touch(fe.env(), touchAt(6, bs, false, t0));
    try testing.expectEqual(evicted0, stats.prefetch_evicted_unconsumed.load());
    try testing.expectEqual(@as(u32, 4), lane.window);
    // Block 7: the task polled (bit cleared) and the block is not resident
    // ⇒ evicted-before-consumed: window ÷2, quiescence armed.
    p.taskStarted(ref.idx, 7);
    _ = p.touch(fe.env(), touchAt(7, bs, false, t0));
    try testing.expectEqual(evicted0 + 1, stats.prefetch_evicted_unconsumed.load());
    try testing.expectEqual(@as(u32, 2), lane.window);
    try testing.expectEqual(@as(u64, 7 + 1 + 2), lane.suppress_until_edge);
    // Settle 6 (a stale generation) and 7 (a failed fill) wasted, 8
    // completed, to close the ledger.
    p.settlePrefetch(ref.idx, ref.generation +% 1, 6, true, bs);
    p.settlePrefetch(ref.idx, ref.generation, 7, false, bs);
    p.settlePrefetch(ref.idx, ref.generation, 8, true, bs);
    // The touch of block 6 (in flight 3, unconsumed drained to 0) admitted
    // one more issue (9) under the window of 4.
    try testing.expectEqualSlices(u64, &.{ 4, 5, 6, 7, 8, 9 }, fe.issued.items);
    p.settlePrefetch(ref.idx, ref.generation, 9, true, bs);
    const issued = stats.prefetch_issued.load() - issued0;
    try testing.expectEqual(issued, (stats.prefetch_completed.load() - completed0) + (stats.prefetch_wasted.load() - wasted0));
    try testing.expectEqual(@as(u64, 0), stats.prefetch_inflight_bytes.load());
    // A refused spawn rolls back and counts wasted.
    fe.refuse = true;
    const w0 = stats.prefetch_wasted.load();
    var p2 = Pipeline.init(cfg);
    b = 0;
    while (b < 4) : (b += 1) _ = p2.touch(fe.env(), touchAt(b, bs, false, t0));
    try testing.expectEqual(w0 + 1, stats.prefetch_wasted.load());
    try testing.expectEqual(@as(u64, 0), stats.prefetch_inflight_bytes.load());
    // The kill switch: cap 0 issues nothing.
    var off = Pipeline.init(.{ .window_cap = 0, .share_pct = 50, .hot_budget = 256 * derive.MiB, .lane_armed = false, .depth_pin = null, .lane_budget_cap = 0 });
    var fe_off: FakeEnv = .{};
    defer fe_off.deinit();
    b = 0;
    while (b < 6) : (b += 1) _ = off.touch(fe_off.env(), touchAt(b, bs, false, t0));
    try testing.expectEqual(@as(usize, 0), fe_off.issued.items.len);
}

test "the ahead lane: zero resident share engages the lane (R2 declines), covered blocks are skipped without spending depth, Red pins depth 0, a pinned depth issues exactly that many" {
    const bs: u64 = 4 * derive.MiB;
    var fe: FakeEnv = .{};
    defer fe.deinit();
    // Share 0 (a 2-block budget over 4 lanes) with the lane armed and a depth pin of 3.
    var p = Pipeline.init(.{ .window_cap = 16, .share_pct = 50, .hot_budget = 8 * derive.MiB, .lane_armed = true, .depth_pin = 3, .lane_budget_cap = derive.GiB });
    const t0: u64 = 10 * STALE_NS;
    try fe.resident_blocks.append(testing.allocator, 5);
    var b: u64 = 0;
    while (b < 4) : (b += 1) _ = p.touch(fe.env(), touchAt(b, bs, false, t0));
    // Nothing on the R2 arm; the lane issued 3 (depth pin), skipping resident 5.
    try testing.expectEqual(@as(usize, 0), fe.issued.items.len);
    try testing.expectEqualSlices(u64, &.{ 4, 6, 7 }, fe.lane_issued.items);
    try testing.expectEqual(@as(u64, 3 * bs), stats.read_lane_inflight_bytes.load());
    // A live completed settle re-drives the walk (completion-driven refill).
    const lane_idx: u8 = 0;
    const gen = p.lanes.lanes[lane_idx].generation;
    p.settleLane(fe.env(), lane_idx, gen, 4, true, bs, 1000, .green, t0);
    try testing.expectEqualSlices(u64, &.{ 4, 6, 7, 8 }, fe.lane_issued.items);
    // Red: depth 0, nothing more.
    p.settleLane(fe.env(), lane_idx, gen, 6, true, bs, 1000, .red, t0);
    try testing.expectEqual(@as(usize, 4), fe.lane_issued.items.len);
    p.settleLane(fe.env(), lane_idx, gen, 7, true, bs, 1000, .red, t0);
    p.settleLane(fe.env(), lane_idx, gen, 8, false, bs, 1000, .green, t0);
    try testing.expectEqual(@as(u64, 0), stats.read_lane_inflight_bytes.load());
    // Lever off: never issues.
    var off = Pipeline.init(.{ .window_cap = 16, .share_pct = 50, .hot_budget = 8 * derive.MiB, .lane_armed = false, .depth_pin = 3, .lane_budget_cap = derive.GiB });
    const n = fe.lane_issued.items.len;
    b = 0;
    while (b < 6) : (b += 1) _ = off.touch(fe.env(), touchAt(b, bs, false, t0));
    try testing.expectEqual(n, fe.lane_issued.items.len);
}
