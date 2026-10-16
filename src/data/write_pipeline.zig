//! The write-pipeline depth governor (charter §6.2 "Write pipeline depth
//! governor": a runtime-derived BDP target, a BBR-flavoured probe-up,
//! in-flight bytes a non-sheddable budget component, the overlay store takes
//! the same permit, `write_pipeline_phase_ns` always-on; `docs/porting/
//! data_write_path.md` §9). PORTABLE: the admission core, the per-lane BDP
//! learner, the probe state machine and the exact-sum phase family; the park
//! is a `sync.Notify` deadline wait over `std.Io` (decision 0001).
//!
//! No fixed depth anywhere: `target = Σ_lanes max(FLOOR × bs, bdp × HEADROOM)
//! × mul/64`, Red-clamped to the measured drain (never a fixed aggregate —
//! finding 39: a 32 MiB clamp starved a 10-lane ~12 ms fabric to ⅛ of its
//! drain, an 8× collapse), capped at `budget / 4`, floored at one block.
//! Field basis (`write_pipeline.rs:62-66`): pure-BDP sustained 11.6 GB/s at
//! its own BDP; forced depth 64 = 13.7 GB/s (+18 %) against 16.6 GB/s raw —
//! the headroom the probe discovers.
//!
//! The empty-pipe bypass predicate is `cur == 0` on the BYTES word — the
//! same word the CAS charges; the blocks counter LAGS and admitted two
//! racing oversized bypassers (`write_pipeline_model.zig` proves the
//! weakening). Wake-liveness is not modelled: `notifyWaiters` stores no
//! permit, so the 5 ms tick is the liveness backstop BY DESIGN and
//! `write_pipeline_admission_tick_wakes` growing while completions flow is a
//! lost wake (PERF-13).
//!
//! Lanes are indexed by the router's dense volume ordinal (Q11: never a
//! backend-id string hash per completion); the Red level is read straight
//! from `mem.Budget`. The phase family is EXACT-SUM (Q3): ten residence legs
//! partition `total` to the nanosecond.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const knobs = @import("../core/knobs.zig");
const derive = @import("../core/derive.zig");
const budget = @import("../mem/budget.zig");
const sync_notify = @import("../sync/notify.zig");
const router = @import("router.zig");

// ---------------------------------------------------------------------------
// Constants (every one with its WHY; §11.3).
// ---------------------------------------------------------------------------

/// Cold posture only (no lane has measured yet) and Red's LOWER bound: eight
/// blocks keeps one 4 MiB stream's kernel-split cohort (4 × 1 MiB) in flight
/// twice over before the first window rolls.
pub const FLOOR_BLOCKS_PER_LANE: u64 = 8;
/// `target ≥ 3 × BDP`: absorbs service-time variance and the non-device legs
/// (crypto, allocate, publish) so the DEVICE leg stays ≥ 1 × BDP.
pub const HEADROOM: u64 = 3;
/// The payload-buffer cap shape: the pipeline may hold at most a quarter of
/// the budget in flight (the transport arena has the same bound).
pub const BUDGET_CAP_DIVISOR: u64 = 4;
/// Two orders above a device service time (~1–2 ms on fabric): a window
/// sees hundreds of completions, so its bandwidth sample is a rate, not a
/// burst.
pub const WINDOW_MS: u64 = 250;
/// The liveness backstop for a target change with no completion to carry
/// the wake; never the transport (module doc).
pub const ADMIT_TICK_MS: u64 = 5;
/// Two governor windows per probe epoch: a full floor→32× ramp at +¼ per
/// adopted epoch ≈ 15 epochs completes in seconds.
pub const PROBE_EPOCH_MS: u64 = 500;
/// Q6 fixed point: 64 = 1.0.
pub const PROBE_MUL_ONE: u64 = 64;
/// 32× is a runaway guard only — the budget cap is the operative bound.
pub const PROBE_MUL_MAX: u64 = PROBE_MUL_ONE * 32;
/// After a dead-gain retreat, hold this many epochs: bounds the dead-gain
/// tax to ~1/(N+1) of epochs.
pub const PROBE_COOLDOWN_EPOCHS: u32 = 8;

/// `max(1, mem_budget / BUDGET_CAP_DIVISOR)`.
pub fn capBytes(budget_bytes: u64) u64 {
    return @max(1, budget_bytes / BUDGET_CAP_DIVISOR);
}

