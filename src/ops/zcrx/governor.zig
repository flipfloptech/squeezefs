//! The lane's park governor, the recv-end classification and the CQ sizing
//! (charter §6.3 "Structural starvation tears the session down";
//! `docs/porting/ops_zcrx.md` §6.2–§6.4). Pure state over an injected
//! clock; portable.
//!
//! Field round 5 "THE PARK NEVER WOKE": the release-hook wake set could
//! never fire with the pool structurally dry, the rq ring not full and
//! every span held by PARTIAL fills — deadlock by construction; so every
//! parked state POLLS bounded instead (`pollBounded`).
//!
//! Episodes, not retries: `onPark` opens an episode once (counting the
//! park and its errno class ONCE); `rearmDue` fires once per park (an
//! unarmed recv ALWAYS retries — a dry pool answers with one cheap `ENOMEM`
//! CQE; the retry IS the probe); only `onProgress` ends an episode (its
//! tail joins `starved_total`); `onIdleDrain` unlatches the probe gate
//! once WITHOUT ending the episode (ending it recounted a park per 200 µs
//! retry — `zcrx_recv_parks = 2,860,473`, round 6).
//!
//! The two structural arms: `structural()` = a SECOND consecutive failover
//! window (the first failover's releases ARE the recovery; a second proves
//! they did not refill the pool — finding J: 0.08 % engagement while 8
//! excluded queues cost the kernel path ~40 %); `economicStructural` = the
//! trickle-proof arm: `lifetime ≥ 2 × bound ∧ starved_total × 2 ≥ lifetime`
//! (the round-1 tape — parks 5, failovers 10, 0.6 GB of ~1500 GB — cycled
//! park → failover → trickle and never fired the first arm; ½ is the
//! majority boundary: rent paid for the whole life, savings only while
//! serving).
//!
//! The failover bound derives from the measured fabric RTT class (decision
//! 0002 row 34 bans the per-op read timeout the reference derived it from):
//! `clamp(4000 × rtt, 500 ms, 2 s)` — ≥ ~4,000 × the 235 µs RTT class the
//! reference's own reasoning named, and far below any volume watchdog.

const std = @import("std");
const derive = @import("../../core/derive.zig");
const abi = @import("abi.zig");

/// The first failover is the transient allowance; the second proves it failed.
pub const REFILL_STRUCTURAL_FAILOVERS: u32 = 2;
/// Poll cadence while parked with pending work: vs the 235 µs fabric-RTT
/// class — one EXT_ARG enter, not a sleep (QUIRK 8 taken).
pub const PARK_POLL_NS: u64 = 200 * std.time.ns_per_us;
pub const RTT_FAILOVER_MULTIPLE: u64 = 4000;
pub const FAILOVER_BOUND_MIN_NS: u64 = 500 * std.time.ns_per_ms;
pub const FAILOVER_BOUND_MAX_NS: u64 = 2 * std.time.ns_per_s;

/// `clamp(4000 × rtt, 500 ms, 2 s)`.
pub fn parkFailBound(rtt_ns: u64) u64 {
    return derive.clamp(std.math.mul(u64, rtt_ns, RTT_FAILOVER_MULTIPLE) catch std.math.maxInt(u64), FAILOVER_BOUND_MIN_NS, FAILOVER_BOUND_MAX_NS);
}

pub const ParkClass = enum(u8) {
    /// `-ENOMEM`: the provider pool has no free net_iov.
    pool_dry,
    /// `-ENOBUFS`: the refill ring starved.
    rq_empty,
    /// `-ENOSPC`: the lane's own CQ was full.
    cq_full,

    pub fn name(self: ParkClass) []const u8 {
        return switch (self) {
            .pool_dry => "pool_dry (ENOMEM: the provider pool has no free net_iov)",
            .rq_empty => "rq_empty (ENOBUFS: the refill ring starved)",
            .cq_full => "cq_full (ENOSPC: the lane's own CQ was full)",
        };
    }
};

pub const RecvEnd = union(enum) {
    /// `res == 0`.
    eof,
    /// Flow control, never device death.
    park: ParkClass,
    /// `res > 0` without `F_MORE`: re-arm at once.
    rearm,
    /// Anything else: poison.
    terminal: i32,
};

