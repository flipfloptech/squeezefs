//! The round-trip predictor (RFC 6298, Jacobson/Karels): `srtt += (R −
//! srtt)/8`, `rttvar += (|srtt − R| − rttvar)/4`, `rto = srtt + 4·rttvar`.
//! One estimator for every "when will this land" question in the daemon —
//! the conveyor's window hold (decision 0003 §L), the committer's ack spin
//! (§AB.4), the transport worker's owed spin (rung-10-workers-per-queue):
//! the deadline is where a round trip stops being a round trip and starts
//! being a stall (TCP's own ruling on the same question; K = 4 bounds the
//! false-timeout rate). No clamp below: a sub-µs device holds sub-µs.

const std = @import("std");

/// RFC 6298 §2.3's gains — `srtt += (R − srtt)/8`, `rttvar += (|srtt − R| −
/// rttvar)/4` — and §2.2's `K = 4` in `rto = srtt + 4·rttvar` (what bounds
/// the false-timeout rate).
pub const ALPHA_SHIFT: u6 = 3;
pub const BETA_SHIFT: u6 = 2;
pub const K: u64 = 4;

/// Fed and read on one thread in the common case; the two words are
/// atomics so a reader on another thread (the RAM contracts' device
/// thread, the stats inode) reads a bound, never a torn correctness term.
pub const RttEstimator = struct {
    srtt_ns: std.atomic.Value(u64) = .init(0),
    rttvar_ns: std.atomic.Value(u64) = .init(0),
    samples: std.atomic.Value(u64) = .init(0),

    pub fn sample(self: *RttEstimator, r: u64) void {
        const n = self.samples.fetchAdd(1, .monotonic);
        if (n == 0) {
            // RFC 6298 §2.2: the first measurement seeds `srtt = R`,
            // `rttvar = R/2`.
            self.srtt_ns.store(r, .monotonic);
            self.rttvar_ns.store(r / 2, .monotonic);
            return;
        }
        const srtt = self.srtt_ns.load(.monotonic);
        const rttvar = self.rttvar_ns.load(.monotonic);
        const dev = if (srtt > r) srtt - r else r - srtt;
        // §2.3: `rttvar` BEFORE `srtt` (the deviation is against the old mean).
        const new_var = rttvar - (rttvar >> BETA_SHIFT) + (dev >> BETA_SHIFT);
        const new_srtt = srtt - (srtt >> ALPHA_SHIFT) + (r >> ALPHA_SHIFT);
        self.rttvar_ns.store(new_var, .monotonic);
        self.srtt_ns.store(new_srtt, .monotonic);
    }

    /// `srtt + 4·rttvar`, capped at `ceiling_ns`; 0 before the first
    /// sample (a hold with no predictor is no hold).
    pub fn bound(self: *const RttEstimator, ceiling_ns: u64) u64 {
        if (self.samples.load(.monotonic) == 0) return 0;
        const rto = self.srtt_ns.load(.monotonic) +| K *| self.rttvar_ns.load(.monotonic);
        return @min(rto, ceiling_ns);
    }

    /// The smoothed mean alone (the inter-arrival predictor's reading);
    /// null before the first sample.
    pub fn mean(self: *const RttEstimator) ?u64 {
        if (self.samples.load(.monotonic) == 0) return null;
        return self.srtt_ns.load(.monotonic);
    }
};

const testing = std.testing;

test "the RFC 6298 round-trip predictor — first sample seeds srtt = R, rttvar = R/2; the gains 1/8 and 1/4; bound = srtt + 4·rttvar capped at the ceiling; 0 before any sample" {
    var e: RttEstimator = .{};
    try testing.expectEqual(@as(u64, 0), e.bound(std.math.maxInt(u64)));
    try testing.expect(e.mean() == null);
    e.sample(200_000);
    try testing.expectEqual(@as(u64, 200_000), e.srtt_ns.load(.monotonic));
    try testing.expectEqual(@as(u64, 100_000), e.rttvar_ns.load(.monotonic));
    try testing.expectEqual(@as(u64, 600_000), e.bound(std.math.maxInt(u64)));
    try testing.expectEqual(@as(u64, 50_000), e.bound(50_000));
    // A second identical sample: rttvar decays by a quarter of itself,
    // srtt stays.
    e.sample(200_000);
    try testing.expectEqual(@as(u64, 200_000), e.srtt_ns.load(.monotonic));
    try testing.expectEqual(@as(u64, 75_000), e.rttvar_ns.load(.monotonic));
    // A 1 ms outlier moves srtt by an eighth of the difference and rttvar
    // by a quarter of the deviation (against the OLD srtt).
    e.sample(1_000_000);
    try testing.expectEqual(@as(u64, 300_000), e.srtt_ns.load(.monotonic));
    try testing.expectEqual(@as(u64, 75_000 - 18_750 + 200_000), e.rttvar_ns.load(.monotonic));
    // Convergence: a constant stream drives rttvar toward 0 and the bound
    // toward the round trip itself.
    for (0..64) |_| e.sample(200_000);
    try testing.expect(e.srtt_ns.load(.monotonic) >= 200_000 and e.srtt_ns.load(.monotonic) < 202_000);
    try testing.expect(e.bound(std.math.maxInt(u64)) < 210_000);
    try testing.expectEqual(e.srtt_ns.load(.monotonic), e.mean().?);
}