// ---------------------------------------------------------------------------
// Stats.
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var write_pipeline_inflight_bytes: counters.Gauge = .{};
    pub var write_pipeline_inflight_blocks: counters.Gauge = .{};
    pub var write_pipeline_admission_waits: counters.ShardedCounter = .{};
    /// Tripwire-class: growth while completions flow = a lost wake.
    pub var write_pipeline_admission_tick_wakes: counters.ShardedCounter = .{};
    pub var write_pipeline_depth_target: counters.Gauge = .{};
    pub var write_pipeline_depth_target_base: counters.Gauge = .{};
    pub var write_pipeline_depth_probe_ups: counters.ShardedCounter = .{};
    pub var write_pipeline_depth_probe_backoffs: counters.ShardedCounter = .{};
    /// An upload a newer write superseded that PUBLISHED its snapshot (the
    /// entry stays parked with the newer pages; decision 0003 §AJ.7).
    pub var write_pipeline_superseded_publishes: counters.ShardedCounter = .{};
    pub var write_pipeline_fence_drops: counters.Tripwire = .init("write_pipeline_fence_drops");
    pub var write_pipeline_stay_parked: counters.ShardedCounter = .{};
    pub var write_pipeline_published: counters.ShardedCounter = .{};
    pub var write_pipeline_discards: counters.ShardedCounter = .{};
    /// A second flush of a block whose upload was already in flight waited
    /// for that upload's settle instead of minting a second block.
    pub var write_pipeline_joins: counters.ShardedCounter = .{};
    /// A flush OBSERVED another task's upload of its block in flight and
    /// parked on the table's settle (counted at the observation, before the
    /// wait: the deterministic rendezvous of "the joiner is in the join").
    pub var write_pipeline_join_waits: counters.ShardedCounter = .{};
    /// `write_pipeline_phase_ns[phase]`: the exact-sum residence family.
    pub var write_pipeline_phase_ns: [Phase.count]counters.ShardedLatencyHistogram = @splat(.{});
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "write_pipeline_inflight_bytes", "write_pipeline_inflight_blocks", "write_pipeline_depth_target", "write_pipeline_depth_target_base" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    inline for (.{ "write_pipeline_admission_waits", "write_pipeline_admission_tick_wakes", "write_pipeline_depth_probe_ups", "write_pipeline_depth_probe_backoffs", "write_pipeline_superseded_publishes", "write_pipeline_stay_parked", "write_pipeline_published", "write_pipeline_discards", "write_pipeline_joins", "write_pipeline_join_waits" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"write_pipeline_fence_drops\":{d},\"write_pipeline_phase_ns\":{{", .{counters.tripwireCount("write_pipeline_fence_drops")});
    inline for (@typeInfo(Phase).@"enum".field_names, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{name});
        try stats.write_pipeline_phase_ns[i].snapshot().writeJson(w);
    }
    try w.writeAll("}}");
}

// ---------------------------------------------------------------------------
// §9.5 The exact-sum phase family (Q3).
// ---------------------------------------------------------------------------

/// The ten residence legs that partition `total`: every span between two
/// boundaries belongs to exactly one leg; `other` is the span after the
/// last named mark.
pub const Phase = enum(u8) {
    admit_wait,
    detach_lag,
    lock_wait,
    crypto,
    allocate,
    dma,
    publish,
    displaced_free,
    inval_tail,
    other,
    total,

    pub const count = @typeInfo(Phase).@"enum".field_names.len;
    pub const legs = count - 1;
};

/// A value on the task's frame: `mark(p)` attributes the span since the
/// previous boundary to `p`; `finish` records every leg (0 for the unmarked)
/// and `total = now − t0`, so `Σ legs ≡ total` on every exit.
pub const Prof = struct {
    t0: u64,
    last: u64,
    legs: [Phase.legs]u64 = @splat(0),

    pub fn start() Prof {
        const now = clock.phaseNs();
        return .{ .t0 = now, .last = now };
    }

    pub fn startAt(now: u64) Prof {
        return .{ .t0 = now, .last = now };
    }

    pub fn mark(self: *Prof, p: Phase) void {
        self.markAt(p, clock.phaseNs());
    }

    pub fn markAt(self: *Prof, p: Phase, now: u64) void {
        assert(p != .total);
        const t = @max(now, self.last);
        self.legs[@backingInt(p)] += t - self.last;
        self.last = t;
    }

    pub fn finish(self: *Prof) void {
        self.finishAt(clock.phaseNs());
    }

    pub fn finishAt(self: *Prof, now: u64) void {
        const t = @max(now, self.last);
        self.legs[@backingInt(Phase.other)] += t - self.last;
        self.last = t;
        for (self.legs, 0..) |ns, i| stats.write_pipeline_phase_ns[i].record(ns);
        stats.write_pipeline_phase_ns[@backingInt(Phase.total)].record(t - self.t0);
    }
};

/// The task-counting contract: every pipeline task ends in exactly one.
pub const Disposition = enum {
    published,
    /// A transient (fencing token expired, a device refusal): the block
    /// stays parked and rides the next pass.
    stay_parked,
    /// `WriterGuardFenced`: publish nothing, free nothing (must-stay-0).
    fence_drop,
    /// The entry vanished (truncate/unlink) or the ino is gone.
    discarded,
    /// Another task's upload of this block was in flight: this one waited
    /// for its settle and found the entry gone (published or discarded by
    /// that task) — nothing minted, nothing published twice.
    joined,
};

pub fn countDisposition(d: Disposition) void {
    switch (d) {
        .published => stats.write_pipeline_published.inc(),
        .stay_parked => stats.write_pipeline_stay_parked.inc(),
        .fence_drop => stats.write_pipeline_fence_drops.fire("a pipeline upload met the writer-guard fence"),
        .discarded => stats.write_pipeline_discards.inc(),
        .joined => stats.write_pipeline_joins.inc(),
    }
}

// ---------------------------------------------------------------------------
// §9.1 The admission core (model-checked).
// ---------------------------------------------------------------------------

pub const Attempt = enum { admitted, full, raced };

pub const AdmissionCore = struct {
    inflight_bytes: Atomic(u64) align(std.atomic.cache_line) = .init(0),
    inflight_blocks: Atomic(u64) align(std.atomic.cache_line) = .init(0),

    /// ONE attempt: `cur = bytes`; `cur != 0 ∧ cur + n > target ⇒ full`;
    /// CAS `cur → cur + n` (AcqRel: the release pairs with the acquire of
    /// a racing attempt's CAS — a permit's charge is visible before the
    /// next charge is judged) ⇒ admitted (+1 block); else raced. The
    /// bypass judges the bytes word (module doc).
    pub fn tryAdmitOnce(self: *AdmissionCore, n: u64, target: u64) Attempt {
        const cur = self.inflight_bytes.load(.monotonic);
        if (cur != 0 and cur + n > target) return .full;
        if (self.inflight_bytes.cmpxchgWeak(cur, cur + n, .acq_rel, .monotonic) != null) return .raced;
        _ = self.inflight_blocks.fetchAdd(1, .monotonic);
        stats.write_pipeline_inflight_bytes.add(n);
        stats.write_pipeline_inflight_blocks.add(1);
        return .admitted;
    }

    /// Exactly once per permit (the permit's `complete`).
    pub fn release(self: *AdmissionCore, n: u64) void {
        const prev = self.inflight_bytes.fetchSub(n, .acq_rel);
        assert(prev >= n);
        const pb = self.inflight_blocks.fetchSub(1, .monotonic);
        assert(pb >= 1);
        stats.write_pipeline_inflight_bytes.sub(n);
        stats.write_pipeline_inflight_blocks.sub(1);
    }

    pub fn inflightBytes(self: *const AdmissionCore) u64 {
        return self.inflight_bytes.load(.monotonic);
    }

    pub fn inflightBlocks(self: *const AdmissionCore) u64 {
        return self.inflight_blocks.load(.monotonic);
    }
};

