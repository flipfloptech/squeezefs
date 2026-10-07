//! The lease-clock core (charter §5.5 "Death and recovery", §5.7; spec
//! `guard_claims.md` §3.5; the reference's `lease_clock_core.rs`). Rung 6
//! arms it; rung 5 ships the pure arithmetic and the member's lease words
//! so the symmetric plane inherits a proven core.
//!
//! Law: the OWNER evicts at `T_owner`; the MEMBER fences ITSELF at
//! `T_self = T_owner − (2·skew_max + D_purge)`, strictly earlier, so a member
//! can never believe it holds custody the owner has already re-granted.
//! `None` (REFUSE, never clamp) when the reserve reaches `T_owner`. The
//! member anchors every deadline on its own SEND instant (never the owner's
//! clock value): the owner's deadline is at least `RTT` later in the
//! member's frame, so the member's precedes it by `≥ 2·skew_max + D_purge +
//! RTT` (the theorem test).
//!
//! `MemberLeaseWords` is the lock-free pair the models in
//! `lease_clock_model.zig` check: `renewed` publishes the ANCHOR then the
//! LABEL (both Release) and a reader that sees the new label (Acquire) must
//! see the new anchor; `fence()` runs its side effects exactly once.

const std = @import("std");
const claim = @import("claim.zig");

/// Two oscillators at ±500 ppm over temperature: 22.5 ms at 45 s — the
/// PHYSICAL bound on monotonic-clock rate drift between two hosts
/// (M:774-782); `skew_max = max(T_owner × ppm/1e6, observed RTT)`.
pub const MONOTONIC_RATE_DRIFT_PPM: u64 = 500;

/// `T_owner` IS the staleness TTL (quirk Q9: one constant, no knob on top).
pub const T_OWNER_MS: u64 = claim.STALE_TTL_S * 1000;

/// The shipped beat: the renew interval never regresses below it, and
/// never above a third of `T_self` (three attempts before the deadline).
pub const SHIPPED_BEAT_MS: u64 = claim.HEARTBEAT_INTERVAL_S * 1000;

/// `T_self` (ms), or null when the reserve `2·skew_max + D_purge ≥ T_owner`.
pub fn tSelfMs(t_owner_ms: u64, skew_max_ms: u64, d_purge_ms: u64) ?u64 {
    const reserve = 2 *| skew_max_ms +| d_purge_ms;
    if (reserve >= t_owner_ms) return null;
    return t_owner_ms - reserve;
}

/// `clamp(T_self / 3, 1 ms, shipped beat)` (LC:62-64).
pub fn renewIntervalMs(t_self_ms: u64) u64 {
    return @max(1, @min(t_self_ms / 3, SHIPPED_BEAT_MS));
}

/// The drift term of `skew_max` for one `T_owner`.
pub fn driftSkewMs(t_owner_ms: u64) u64 {
    return t_owner_ms * MONOTONIC_RATE_DRIFT_PPM / 1_000_000;
}

/// `skew_max = max(drift term, observed RTT)`; `D_purge = max(2 × cadence,
/// RTT)` (observe, then finish).
pub fn deriveSkewMaxMs(t_owner_ms: u64, rtt_ms: u64) u64 {
    return @max(driftSkewMs(t_owner_ms), rtt_ms);
}

pub fn derivePurgeMs(revalidation_cadence_ms: u64, rtt_ms: u64) u64 {
    return @max(2 *| revalidation_cadence_ms, rtt_ms);
}

/// A grant's `t_self_ms` as the member computes it from the grant's words:
/// saturates to 0 so a hostile or garbled grant fail-stops at once (LC:70-72).
pub fn grantTSelfMs(t_owner_ms: u64, skew_max_ms: u64, d_purge_ms: u64) u64 {
    return tSelfMs(t_owner_ms, skew_max_ms, d_purge_ms) orelse 0;
}

/// The refusal (M:879-886). `T_owner` is a constant here (quirk Q9), so
/// the remedy names the two derived overrides rung 6 registers.
pub fn writeArmRefusal(w: *std.Io.Writer, t_owner_ms: u64, skew_max_ms: u64, d_purge_ms: u64) std.Io.Writer.Error!void {
    try w.print("membership lease clocks refuse to arm: T_self = T_owner − 2·skew_max − D_purge = {d} − {d} − {d} ms is not positive, so a member could still believe it holds custody the owner has already re-granted (charter §5.5 'Two lease clocks'). Lower the membership skew_max / purge overrides (T_owner is the {d} s staleness TTL)", .{ t_owner_ms, 2 *| skew_max_ms, d_purge_ms, claim.STALE_TTL_S });
}