/// A recv CQE without `IORING_CQE_F_MORE`.
pub fn classifyRecvEnd(res: i32) RecvEnd {
    if (res == 0) return .eof;
    if (res > 0) return .rearm;
    const e: std.os.linux.E = @fromBackingInt(@intCast(@as(u16, @intCast(-res))));
    return switch (e) {
        .NOMEM => .{ .park = .pool_dry },
        .NOBUFS => .{ .park = .rq_empty },
        .NOSPC => .{ .park = .cq_full },
        else => .{ .terminal = res },
    };
}

pub const Governor = struct {
    born_ns: u64,
    armed: bool = false,
    episode_active: bool = false,
    episode_start_ns: u64 = 0,
    window_start_ns: u64 = 0,
    /// Latched for the funnel: new reads bypass this queue while recovering.
    starved: bool = false,
    failover_streak: u32 = 0,
    starved_total_ns: u64 = 0,
    parks: u64 = 0,
    failovers: u64 = 0,
    rearm_pending: bool = false,

    pub fn init(now_ns: u64) Governor {
        return .{ .born_ns = now_ns };
    }

    pub fn onArmed(self: *Governor) void {
        self.armed = true;
        self.rearm_pending = false;
    }

    /// Unarm; true iff a NEW episode started (count the park + its class once).
    pub fn onPark(self: *Governor, now_ns: u64) bool {
        self.armed = false;
        self.rearm_pending = true;
        if (self.episode_active) return false;
        self.episode_active = true;
        self.episode_start_ns = now_ns;
        self.window_start_ns = now_ns;
        self.starved = true;
        self.parks += 1;
        return true;
    }

    /// The ONLY verb that ends an episode; returns the tail (ns since the
    /// window start) that joins `starved_total`.
    pub fn onProgress(self: *Governor, now_ns: u64) ?u64 {
        if (!self.episode_active) return null;
        const tail = now_ns -| self.window_start_ns;
        self.starved_total_ns += tail;
        self.episode_active = false;
        self.starved = false;
        self.failover_streak = 0;
        return tail;
    }

    /// Once per park, unconditional.
    pub fn rearmDue(self: *Governor) bool {
        if (!self.rearm_pending or self.armed) return false;
        self.rearm_pending = false;
        return true;
    }

    /// The queue drained while starved: one read may re-enter, the episode
    /// stays open.
    pub fn onIdleDrain(self: *Governor) void {
        if (self.episode_active) self.starved = false;
    }

    /// Poll at `PARK_POLL_NS` iff an episode is open AND work is pending;
    /// idle-starved blocks in the enter.
    pub fn pollBounded(self: *const Governor, pending_work: bool) bool {
        return self.episode_active and pending_work;
    }

    /// At most once per `bound` since the window start; restarts the window,
    /// accounts the window into `starved_total`; returns the window length.
    pub fn failoverDue(self: *Governor, now_ns: u64, bound_ns: u64) ?u64 {
        if (!self.episode_active) return null;
        const elapsed = now_ns -| self.window_start_ns;
        if (elapsed < bound_ns) return null;
        self.window_start_ns = now_ns;
        self.failover_streak += 1;
        self.failovers += 1;
        self.starved_total_ns += elapsed;
        self.starved = true;
        return elapsed;
    }

    /// Arm 1.
    pub fn structural(self: *const Governor) bool {
        return self.failover_streak >= REFILL_STRUCTURAL_FAILOVERS;
    }

    /// Arm 2: the trickle-proof majority law.
    pub fn economicStructural(self: *const Governor, now_ns: u64, bound_ns: u64) bool {
        const lifetime = now_ns -| self.born_ns;
        return lifetime >= 2 * bound_ns and self.starved_total_ns * 2 >= lifetime;
    }
};

// ---------------------------------------------------------------------------
// CQ sizing (§6.2): zcrx NEVER takes the CQ overflow path — a full CQ
// terminates the recv `-ENOSPC`; userspace sizes adequately and reaps.
// ---------------------------------------------------------------------------

/// `sq = next_pow2(depth + 8)`: commands + recv/doorbell arms + partial resubmits.
pub fn sqEntriesFor(depth: u32) u16 {
    return @intCast(@min(derive.nextPow2(@as(u64, depth) + 8), 32768));
}