// ---------------------------------------------------------------------------
// §9.2 The per-lane BDP learner.
// ---------------------------------------------------------------------------

/// `prev − prev/8 + sample/8` (⅛ EWMA; the first sample seeds it).
pub fn ewmaStep(prev: u64, sample: u64) u64 {
    if (prev == 0) return sample;
    return prev - prev / 8 + sample / 8;
}

/// Fast up, slow down: the window's rate if higher, else the previous peak
/// decayed by ⅛.
pub fn rolledBwPeak(prev: u64, win_bytes: u64, elapsed_ms: u64) u64 {
    const rate = if (elapsed_ms == 0) 0 else win_bytes * 1000 / elapsed_ms;
    return @max(rate, prev - prev / 8);
}

/// Decay the floor UPWARD by ⅛, never past the EWMA (queueing inflation
/// must not feed the BDP: at saturation inflight ≡ bw × latency by Little's
/// law, so the floor is the service time, the EWMA the queued time).
pub fn rolledLatFloor(floor: u64, ewma: u64) u64 {
    return @max(@min(floor + floor / 8, ewma), 1);
}

/// `bw × lat / 1e9` bytes.
pub fn bdpBytes(bw_bps: u64, lat_ns: u64) u64 {
    const prod = @as(u128, bw_bps) * @as(u128, lat_ns) / std.time.ns_per_s;
    return @intCast(@min(prod, std.math.maxInt(u64)));
}

/// `max(FLOOR × bs, bdp × HEADROOM)`.
pub fn laneTargetBytes(bdp: u64, bs: u64) u64 {
    return @max(FLOOR_BLOCKS_PER_LANE * bs, bdp *| HEADROOM);
}

pub const Lane = struct {
    active: Atomic(bool) = .init(false),
    ewma_lat_ns: Atomic(u64) = .init(0),
    /// The running minimum within the window; rolled upward at the window.
    lat_floor_ns: Atomic(u64) = .init(0),
    bw_peak_bps: Atomic(u64) = .init(0),
    win_start_ms: Atomic(u64) = .init(0),
    win_bytes: Atomic(u64) = .init(0),

    /// One completion: EWMA, floor = running min, window bytes; the window
    /// rolls once per `WINDOW_MS` under a single-winner CAS on
    /// `win_start_ms` (the loser's bytes land in the new window).
    pub fn record(self: *Lane, bytes: u64, dur_ns: u64, now_ms: u64) void {
        self.active.store(true, .monotonic);
        var prev = self.ewma_lat_ns.load(.monotonic);
        while (true) {
            prev = self.ewma_lat_ns.cmpxchgWeak(prev, ewmaStep(prev, dur_ns), .monotonic, .monotonic) orelse break;
        }
        var floor = self.lat_floor_ns.load(.monotonic);
        while (floor == 0 or dur_ns < floor) {
            floor = self.lat_floor_ns.cmpxchgWeak(floor, @max(dur_ns, 1), .monotonic, .monotonic) orelse break;
        }
        _ = self.win_bytes.fetchAdd(bytes, .monotonic);
        const start = self.win_start_ms.load(.monotonic);
        if (start == 0) {
            _ = self.win_start_ms.cmpxchgStrong(0, @max(now_ms, 1), .monotonic, .monotonic);
            return;
        }
        if (now_ms -| start < WINDOW_MS) return;
        // Single winner rolls the window.
        if (self.win_start_ms.cmpxchgStrong(start, @max(now_ms, 1), .monotonic, .monotonic) != null) return;
        const wb = self.win_bytes.swap(0, .monotonic);
        const peak = rolledBwPeak(self.bw_peak_bps.load(.monotonic), wb, now_ms - start);
        self.bw_peak_bps.store(peak, .monotonic);
        self.lat_floor_ns.store(rolledLatFloor(self.lat_floor_ns.load(.monotonic), self.ewma_lat_ns.load(.monotonic)), .monotonic);
    }

    /// The raw BDP (0 while unlearned).
    pub fn rawBdp(self: *const Lane) u64 {
        return bdpBytes(self.bw_peak_bps.load(.monotonic), self.lat_floor_ns.load(.monotonic));
    }

    pub fn targetBytes(self: *const Lane, bs: u64) u64 {
        return laneTargetBytes(self.rawBdp(), bs);
    }
};

// ---------------------------------------------------------------------------
// §9.3 The BBR-flavoured probe.
// ---------------------------------------------------------------------------

pub const ProbeState = enum(u8) { hold, probing };

/// The roll's inputs and outputs as a plain value (the table in §9.3);
/// `Probe.roll` applies it under the single-winner CAS.
pub const RollState = struct {
    mul_q6: u64 = PROBE_MUL_ONE,
    prev_mul_q6: u64 = PROBE_MUL_ONE,
    state: ProbeState = .hold,
    baseline_bps: u64 = 0,
    adopted_bps: u64 = 0,
    cooldown: u32 = 0,
    ups: u64 = 0,
    backoffs: u64 = 0,
};