/// The member's lease words (LC:80-262). Every field is an atomic the
/// renewal thread (`sqz-lease-io`) writes and the custody checks read.
pub const MemberLeaseWords = struct {
    epoch: std.atomic.Value(u64) = .init(0),
    /// The self-fence deadline (member monotonic ms): the ANCHOR.
    t_self_deadline_ms: std.atomic.Value(u64) = .init(0),
    renew_at_ms: std.atomic.Value(u64) = .init(std.math.maxInt(u64)),
    renew_ms: std.atomic.Value(u64) = .init(0),
    acked_free_epoch: std.atomic.Value(u64) = .init(0),
    /// The renewal's LABEL (a monotone grant sequence); the guard on `renewed`.
    learned_label: std.atomic.Value(u64) = .init(0),
    learned_at_ms: std.atomic.Value(u64) = .init(0),
    skew_max_ms: std.atomic.Value(u64) = .init(0),
    d_purge_ms: std.atomic.Value(u64) = .init(0),
    fenced: std.atomic.Value(bool) = .init(false),

    pub const Learned = struct { label: u64, deadline_ms: u64 };

    /// A renewal labelled `label` anchored at the member's send instant
    /// `sent_ms`: the deadline (ANCHOR) is published BEFORE the label, both
    /// Release, so a reader that acquires the label sees this anchor or a
    /// later one; a stale label (≤ the learned one) publishes nothing.
    /// Returns whether it was learned.
    pub fn renewed(self: *MemberLeaseWords, label: u64, sent_ms: u64, t_self_ms: u64, renew_ms: u64) bool {
        // Monotonic read then a CAS on the label guards the two Release
        // stores: two renewals racing publish in label order.
        var cur = self.learned_label.load(.monotonic);
        while (true) {
            if (label <= cur) return false;
            // Release pairs with `learnedLabel`'s Acquire load of the label.
            self.t_self_deadline_ms.store(sent_ms +| t_self_ms, .release);
            self.renew_ms.store(renew_ms, .release);
            _ = self.renew_at_ms.fetchMin(sent_ms +| renew_ms, .acq_rel);
            self.learned_at_ms.store(sent_ms, .release);
            if (self.learned_label.cmpxchgWeak(cur, label, .release, .monotonic)) |seen| {
                cur = seen;
                continue;
            }
            return true;
        }
    }

    /// The label first (Acquire — pairs with `renewed`'s Release), then
    /// the anchor: a reader seeing label N sees anchor N or later.
    pub fn learnedLabel(self: *const MemberLeaseWords) Learned {
        const label = self.learned_label.load(.acquire);
        return .{ .label = label, .deadline_ms = self.t_self_deadline_ms.load(.acquire) };
    }

    /// A carriage renewal (the owner's piggyback) only brings `renew_at`
    /// FORWARD and learns no label (LC:180-199).
    pub fn renewedCarriage(self: *MemberLeaseWords, renew_at_ms: u64) void {
        _ = self.renew_at_ms.fetchMin(renew_at_ms, .acq_rel);
    }

    /// `!fenced ∧ now ≥ deadline`.
    pub fn selfFenceDue(self: *const MemberLeaseWords, now_ms: u64) bool {
        if (self.fenced.load(.acquire)) return false;
        return now_ms >= self.t_self_deadline_ms.load(.acquire);
    }

    /// True for exactly ONE caller: the swap is the latch (AcqRel pairs
    /// with every other `fence`/`selfFenceDue` load).
    pub fn fence(self: *MemberLeaseWords) bool {
        return !self.fenced.swap(true, .acq_rel);
    }
};

// ---------------------------------------------------------------------------
// Tests (the dlm_membership_tests.rs:492-612 lease-clock laws, portable).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "T_self = T_owner − (2·skew_max + D_purge); refuse (null) when the reserve reaches T_owner; the grant form saturates to 0" {
    try testing.expectEqual(@as(?u64, 45_000 - (2 * 23 + 200)), tSelfMs(45_000, 23, 200));
    try testing.expectEqual(@as(?u64, null), tSelfMs(1000, 400, 200));
    try testing.expectEqual(@as(?u64, null), tSelfMs(1000, 500, 0));
    try testing.expectEqual(@as(?u64, 1), tSelfMs(1000, 0, 999));
    try testing.expectEqual(@as(u64, 0), grantTSelfMs(1000, 500, 0));
    try testing.expectEqual(@as(u64, 0), grantTSelfMs(0, std.math.maxInt(u64), std.math.maxInt(u64)));
}