/// `flat = ceil(max_xfer / chunk)`; known MTU ⇒ `per_cmd = max(flat,
/// ceil(max_xfer / mtu) × burst / chunk)` else `2 × flat`; `cq =
/// next_pow2(depth × per_cmd + sq)` clamped to the kernel max. Field: `64 ×
/// 351 + 128 = 22,592 → 32,768`.
pub fn cqEntriesFor(depth: u32, max_xfer: u64, chunk: u64, sq: u32, mtu: ?u32) u32 {
    const flat = derive.ceilDiv(max_xfer, chunk);
    const per_cmd: u64 = if (mtu) |m| blk: {
        const burst = derive.ceilDiv(m, chunk) * chunk;
        break :blk @max(flat, derive.ceilDiv(max_xfer, m) * (burst / chunk));
    } else 2 * flat;
    const want = derive.nextPow2(@as(u64, depth) * per_cmd + sq);
    return @intCast(@min(want, abi.KERNEL_MAX_CQ_ENTRIES));
}

/// When the CQ hit the kernel max, admission shrinks to what the CQ can
/// complete: `(cq − sq) × chunk × payload / burst` (unknown MTU ⇒ `/ 2`).
pub fn cqAdmittedWindowBytes(cq: u32, sq: u32, chunk: u64, mtu: ?u32) u64 {
    const spans = @as(u64, cq) - sq;
    if (mtu) |m| {
        const burst = derive.ceilDiv(m, chunk) * chunk;
        return spans * chunk * m / burst;
    }
    return spans * chunk / 2;
}

// ---------------------------------------------------------------------------
// Tests (the reference's governor tests ported; venue: any).
// ---------------------------------------------------------------------------

const testing = std.testing;
const ms = std.time.ns_per_ms;

test "cold start: the first park opens an episode and counts once; the rearm fires once per park; retries never recount" {
    var g = Governor.init(0);
    g.onArmed();
    try testing.expect(g.onPark(10));
    try testing.expectEqual(@as(u64, 1), g.parks);
    try testing.expect(g.starved);
    try testing.expect(g.rearmDue());
    try testing.expect(!g.rearmDue());
    g.onArmed();
    try testing.expect(!g.onPark(20));
    try testing.expectEqual(@as(u64, 1), g.parks);
    try testing.expect(g.rearmDue());
}

test "poll-bounded only while an episode is open AND work is pending; idle-drain unlatches the gate without ending the episode" {
    var g = Governor.init(0);
    try testing.expect(!g.pollBounded(true));
    _ = g.onPark(0);
    try testing.expect(g.pollBounded(true));
    try testing.expect(!g.pollBounded(false));
    g.onIdleDrain();
    try testing.expect(!g.starved);
    try testing.expect(g.episode_active);
    try testing.expectEqual(@as(u64, 1), g.parks);
    // A park after the idle-drain is still the same episode.
    try testing.expect(!g.onPark(5));
}

test "progress is the only verb that ends an episode: the tail joins starved_total and the streak resets" {
    var g = Governor.init(0);
    _ = g.onPark(100 * ms);
    try testing.expectEqual(@as(?u64, 50 * ms), g.onProgress(150 * ms));
    try testing.expect(!g.episode_active and !g.starved);
    try testing.expectEqual(50 * ms, g.starved_total_ns);
    try testing.expect(g.onProgress(200 * ms) == null);
}

test "failover once per window: the window restarts, the streak grows, the window is accounted; the second consecutive failover is structural" {
    var g = Governor.init(0);
    const bound = parkFailBound(235 * std.time.ns_per_us);
    try testing.expectEqual(940 * ms, bound);
    _ = g.onPark(0);
    try testing.expect(g.failoverDue(bound - 1, bound) == null);
    try testing.expectEqual(@as(?u64, bound), g.failoverDue(bound, bound));
    try testing.expect(!g.structural());
    try testing.expect(g.failoverDue(bound + 1, bound) == null);
    try testing.expectEqual(@as(?u64, bound), g.failoverDue(2 * bound, bound));
    try testing.expect(g.structural());
    try testing.expectEqual(2 * bound, g.starved_total_ns);
    try testing.expectEqual(@as(u64, 2), g.failovers);
    // Progress between failovers resets the streak: the first arm never fires on a trickle.
    var h = Governor.init(0);
    _ = h.onPark(0);
    _ = h.failoverDue(bound, bound);
    _ = h.onProgress(bound + 1);
    _ = h.onPark(bound + 2);
    _ = h.failoverDue(2 * bound + 2, bound);
    try testing.expect(!h.structural());
}