pub const Transition = enum { adopt, retreat, bleed, collapse_step_back, cooldown_tick, launch, none };

/// ONE epoch roll (pure).
pub fn rollDecision(s: *RollState, bps: u64, saturated: bool, headroom: bool) Transition {
    switch (s.state) {
        .probing => {
            if (saturated and bps >= s.baseline_bps + @max(s.baseline_bps / 16, 1)) {
                // ADOPT: discovery compounds — re-arm immediately.
                s.adopted_bps = bps;
                s.state = .hold;
                return .adopt;
            }
            // RETREAT on dead gain; a saturated dead gain cools down.
            s.mul_q6 = s.prev_mul_q6;
            s.state = .hold;
            if (saturated) s.cooldown = PROBE_COOLDOWN_EPOCHS;
            s.backoffs += 1;
            return .retreat;
        },
        .hold => {
            if (!saturated) {
                // The latency guard: bleed toward 1.0.
                s.mul_q6 = @max(s.mul_q6 - s.mul_q6 / 8, PROBE_MUL_ONE);
                return .bleed;
            }
            const collapsed = s.mul_q6 > PROBE_MUL_ONE and s.adopted_bps > 0 and bps < s.adopted_bps - s.adopted_bps / 8;
            if (collapsed) {
                // Senior to launching: step back ×0.8 and re-baseline.
                s.mul_q6 = @max(s.mul_q6 - s.mul_q6 / 5, PROBE_MUL_ONE);
                s.adopted_bps = bps;
                s.backoffs += 1;
                return .collapse_step_back;
            }
            if (s.cooldown > 0) {
                s.cooldown -= 1;
                return .cooldown_tick;
            }
            if (headroom and s.mul_q6 < PROBE_MUL_MAX and bps > 0) {
                s.prev_mul_q6 = s.mul_q6;
                s.baseline_bps = bps;
                s.mul_q6 = @min(s.mul_q6 + s.mul_q6 / 4, PROBE_MUL_MAX);
                s.state = .probing;
                s.ups += 1;
                return .launch;
            }
            return .none;
        },
    }
}

pub const Probe = struct {
    /// The published multiplier (every target computation reads it).
    mul_q6: Atomic(u64) = .init(PROBE_MUL_ONE),
    epoch_start_ms: Atomic(u64) = .init(0),
    epoch_bytes: Atomic(u64) = .init(0),
    /// The roller's private state (only the CAS winner touches it).
    s: RollState = .{},

    pub fn noteBytes(self: *Probe, n: u64) void {
        _ = self.epoch_bytes.fetchAdd(n, .monotonic);
    }

    /// Roll iff an epoch elapsed; single winner by CAS on `epoch_start_ms`
    /// (a check-then-store roll lets two rollers both launch — the model's
    /// weakening). Returns the transition for the winner, null otherwise.
    pub fn roll(self: *Probe, now_ms: u64, saturated: bool, headroom: bool) ?Transition {
        const start = self.epoch_start_ms.load(.monotonic);
        if (start == 0) {
            _ = self.epoch_start_ms.cmpxchgStrong(0, @max(now_ms, 1), .monotonic, .monotonic);
            return null;
        }
        if (now_ms -| start < PROBE_EPOCH_MS) return null;
        if (self.epoch_start_ms.cmpxchgStrong(start, @max(now_ms, 1), .acq_rel, .monotonic) != null) return null;
        const bytes = self.epoch_bytes.swap(0, .monotonic);
        const bps = bytes * 1000 / (now_ms - start);
        const t = rollDecision(&self.s, bps, saturated, headroom);
        self.mul_q6.store(self.s.mul_q6, .release);
        switch (t) {
            .launch => stats.write_pipeline_depth_probe_ups.inc(),
            .retreat, .collapse_step_back => stats.write_pipeline_depth_probe_backoffs.inc(),
            else => {},
        }
        return t;
    }

    pub fn mul(self: *const Probe) u64 {
        return self.mul_q6.load(.acquire);
    }
};