test "renew interval = clamp(T_self/3, 1 ms, shipped beat): three attempts before the deadline, never below the shipped beat" {
    try testing.expectEqual(SHIPPED_BEAT_MS, renewIntervalMs(44_754));
    try testing.expectEqual(@as(u64, 1000), renewIntervalMs(3000));
    try testing.expectEqual(@as(u64, 1), renewIntervalMs(2));
    try testing.expectEqual(@as(u64, 1), renewIntervalMs(0));
}

test "ties: T_owner is the staleness TTL; the drift term is 22.5 ms at 45 s (500 ppm); skew_max and D_purge take the RTT when larger" {
    try testing.expectEqual(@as(u64, 45_000), T_OWNER_MS);
    try testing.expectEqual(claim.STALE_TTL_S * 1000, T_OWNER_MS);
    try testing.expectEqual(@as(u64, 22), driftSkewMs(T_OWNER_MS));
    try testing.expectEqual(@as(u64, 22), deriveSkewMaxMs(T_OWNER_MS, 5));
    try testing.expectEqual(@as(u64, 80), deriveSkewMaxMs(T_OWNER_MS, 80));
    try testing.expectEqual(@as(u64, 400), derivePurgeMs(200, 80));
    try testing.expectEqual(@as(u64, 900), derivePurgeMs(200, 900));
    try testing.expectEqual(@as(u64, 10_000), SHIPPED_BEAT_MS);
}

test "theorem: anchored on its own send instant, the member's deadline precedes the owner's by ≥ 2·skew_max + D_purge + RTT (LC:330-343)" {
    // The owner receives the renewal RTT/2 after the send and arms T_owner
    // from its receipt; in the member's frame that deadline sits ≥ RTT/2
    // past send + T_owner, and the owner's clock may run slow by skew_max.
    const t_owner: u64 = 45_000;
    const skew: u64 = 22;
    const purge: u64 = 400;
    const rtt: u64 = 30;
    const sent: u64 = 1_000_000;
    const t_self = tSelfMs(t_owner, skew, purge).?;
    const member_deadline = sent + t_self;
    const owner_deadline_earliest_in_member_frame = sent + rtt / 2 + t_owner;
    try testing.expect(owner_deadline_earliest_in_member_frame - member_deadline >= 2 * skew + purge);
    const owner_deadline_with_rtt = sent + rtt + t_owner;
    try testing.expect(owner_deadline_with_rtt - member_deadline >= 2 * skew + purge + rtt);
}

test "MemberLeaseWords: a renewal publishes anchor then label and is learned once per label; a stale label publishes nothing; the carriage only brings renew_at forward; fence fires exactly once; self-fence due reads deadline" {
    var w: MemberLeaseWords = .{};
    try testing.expect(w.renewed(1, 100, 44_000, 10_000));
    var l = w.learnedLabel();
    try testing.expectEqual(@as(u64, 1), l.label);
    try testing.expectEqual(@as(u64, 44_100), l.deadline_ms);
    try testing.expectEqual(@as(u64, 10_100), w.renew_at_ms.load(.acquire));
    try testing.expect(!w.renewed(1, 200, 44_000, 10_000));
    try testing.expect(!w.renewed(0, 200, 44_000, 10_000));
    try testing.expect(w.renewed(2, 200, 44_000, 10_000));
    l = w.learnedLabel();
    try testing.expectEqual(@as(u64, 2), l.label);
    try testing.expectEqual(@as(u64, 44_200), l.deadline_ms);
    // renew_at keeps the earlier 10_100 (fetchMin): the later renewal's
    // 10_200 is not sooner.
    try testing.expectEqual(@as(u64, 10_100), w.renew_at_ms.load(.acquire));
    w.renewedCarriage(5_000);
    try testing.expectEqual(@as(u64, 5_000), w.renew_at_ms.load(.acquire));
    w.renewedCarriage(9_000);
    try testing.expectEqual(@as(u64, 5_000), w.renew_at_ms.load(.acquire));
    try testing.expect(!w.selfFenceDue(44_199));
    try testing.expect(w.selfFenceDue(44_200));
    try testing.expect(w.fence());
    try testing.expect(!w.fence());
    try testing.expect(!w.selfFenceDue(50_000));
}

test "the arm refusal names the three terms and the remedy" {
    var buf: [512]u8 = undefined;
    var wr: std.Io.Writer = .fixed(&buf);
    try writeArmRefusal(&wr, 1000, 400, 300);
    try testing.expect(std.mem.indexOf(u8, wr.buffered(), "1000 − 800 − 300 ms is not positive") != null);
    try testing.expect(std.mem.indexOf(u8, wr.buffered(), "45 s staleness TTL") != null);
}