test "the economic arm: a trickle cannot reset it (the round-1 tape fires at the majority boundary); a majority-serving lane never fires" {
    const bound = 1000 * ms;
    var g = Governor.init(0);
    // Park → failover → trickle progress, five times: starved 5 × bound of a
    // 5 × bound + 5 ms life.
    var t: u64 = 0;
    for (0..5) |_| {
        _ = g.onPark(t);
        t += bound;
        _ = g.failoverDue(t, bound);
        t += 1 * ms;
        _ = g.onProgress(t);
    }
    try testing.expect(!g.structural());
    try testing.expect(g.economicStructural(t, bound));
    // Serving 3 × bound between 1 × bound starvations: never.
    var h = Governor.init(0);
    t = 0;
    for (0..5) |_| {
        t += 3 * bound;
        _ = h.onPark(t);
        t += bound;
        _ = h.failoverDue(t, bound);
        _ = h.onProgress(t);
    }
    try testing.expect(!h.economicStructural(t, bound));
    // Too young to judge.
    var y = Governor.init(0);
    _ = y.onPark(0);
    _ = y.failoverDue(bound, bound);
    try testing.expect(!y.economicStructural(bound, bound));
}

test "the failover bound derives from the RTT class: clamp(4000 × rtt, 500 ms, 2 s)" {
    try testing.expectEqual(500 * ms, parkFailBound(10 * std.time.ns_per_us));
    try testing.expectEqual(940 * ms, parkFailBound(235 * std.time.ns_per_us));
    try testing.expectEqual(2000 * ms, parkFailBound(10 * ms));
    try testing.expectEqual(2000 * ms, parkFailBound(std.math.maxInt(u64)));
}

test "recv-end classification: 0 eof; ENOMEM/ENOBUFS/ENOSPC park by class (never poison); > 0 rearm; anything else terminal" {
    try testing.expectEqual(RecvEnd.eof, classifyRecvEnd(0));
    try testing.expectEqual(RecvEnd{ .park = .pool_dry }, classifyRecvEnd(-@as(i32, @backingInt(std.os.linux.E.NOMEM))));
    try testing.expectEqual(RecvEnd{ .park = .rq_empty }, classifyRecvEnd(-@as(i32, @backingInt(std.os.linux.E.NOBUFS))));
    try testing.expectEqual(RecvEnd{ .park = .cq_full }, classifyRecvEnd(-@as(i32, @backingInt(std.os.linux.E.NOSPC))));
    try testing.expectEqual(RecvEnd.rearm, classifyRecvEnd(4096));
    try testing.expectEqual(RecvEnd{ .terminal = -104 }, classifyRecvEnd(-104));
    try testing.expect(std.mem.indexOf(u8, ParkClass.pool_dry.name(), "ENOMEM") != null);
}

test "CQ sizing derives from the chunk-grain completion demand: the field shape 64 × 351 + 128 → 32768; the kernel max clamps and shrinks admission" {
    try testing.expectEqual(@as(u16, 128), sqEntriesFor(64));
    try testing.expectEqual(@as(u16, 16), sqEntriesFor(4));
    // 1 MiB / 9000 = 117 PDUs × 3 chunks/burst = 351 per command.
    try testing.expectEqual(@as(u32, 32768), cqEntriesFor(64, 1 << 20, 4096, 128, 9000));
    // Unknown MTU: 2 × 256 = 512 per command; 64 × 512 + 128 → 65536 (the max).
    try testing.expectEqual(@as(u32, 65536), cqEntriesFor(64, 1 << 20, 4096, 128, null));
    // Past the max: clamped.
    try testing.expectEqual(abi.KERNEL_MAX_CQ_ENTRIES, cqEntriesFor(64, 1 << 20, 4096, 128, 1500));
    // The admitted window the clamped CQ can complete.
    try testing.expectEqual(@as(u64, (65536 - 128) * 4096 * 9000 / 12288), cqAdmittedWindowBytes(65536, 128, 4096, 9000));
    try testing.expectEqual(@as(u64, (65536 - 128) * 2048), cqAdmittedWindowBytes(65536, 128, 4096, null));
    // The sim leg: a 64 KiB chunk, no MTU: 2 × 16 = 32 per command.
    try testing.expectEqual(@as(u32, 1024), cqEntriesFor(16, 1 << 20, 64 << 10, 32, null));
}