// ---------------------------------------------------------------------------
// §9.2/§9.4 The governor.
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS`: unset = adaptive; 0 = the
/// synchronous inline control arm; N = pinned.
pub const DepthOverride = union(enum) {
    adaptive,
    sync_inline,
    pinned: u64,
};

pub fn resolveDepthOverride(explicit: ?u64) DepthOverride {
    const n = explicit orelse return .adaptive;
    return if (n == 0) .sync_inline else .{ .pinned = n };
}

pub fn depthOverrideFromEnv(env: *const std.process.Environ.Map) DepthOverride {
    return resolveDepthOverride(knobs.optIntKnob("SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS", u64, env));
}

/// The S7 custody epoch the permit carries (the device layer's fence
/// classification; `nvme_dev.CustodyHooks`). Absent ⇒ 0.
pub const CustodyEpochSource = struct {
    ctx: ?*anyopaque,
    current: *const fn (?*anyopaque) u64,
};

pub const Permit = struct {
    bytes: u64,
    /// The custody epoch captured at ADMISSION (the permit is the carrier).
    auth_epoch: u64,
    admitted_ns: u64,
};

pub const Governor = struct {
    gpa: Allocator,
    block_size: u64,
    lanes: []Lane,
    core: AdmissionCore = .{},
    probe: Probe = .{},
    override: DepthOverride,
    /// The R5 authority: Red reads + the cap; null = no budget (tests).
    mem: ?*const budget.MemBudget,
    /// `capBytes(budget)` when `mem` is null.
    cap_fallback: u64,
    custody: ?CustodyEpochSource = null,
    completions: sync_notify.Notify = .init,
    /// `admission_waits` at the last roll (the saturation input).
    waits_at_roll: Atomic(u64) = .init(0),
    /// Test seam: fires inside an admitter's park window — registered on
    /// `completions`, its re-check not yet run (the window a completion
    /// must not be lost in).
    test_park_window: ?ParkWindow = null,

    pub const ParkWindow = struct {
        ctx: *anyopaque,
        at: *const fn (ctx: *anyopaque) void,
    };

    pub fn init(gpa: Allocator, block_size: u64, lanes: usize, override: DepthOverride, mem: ?*const budget.MemBudget, cap_fallback: u64) Allocator.Error!Governor {
        assert(lanes >= 1 and lanes <= router.MAX_VOLUMES);
        const ls = try gpa.alloc(Lane, lanes);
        @memset(ls, .{});
        return .{ .gpa = gpa, .block_size = block_size, .lanes = ls, .override = override, .mem = mem, .cap_fallback = cap_fallback };
    }

    pub fn deinit(self: *Governor) void {
        self.gpa.free(self.lanes);
        self.* = undefined;
    }

    /// The R5 component: in-flight DMA bytes are NON-sheddable (they drain
    /// by completion), weight 0, no shed.
    pub fn component(_: *Governor) budget.Component {
        return budget.Component.init("write_pipeline_inflight", 0, 0, .{ .gauge = &stats.write_pipeline_inflight_bytes }, null);
    }

    fn red(self: *const Governor) bool {
        const m = self.mem orelse return false;
        return m.level() == .red;
    }

    pub fn capBytesNow(self: *const Governor) u64 {
        const m = self.mem orelse return self.cap_fallback;
        return capBytes(m.budgetBytes());
    }

    /// Σ active lanes' targets (the floor when none has learned).
    fn baseTargetBytes(self: *const Governor) struct { base: u64, raw_bdp_sum: u64 } {
        var sum: u64 = 0;
        var raw: u64 = 0;
        var any = false;
        for (self.lanes) |*l| {
            if (!l.active.load(.monotonic)) continue;
            any = true;
            sum +|= l.targetBytes(self.block_size);
            raw +|= l.rawBdp();
        }
        if (!any) sum = FLOOR_BLOCKS_PER_LANE * self.block_size;
        return .{ .base = sum, .raw_bdp_sum = raw };
    }

    /// Pinned ⇒ `n × bs`; else `base × mul/64`, Red ⇒ `min(raw, max(Σ raw
    /// BDP, floor))`, capped at `budget/4`, never below one block.
    pub fn depthTargetBytes(self: *const Governor) u64 {
        const bs = self.block_size;
        const cap = self.capBytesNow();
        const t = switch (self.override) {
            .pinned => |n| n *| bs,
            .sync_inline => bs,
            .adaptive => blk: {
                const b = self.baseTargetBytes();
                stats.write_pipeline_depth_target_base.set(b.base);
                var raw = @as(u64, @intCast(@min(@as(u128, b.base) * self.probe.mul() / PROBE_MUL_ONE, std.math.maxInt(u64))));
                if (self.red()) raw = @min(raw, @max(b.raw_bdp_sum, FLOOR_BLOCKS_PER_LANE * bs));
                break :blk raw;
            },
        };
        const capped = @max(@min(t, cap), bs);
        stats.write_pipeline_depth_target.set(capped);
        return capped;
    }

    fn mintPermit(self: *Governor, n: u64) Permit {
        return .{ .bytes = n, .auth_epoch = if (self.custody) |c| c.current(c.ctx) else 0, .admitted_ns = clock.monotonicNs() };
    }

    /// One non-parking attempt loop (raced ⇒ re-evaluate): the sync-inline
    /// arm and the fast path of `admitSegment`.
    pub fn tryAdmit(self: *Governor, n: u64) ?Permit {
        while (true) {
            switch (self.core.tryAdmitOnce(n, self.depthTargetBytes())) {
                .admitted => return self.mintPermit(n),
                .full => return null,
                .raced => {},
            }
        }
    }

    /// `admit_segment`: fast attempt first; then register (`enable`) BEFORE
    /// the re-check (PERF-13: a completion between a Full re-check and the
    /// park's first poll was lost outright, pinning p99 at the tick);
    /// re-check; `admission_waits` once per park; park with the tick as the
    /// liveness backstop (`admission_tick_wakes`).
    pub fn admitSegment(self: *Governor, io: Io, n: u64) Permit {
        if (self.tryAdmit(n)) |p| return p;
        while (true) {
            var w = self.completions.notified();
            _ = w.enable(io);
            if (self.test_park_window) |h| h.at(h.ctx);
            if (self.tryAdmit(n)) |p| {
                w.cancel(io);
                return p;
            }
            stats.write_pipeline_admission_waits.inc();
            switch (w.waitUntilUncancelable(io, clock.monotonicNs() + ADMIT_TICK_MS * std.time.ns_per_ms)) {
                .notified => {},
                .deadline => stats.write_pipeline_admission_tick_wakes.inc(),
            }
        }
    }

    /// A permit returned UNSPENT (the arm it was admitted for declined
    /// before any DMA): the depth comes back and parked admitters wake, but
    /// no lane learns from it — a zero-length service would read as
    /// infinite bandwidth to the BDP learner.
    pub fn returnUnused(self: *Governor, io: Io, p: Permit) void {
        self.core.release(p.bytes);
        self.completions.notifyWaiters(io);
    }

    /// The completion at `now_ns` (`monotonicNs`, the permit's own clock —
    /// one read serves the BDP sample and the epochs): release the permit,
    /// feed the lane's BDP, feed the probe epoch (and roll it), wake parked
    /// admitters. Never waits.
    pub fn complete(self: *Governor, io: Io, p: Permit, lane_ord: u32, now_ns: u64) void {
        self.core.release(p.bytes);
        const dur_ns = now_ns -| p.admitted_ns;
        const now_ms = now_ns / std.time.ns_per_ms;
        if (lane_ord < self.lanes.len) self.lanes[lane_ord].record(p.bytes, dur_ns, now_ms);
        self.probe.noteBytes(p.bytes);
        self.rollProbe(now_ms);
        self.completions.notifyWaiters(io);
    }

    fn rollProbe(self: *Governor, now_ms: u64) void {
        const waits = stats.write_pipeline_admission_waits.load();
        const target = self.depthTargetBytes();
        const saturated = waits > self.waits_at_roll.load(.monotonic) or self.core.inflightBytes() >= target;
        const headroom = !self.red() and target < self.capBytesNow();
        if (self.probe.roll(now_ms, saturated, headroom) != null) self.waits_at_roll.store(waits, .monotonic);
    }

    /// Wait for the pipe to drain (teardown / fsync of a quiet mount);
    /// register-then-check like the admission park. True iff drained.
    pub fn quiesce(self: *Governor, io: Io, deadline_ns: u64) bool {
        while (self.core.inflightBytes() != 0) {
            var w = self.completions.notified();
            _ = w.enable(io);
            if (self.core.inflightBytes() == 0) {
                w.cancel(io);
                return true;
            }
            if (w.waitUntilUncancelable(io, @min(deadline_ns, clock.monotonicNs() + ADMIT_TICK_MS * std.time.ns_per_ms)) == .deadline and clock.monotonicNs() >= deadline_ns) return false;
        }
        return true;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const BS: u64 = 4 * derive.MiB;

test "depth_override_lever_resolves_sync_pinned_adaptive" {
    try testing.expectEqual(DepthOverride.adaptive, resolveDepthOverride(null));
    try testing.expectEqual(DepthOverride.sync_inline, resolveDepthOverride(0));
    try testing.expectEqual(DepthOverride{ .pinned = 7 }, resolveDepthOverride(7));
}

test "lane_target_is_floor_while_unlearned_and_bdp_headroom_once_measured; bw_peak_learns_fast_up_and_decays_slow_down; lat_floor_decays_upward_but_never_past_the_ewma" {
    var l: Lane = .{};
    try testing.expectEqual(FLOOR_BLOCKS_PER_LANE * BS, l.targetBytes(BS));
    // 10 completions of 4 MiB at 2 ms each over one window: bw = 40 MiB/250 ms = 160 MiB/s... measured from the window.
    var t: u64 = 1000;
    var i: u32 = 0;
    while (i < 10) : (i += 1) {
        l.record(BS, 2 * std.time.ns_per_ms, t);
        t += 25;
    }
    l.record(BS, 2 * std.time.ns_per_ms, 1000 + WINDOW_MS); // rolls the window
    const peak = l.bw_peak_bps.load(.monotonic);
    try testing.expect(peak > 0);
    try testing.expectEqual(rolledBwPeak(0, 11 * BS, WINDOW_MS), peak);
    const bdp = l.rawBdp();
    try testing.expectEqual(bdpBytes(peak, l.lat_floor_ns.load(.monotonic)), bdp);
    try testing.expectEqual(laneTargetBytes(bdp, BS), l.targetBytes(BS));
    // Fast up / slow down.
    try testing.expectEqual(@as(u64, 100), rolledBwPeak(50, 25, 250));
    try testing.expectEqual(@as(u64, 800 - 100), rolledBwPeak(800, 0, 250));
    // The floor decays up by ⅛ but never past the EWMA; never 0.
    try testing.expectEqual(@as(u64, 900), rolledLatFloor(800, 2000));
    try testing.expectEqual(@as(u64, 850), rolledLatFloor(800, 850));
    try testing.expectEqual(@as(u64, 1), rolledLatFloor(0, 0));
    try testing.expectEqual(@as(u64, 100), ewmaStep(0, 100));
    try testing.expectEqual(@as(u64, 100 - 12 + 25), ewmaStep(100, 200));
    try testing.expectEqual(FLOOR_BLOCKS_PER_LANE * 4096, laneTargetBytes(1, 4096)); // the floor wins over a tiny BDP
    try testing.expectEqual(@as(u64, 3 * 100_000), laneTargetBytes(100_000, 4096)); // 3 × BDP once it exceeds the floor
}

test "probe_core: probes up on responsive saturated epochs; backs off when gain dies and cools down; the latency guard bleeds unsaturated; the hold revalidation retreats on collapse; bounded and never probes without headroom" {
    var s: RollState = .{};
    // No headroom: never launches.
    try testing.expectEqual(Transition.none, rollDecision(&s, 1000, true, false));
    try testing.expectEqual(PROBE_MUL_ONE, s.mul_q6);
    // Launch, respond, adopt.
    try testing.expectEqual(Transition.launch, rollDecision(&s, 1000, true, true));
    try testing.expectEqual(PROBE_MUL_ONE + PROBE_MUL_ONE / 4, s.mul_q6);
    try testing.expectEqual(ProbeState.probing, s.state);
    try testing.expectEqual(Transition.adopt, rollDecision(&s, 1100, true, true));
    try testing.expectEqual(@as(u64, 1100), s.adopted_bps);
    try testing.expectEqual(ProbeState.hold, s.state);
    // Launch again; dead gain ⇒ retreat to prev + cooldown 8.
    const before = s.mul_q6;
    try testing.expectEqual(Transition.launch, rollDecision(&s, 1100, true, true));
    try testing.expectEqual(Transition.retreat, rollDecision(&s, 1100, true, true));
    try testing.expectEqual(before, s.mul_q6);
    try testing.expectEqual(PROBE_COOLDOWN_EPOCHS, s.cooldown);
    var k: u32 = 0;
    while (k < PROBE_COOLDOWN_EPOCHS) : (k += 1) try testing.expectEqual(Transition.cooldown_tick, rollDecision(&s, 1100, true, true));
    try testing.expectEqual(Transition.launch, rollDecision(&s, 1100, true, true));
    _ = rollDecision(&s, 1300, true, true); // adopt
    // Collapse: delivery falls > ⅛ below adopted ⇒ step back ×0.8 (senior to launching).
    const m = s.mul_q6;
    try testing.expectEqual(Transition.collapse_step_back, rollDecision(&s, 1000, true, true));
    try testing.expectEqual(@max(m - m / 5, PROBE_MUL_ONE), s.mul_q6);
    // Unsaturated: bleed toward 1.0.
    var bled = s.mul_q6;
    var n: u32 = 0;
    while (n < 64) : (n += 1) {
        try testing.expectEqual(Transition.bleed, rollDecision(&s, 0, false, true));
        try testing.expect(s.mul_q6 <= bled);
        bled = s.mul_q6;
    }
    try testing.expectEqual(PROBE_MUL_ONE, s.mul_q6);
    // Bounded: ramp to the max and stop launching.
    var r: RollState = .{};
    var loops: u32 = 0;
    var bps: u64 = 1000;
    while (loops < 200) : (loops += 1) {
        _ = rollDecision(&r, bps, true, true); // delivery keeps responding: every probe adopts
        if (bps < 1 << 50) bps *= 2;
    }
    try testing.expect(r.mul_q6 <= PROBE_MUL_MAX);
    try testing.expectEqual(PROBE_MUL_MAX, r.mul_q6);
    try testing.expectEqual(Transition.none, rollDecision(&r, bps, true, true)); // at the ceiling, saturated and responsive: no launch
}

test "the admission core: target holds, the empty pipe always admits ONE oversized segment, release settles both gauges to zero" {
    var c: AdmissionCore = .{};
    try testing.expectEqual(Attempt.admitted, c.tryAdmitOnce(100, 50)); // empty-pipe bypass
    try testing.expectEqual(Attempt.full, c.tryAdmitOnce(1, 50));
    c.release(100);
    try testing.expectEqual(@as(u64, 0), c.inflightBytes());
    try testing.expectEqual(@as(u64, 0), c.inflightBlocks());
    try testing.expectEqual(Attempt.admitted, c.tryAdmitOnce(30, 50));
    try testing.expectEqual(Attempt.admitted, c.tryAdmitOnce(20, 50));
    try testing.expectEqual(Attempt.full, c.tryAdmitOnce(1, 50));
    c.release(30);
    c.release(20);
}

test "depth_target_grows_with_measured_bandwidth_and_stays_budget_bounded; red_clamps_target_to_measured_drain_and_pinned_override_wins_verbatim; the floor is one block" {
    var g = try Governor.init(testing.allocator, BS, 2, .adaptive, null, capBytes(64 * derive.GiB));
    defer g.deinit();
    try testing.expectEqual(FLOOR_BLOCKS_PER_LANE * BS, g.depthTargetBytes());
    // Learn a fat lane: 100 × 4 MiB in 250 ms at 1 ms each ⇒ bw 1.6 GB/s, floor 1 ms ⇒ bdp 1.6 MB ⇒ target still the 8-block floor; grow the latency to see BDP win.
    var t: u64 = 10_000;
    var i: u32 = 0;
    while (i <= 100) : (i += 1) {
        g.lanes[0].record(BS, 40 * std.time.ns_per_ms, t);
        t += WINDOW_MS / 100;
    }
    g.lanes[0].record(BS, 40 * std.time.ns_per_ms, 10_000 + WINDOW_MS + 1);
    const learned = g.depthTargetBytes();
    try testing.expect(learned > FLOOR_BLOCKS_PER_LANE * BS);
    try testing.expect(learned <= g.capBytesNow());
    // A tiny budget cap bounds it; never below one block.
    g.cap_fallback = BS / 2;
    try testing.expectEqual(BS, g.depthTargetBytes());
    g.cap_fallback = capBytes(64 * derive.GiB);
    // Pinned wins verbatim.
    g.override = .{ .pinned = 3 };
    try testing.expectEqual(3 * BS, g.depthTargetBytes());
    g.override = .sync_inline;
    try testing.expectEqual(BS, g.depthTargetBytes());
    // Red: clamp to the measured drain (Σ raw BDP), never a fixed aggregate floor.
    g.override = .adaptive;
    g.probe.mul_q6.store(PROBE_MUL_MAX, .monotonic);
    const probed = g.depthTargetBytes();
    try testing.expect(probed > learned);
    var mb: budget.MemBudget = .{};
    mb.level_raw.store(@backingInt(budget.Level.red), .monotonic);
    mb.budget.store(64 * derive.GiB, .monotonic);
    g.mem = &mb;
    const red_t = g.depthTargetBytes();
    const raw = g.lanes[0].rawBdp();
    try testing.expectEqual(@max(@max(raw, FLOOR_BLOCKS_PER_LANE * BS), BS), red_t);
    try testing.expect(red_t < probed);
}

test "admission_parks_at_target_and_wakes_on_completion; empty_pipe_always_admits_and_quiesce_waits_for_drain; the wake is not lost to the park window" {
    const io = testing.io;
    var g = try Governor.init(testing.allocator, 4096, 1, .{ .pinned = 2 }, null, capBytes(1 * derive.GiB));
    defer g.deinit();
    const p1 = g.admitSegment(io, 4096);
    const p2 = g.admitSegment(io, 4096);
    try testing.expectEqual(@as(?Permit, null), g.tryAdmit(4096));
    // (a) A completion inside the park window — the admitter registered,
    // its re-check not yet run — is met by the re-check: admitted, no park
    // counted (a lost wake here would sleep to the backstop tick).
    const Window = struct {
        gov: *Governor,
        p: Permit,
        fired: bool = false,
        fn at(ctx: *anyopaque) void {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            if (w.fired) return;
            w.fired = true;
            w.gov.complete(testing.io, w.p, 0, w.p.admitted_ns + 1_000_000);
        }
    };
    var win: Window = .{ .gov = &g, .p = p1 };
    g.test_park_window = .{ .ctx = &win, .at = &Window.at };
    const waits_a = stats.write_pipeline_admission_waits.load();
    const p3 = g.admitSegment(io, 4096);
    g.test_park_window = null;
    try testing.expect(win.fired);
    try testing.expectEqual(waits_a, stats.write_pipeline_admission_waits.load());
    // (b) A completion once the admitter PARKED (its re-check failed and
    // the park is counted): the admission waits for it, and every park it
    // counts ends in that one completion's wake or a backstop tick — the
    // count is the schedule's (a completer slower than the 5 ms tick adds
    // a ticked re-park), the identity is not.
    const Completer = struct {
        fn run(io_: Io, gov: *Governor, p: Permit, waits0: u64, completing: *Atomic(bool)) void {
            while (stats.write_pipeline_admission_waits.load() == waits0) std.Thread.yield() catch {};
            // Ordered before the release the admission's CAS reads (acq_rel).
            completing.store(true, .release);
            gov.complete(io_, p, 0, 1_000_000);
        }
    };
    const waits_b = stats.write_pipeline_admission_waits.load();
    const ticks_b = stats.write_pipeline_admission_tick_wakes.load();
    var completing: Atomic(bool) = .init(false);
    var fut = io.concurrent(Completer.run, .{ io, &g, p2, waits_b, &completing }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency"),
    };
    const p4 = g.admitSegment(io, 4096);
    try testing.expect(completing.load(.acquire));
    fut.await(io);
    const waits = stats.write_pipeline_admission_waits.load() - waits_b;
    const ticks = stats.write_pipeline_admission_tick_wakes.load() - ticks_b;
    try testing.expect(waits >= 1 and ticks <= waits and waits <= 1 + ticks);
    try testing.expectEqual(@as(u64, 2), g.core.inflightBlocks());
    // quiesce waits for the drain (no wall-clock bound: it returns once
    // the drainer's completions land).
    const Drainer = struct {
        fn run(io_: Io, gov: *Governor, a: Permit, b: Permit) void {
            while (gov.completions.waiters(io_) == 0) std.Thread.yield() catch {};
            gov.complete(io_, a, 0, 1_000_000);
            gov.complete(io_, b, 0, 1_000_000);
        }
    };
    var fut2 = io.concurrent(Drainer.run, .{ io, &g, p3, p4 }) catch return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency");
    try testing.expect(g.quiesce(io, std.math.maxInt(u64)));
    fut2.await(io);
    try testing.expectEqual(@as(u64, 0), g.core.inflightBytes());
}

test "phase_family_is_always_on_with_exact_keys; recording_lands_in_exactly_the_named_phase; Σ legs ≡ total to the nanosecond on every exit" {
    const total_before = stats.write_pipeline_phase_ns[@backingInt(Phase.total)].snapshot();
    var p = Prof.startAt(1_000);
    p.markAt(.admit_wait, 1_500);
    p.markAt(.lock_wait, 1_700);
    p.markAt(.dma, 9_700);
    p.finishAt(10_000);
    const after = stats.write_pipeline_phase_ns[@backingInt(Phase.total)].snapshot();
    try testing.expectEqual(total_before.count + 1, after.count);
    try testing.expectEqual(total_before.sum_ns + 9_000, after.sum_ns);
    try testing.expectEqual(@as(u64, 500), p.legs[@backingInt(Phase.admit_wait)]);
    try testing.expectEqual(@as(u64, 200), p.legs[@backingInt(Phase.lock_wait)]);
    try testing.expectEqual(@as(u64, 8_000), p.legs[@backingInt(Phase.dma)]);
    try testing.expectEqual(@as(u64, 300), p.legs[@backingInt(Phase.other)]);
    var sum: u64 = 0;
    for (p.legs) |l| sum += l;
    try testing.expectEqual(@as(u64, 9_000), sum);
    // Every leg has the same count as total (0 ns recorded for the unmarked).
    for (stats.write_pipeline_phase_ns[0..Phase.legs]) |*h| try testing.expectEqual(after.count, h.snapshot().count);
    // A clock that reads backwards never underflows (the max with `last`).
    var q = Prof.startAt(100);
    q.markAt(.crypto, 50);
    q.finishAt(60);
    try testing.expectEqual(@as(u64, 0), q.legs[@backingInt(Phase.crypto)]);
    var buf: [8192]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"write_pipeline_phase_ns\":{\"admit_wait\":") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"total\":") != null);
}

test "disposition_mapping_is_the_task_counting_contract" {
    const pub0 = stats.write_pipeline_published.load();
    const stay0 = stats.write_pipeline_stay_parked.load();
    const disc0 = stats.write_pipeline_discards.load();
    const fence0 = counters.tripwireCount("write_pipeline_fence_drops");
    countDisposition(.published);
    countDisposition(.stay_parked);
    countDisposition(.discarded);
    try testing.expectEqual(pub0 + 1, stats.write_pipeline_published.load());
    try testing.expectEqual(stay0 + 1, stats.write_pipeline_stay_parked.load());
    try testing.expectEqual(disc0 + 1, stats.write_pipeline_discards.load());
    try testing.expectEqual(fence0, counters.tripwireCount("write_pipeline_fence_drops")); // none of the three fires it
}

test {
    _ = @import("write_pipeline_model.zig");
}
