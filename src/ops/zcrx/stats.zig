//! The `zcrx_*` instruments (charter §6.3, §10; `docs/porting/ops_zcrx.md`
//! §9.2): always exported, zero when the lane is inert. Portable.
//!
//! Row-validity laws an engaged row must satisfy: `zcrx_lane_poisoned = 0`,
//! `zcrx_frame_violations = 0`, `zcrx_fill_fallbacks ≈ 0`,
//! `zcrx_degraded_bypasses ≈ 0`, parks/failovers bounded; and the
//! engagement law `zcrx_gather_bytes ≡ zcrx_fill_bytes` byte-exact,
//! unconditionally (both counted at the WHOLE-read success boundary).

const std = @import("std");
const counters = @import("../../core/counters.zig");

/// 1 while a session is armed on some device; dropped by poison/teardown.
pub var zcrx_lane_armed: counters.Gauge = .{};
/// The probe ladder's verdict code on this host (`probe.Verdict.Tag`):
/// 0 = not probed / Present, else WHY the lane is Absent (loud once).
pub var zcrx_lane_absent_reason: counters.Gauge = .{};

/// THE funnel's three mutually exclusive outcomes per cold fill: the lane
/// served it, the kernel path served it (lane absent or the range
/// ineligible), or the lane was present and REFUSED it (degraded bypass,
/// admission decline or a per-op fallback — counted below by reason).
pub var zcrx_fills: counters.ShardedCounter = .{};
pub var zcrx_kernel_fills: counters.ShardedCounter = .{};
pub var zcrx_lane_refused: counters.ShardedCounter = .{};

/// Lane-served bytes at the WHOLE-read success boundary (the fill provenance).
pub var zcrx_fill_bytes: counters.ShardedCounter = .{};
/// The priced completion pass: `gather ≡ fill` byte-exact.
pub var zcrx_gather_bytes: counters.ShardedCounter = .{};
/// The fused SUBSET of gather landing in the caller's registered destination.
pub var zcrx_dest_gather_bytes: counters.ShardedCounter = .{};
/// Per-op lane → kernel retries (≈ 0 on a healthy row).
pub var zcrx_fill_fallbacks: counters.ShardedCounter = .{};
/// Framing violations (each poisoned a session): must-stay-0.
pub var zcrx_frame_violations: counters.Tripwire = .init("zcrx_frame_violations");
/// Session poison transitions (REAL transport poison only): must-stay-0.
/// Teardown and failovers are NOT poison.
pub var zcrx_lane_poisoned: counters.Tripwire = .init("zcrx_lane_poisoned");
/// Arm failures (the association refused at some step).
pub var zcrx_conn_errors: counters.ShardedCounter = .{};
/// The header edge copy (≪ 1 % of fill bytes by construction).
pub var zcrx_hdr_copy_bytes: counters.ShardedCounter = .{};
/// Live area bytes = the R5 `zcrx_area` component source.
pub var zcrx_area_bytes: counters.Gauge = .{};
/// Whole-read admission DECLINES (one per declined read): the sizing instrument.
pub var zcrx_area_admission_waits: counters.ShardedCounter = .{};
/// Funnel reads bypassing a starved lane.
pub var zcrx_degraded_bypasses: counters.ShardedCounter = .{};
/// Starvation EPISODES; `parks ≡ pool_dry + rq_empty + cq_full`.
pub var zcrx_recv_parks: counters.ShardedCounter = .{};
pub var zcrx_parks_pool_dry: counters.ShardedCounter = .{};
pub var zcrx_parks_rq_empty: counters.ShardedCounter = .{};
pub var zcrx_parks_cq_full: counters.ShardedCounter = .{};
/// Windows past the failover bound.
pub var zcrx_recv_failovers: counters.ShardedCounter = .{};
/// Cumulative starved time (windows + tails, exact).
pub var zcrx_starved_ms: counters.ShardedCounter = .{};
/// Structural escalations latched (either arm): "did no-harm fire?".
pub var zcrx_structural_teardowns: counters.ShardedCounter = .{};
/// Probe refusals announced (one per device per mount).
pub var zcrx_probe_refusals: counters.ShardedCounter = .{};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try counters.writeNamespaceJson(@This(), w);
}

test "the family names every zcrx_* instrument; the three funnel outcomes and the engagement pair are present" {
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    inline for (.{ "zcrx_lane_armed", "zcrx_lane_absent_reason", "zcrx_fills", "zcrx_kernel_fills", "zcrx_lane_refused", "zcrx_fill_bytes", "zcrx_gather_bytes", "zcrx_dest_gather_bytes", "zcrx_frame_violations", "zcrx_lane_poisoned", "zcrx_area_bytes", "zcrx_recv_parks", "zcrx_parks_pool_dry", "zcrx_structural_teardowns" }) |k| {
        try std.testing.expect(std.mem.indexOf(u8, w.buffered(), "\"" ++ k ++ "\":") != null);
    }
}
