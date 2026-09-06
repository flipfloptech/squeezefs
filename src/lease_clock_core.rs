//! DLM **S6**'s lease-clock law + the member-session lease words — the
//! `T_self = T_owner − 2·skew_max − D_purge` arithmetic (`docs/`
//! `pre-rc-engineering-spec.md` §6.7 "Two lease clocks, and the client's
//! is stricter") and the word protocol a member re-anchors its lease
//! through ([`crate::membership::MemberSession`]).
//!
//! Extracted dependency-free (KD-MW-10, `docs/design-full-multi-writer.md`)
//! so `loom-models/` can `#[path]`-include it and model-check the exact
//! shipped transitions — the `gauge_core` pattern; the main build never
//! sets `cfg(loom)`. Two things live here:
//!
//! 1. **The clock law** (pure arithmetic, deterministic tests below):
//!    [`t_self_nanos`] refuses — never clamps — when the reserve
//!    `2·skew_max + D_purge` reaches `T_owner`, because a member that
//!    cannot fail-stop before the owner may re-grant is the divergence
//!    the asymmetry exists to prevent; [`t_self_ms`] is the grant-side
//!    saturating form (zero = a hostile/garbled grant, which must
//!    fail-stop immediately rather than be trusted).
//! 2. **The member lease words** ([`MemberLeaseWords`]): the deadline /
//!    renewal / §6.8-item-3 label words a member session re-anchors on
//!    every renewal. The load-bearing ordering is the **anchor-before-
//!    label publication** in [`MemberLeaseWords::renewed`]: the §6.8
//!    acknowledgement ladder reads `(label, anchor)` as a pair
//!    ([`MemberLeaseWords::learned_label`] — label first, Acquire), so
//!    the anchor must be published FIRST or a ladder that observes the
//!    new label could pair it with the OLD (earlier) anchor and qualify
//!    too soon. Verified by weakening in the loom model
//!    (`lease_clock_models` in `loom-models/src/lib.rs`): flipping the
//!    two stores, or weakening the label Release/Acquire pair to
//!    Relaxed, fails `renewal_label_never_pairs_with_the_old_anchor`.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
}

use atomic::{AtomicBool, AtomicU64, Ordering};

/// One millisecond, in the nanosecond frame the law's exact form runs in.
pub const NANOS_PER_MS: u128 = 1_000_000;

/// The clock law's exact (nanosecond) form: `T_self = T_owner −
/// (2·skew_max + D_purge)`, or `None` when the reserve reaches `T_owner`
/// — the **refusal**, never a clamp (`LeaseClocks::with_params`'s
/// contract: silently shortening someone else's lease would hide the
/// divergence).
pub fn t_self_nanos(t_owner: u128, skew_max: u128, d_purge: u128) -> Option<u128> {
    let reserve = 2 * skew_max + d_purge;
    if reserve >= t_owner {
        return None;
    }
    Some(t_owner - reserve)
}

/// Renewal cadence: `min(T_self / 3, shipped_beat)`, floored at one
/// millisecond — three attempts before the member's own deadline, and
/// never a regression below the cadence the tree already ships.
pub fn renew_interval_nanos(t_self: u128, shipped_beat: u128) -> u128 {
    (t_self / 3).min(shipped_beat).max(NANOS_PER_MS)
}

/// The grant-side millisecond form (`Grant::t_self_ms`): saturating at
/// zero, because a grant whose reserve exceeds its TTL is refused at arm
/// — zero here means a hostile/garbled grant, which must fail-stop
/// immediately rather than be trusted.
pub fn t_self_ms(t_owner_ms: u64, skew_max_ms: u64, d_purge_ms: u64) -> u64 {
    t_owner_ms.saturating_sub(2 * skew_max_ms + d_purge_ms)
}

/// A member's view of its own lease — the words behind
/// [`crate::membership::MemberSession`], anchored on the member's own
/// SEND instant (never a value from the owner's clock), so by
/// construction its deadline precedes the owner's by at least
/// `2·skew_max + D_purge + RTT`.
#[derive(Debug)]
pub struct MemberLeaseWords {
    epoch: AtomicU64,
    t_self_deadline_ms: AtomicU64,
    renew_at_ms: AtomicU64,
    /// The grant's renewal cadence, ms — kept so the renewal loop's
    /// per-attempt deadline bound (finding 2, 2026-08-20: an attempt must
    /// never occupy the lease venue past one cadence) is the plane's own
    /// number rather than a second derivation.
    renew_ms: AtomicU64,
    acked_free_epoch: AtomicU64,
    /// §6.8 item 3: the causal LABEL the last grant carried — the owner's
    /// own monotonic instant of that grant, echoed back once the member
    /// has finished with everything freed at or before it, so the
    /// comparison the writer performs is between two readings of ONE
    /// clock.
    learned_label: AtomicU64,
    /// The member-clock instant at which that label was learned — the
    /// anchor the acknowledgement ladder's qualification wait measures
    /// from (its own clock, for its own durations).
    learned_at_ms: AtomicU64,
    /// The grant's `skew_max` / `D_purge`, kept so the ladder's two waits
    /// are the plane's own numbers rather than a second derivation.
    skew_max_ms: AtomicU64,
    d_purge_ms: AtomicU64,
    fenced: AtomicBool,
}

impl MemberLeaseWords {
    /// Adopt a grant. `anchor_ms` is the instant the member SENT the
    /// request.
    #[allow(clippy::too_many_arguments)] // the grant's seven fields, verbatim
    pub fn adopt(
        epoch: u64,
        t_self_ms: u64,
        renew_ms: u64,
        skew_max_ms: u64,
        d_purge_ms: u64,
        label: u64,
        anchor_ms: u64,
    ) -> Self {
        Self {
            epoch: AtomicU64::new(epoch),
            t_self_deadline_ms: AtomicU64::new(anchor_ms + t_self_ms),
            renew_at_ms: AtomicU64::new(anchor_ms + renew_ms),
            renew_ms: AtomicU64::new(renew_ms),
            acked_free_epoch: AtomicU64::new(0),
            learned_label: AtomicU64::new(label),
            learned_at_ms: AtomicU64::new(anchor_ms),
            skew_max_ms: AtomicU64::new(skew_max_ms),
            d_purge_ms: AtomicU64::new(d_purge_ms),
            fenced: AtomicBool::new(false),
        }
    }

    /// Re-anchor on a successful renewal — including the §6.8 item-3
    /// label this grant carried (monotone: a label is never un-learned).
    #[allow(clippy::too_many_arguments)] // the grant's seven fields, verbatim
    pub fn renewed(
        &self,
        epoch: u64,
        t_self_ms: u64,
        renew_ms: u64,
        skew_max_ms: u64,
        d_purge_ms: u64,
        label: u64,
        anchor_ms: u64,
    ) {
        self.epoch.store(epoch, Ordering::Release);
        self.t_self_deadline_ms
            .store(anchor_ms + t_self_ms, Ordering::Release);
        self.renew_at_ms
            .store(anchor_ms + renew_ms, Ordering::Release);
        self.renew_ms.store(renew_ms, Ordering::Release);
        self.skew_max_ms.store(skew_max_ms, Ordering::Release);
        self.d_purge_ms.store(d_purge_ms, Ordering::Release);
        if label > self.learned_label.load(Ordering::Acquire) {
            // Order matters: the anchor is published FIRST, so a ladder
            // that observes the new label can never pair it with the old
            // (earlier) anchor and qualify too soon. Weakening-verified
            // (loom `renewal_label_never_pairs_with_the_old_anchor`):
            // flipping these two stores — or weakening the label's
            // Release/Acquire pair to Relaxed — fails the model.
            self.learned_at_ms.store(anchor_ms, Ordering::Release);
            self.learned_label.store(label, Ordering::Release);
        }
    }

    /// A CARRIAGE renewal (§6.8 item 3, hold-time lever (b)): the renewal a
    /// promoted acknowledgement triggered ahead of the beat. It renews the
    /// lease exactly like [`Self::renewed`] — epoch, deadline, the grant's
    /// clocks — with two deliberate differences: the beat is only ever
    /// brought FORWARD (`renew_at = min(scheduled, anchor + renew_ms)`, so
    /// a prod riding this grant is honoured and a routine value never
    /// pushes the routine beat later), and the grant's LABEL is NOT
    /// learned. Learning is monotone-safe to skip; what it protects is the
    /// ladder's qualification phase — a label learned just after a pass
    /// (which is when a promotion happens) qualifies a whole pass later
    /// than one the routine beat learns at its own phase, and the ladder
    /// adopts the LAST learned pair. The routine beat stays the label
    /// source; this renewal is pure carriage.
    pub fn renewed_carriage(
        &self,
        epoch: u64,
        t_self_ms: u64,
        renew_ms: u64,
        skew_max_ms: u64,
        d_purge_ms: u64,
        anchor_ms: u64,
    ) {
        self.epoch.store(epoch, Ordering::Release);
        self.t_self_deadline_ms
            .store(anchor_ms + t_self_ms, Ordering::Release);
        self.renew_at_ms
            .fetch_min(anchor_ms + renew_ms, Ordering::AcqRel);
        self.renew_ms.store(renew_ms, Ordering::Release);
        self.skew_max_ms.store(skew_max_ms, Ordering::Release);
        self.d_purge_ms.store(d_purge_ms, Ordering::Release);
    }

    /// §6.8 item 3: the label last learned from the owner and the
    /// member-clock instant it arrived — the acknowledgement ladder's two
    /// inputs. Label first (Acquire): observing a label pins its anchor.
    pub fn learned_label(&self) -> (u64, u64) {
        let label = self.learned_label.load(Ordering::Acquire);
        (label, self.learned_at_ms.load(Ordering::Acquire))
    }

    /// The grant's clock-skew bound, ms.
    pub fn skew_max_ms(&self) -> u64 {
        self.skew_max_ms.load(Ordering::Acquire)
    }

    /// The grant's `D_purge`, ms.
    pub fn d_purge_ms(&self) -> u64 {
        self.d_purge_ms.load(Ordering::Acquire)
    }

    /// The lease epoch to present on the next renewal.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// The member's own deadline, in its clock's milliseconds.
    pub fn t_self_deadline_ms(&self) -> u64 {
        self.t_self_deadline_ms.load(Ordering::Acquire)
    }

    /// When the next renewal is due.
    pub fn renew_at_ms(&self) -> u64 {
        self.renew_at_ms.load(Ordering::Acquire)
    }

    /// The grant's renewal cadence, ms — the renewal loop's per-attempt
    /// deadline floor (an attempt never occupies the lease venue past one
    /// cadence).
    pub fn renew_interval_ms(&self) -> u64 {
        self.renew_ms.load(Ordering::Acquire)
    }

    /// `true` ⇔ the member is past its own deadline and MUST fail-stop
    /// now — before the owner's TTL lets the objects be granted
    /// elsewhere.
    pub fn self_fence_due(&self, now_ms: u64) -> bool {
        !self.fenced.load(Ordering::Acquire)
            && now_ms >= self.t_self_deadline_ms.load(Ordering::Acquire)
    }

    /// Latch the fence. `true` ⇔ this call was the FIRST — the
    /// exactly-once edge every fence side effect (metrics, custody
    /// poison) keys on. Weakening-verified (loom
    /// `self_fence_side_effects_run_exactly_once`): replacing the swap
    /// with a load-then-store fails the model.
    pub fn fence(&self) -> bool {
        !self.fenced.swap(true, Ordering::AcqRel)
    }

    /// `true` ⇔ this session has fail-stopped.
    pub fn fenced(&self) -> bool {
        self.fenced.load(Ordering::Acquire)
    }

    /// Acknowledge having passed freed-offset `epoch` (§6.8 item 3).
    /// Monotone — an acknowledgement is never withdrawn.
    pub fn ack_free_epoch(&self, epoch: u64) {
        self.acked_free_epoch.fetch_max(epoch, Ordering::AcqRel);
    }

    /// The freed-offset epoch this member has acknowledged.
    pub fn acked_free_epoch(&self) -> u64 {
        self.acked_free_epoch.load(Ordering::Acquire)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// The law's exact identity and its refusal edge: `T_self + reserve
    /// == T_owner` whenever admitted, and `reserve >= T_owner` refuses
    /// (never clamps).
    #[test]
    fn t_self_law_is_exact_and_refuses_at_the_reserve() {
        // The shipped 45 s TTL with the 22.5 ms drift bound and a 2 s
        // purge window.
        let t_owner = 45_000 * NANOS_PER_MS;
        let skew = 22_500_000u128; // 22.5 ms in ns — sub-ms precision is real
        let purge = 2_000 * NANOS_PER_MS;
        let t_self = t_self_nanos(t_owner, skew, purge).expect("the shipped shape admits");
        assert_eq!(t_self + 2 * skew + purge, t_owner, "the identity is exact");

        // The refusal edge: reserve == T_owner refuses (>=, not >).
        assert_eq!(t_self_nanos(2 * skew + purge, skew, purge), None);
        assert_eq!(t_self_nanos(0, 0, 0), None, "a zero TTL admits nothing");
    }

    /// The grant-side ms form saturates at zero (a garbled grant must
    /// fail-stop immediately, not be trusted).
    #[test]
    fn grant_t_self_ms_saturates_to_immediate_fence() {
        assert_eq!(t_self_ms(45_000, 25, 2_000), 45_000 - 2 * 25 - 2_000);
        assert_eq!(t_self_ms(1_000, 400, 300), 0, "reserve >= TTL is zero");
    }

    /// Renewal cadence: three attempts before the deadline, never a
    /// regression below the shipped beat, floored at 1 ms.
    #[test]
    fn renew_interval_gives_three_attempts_and_holds_the_floor() {
        let beat = 10_000 * NANOS_PER_MS;
        assert_eq!(
            renew_interval_nanos(45_000 * NANOS_PER_MS, beat),
            beat,
            "a long T_self keeps the shipped beat"
        );
        assert_eq!(
            renew_interval_nanos(9_000 * NANOS_PER_MS, beat),
            3_000 * NANOS_PER_MS,
            "a short T_self yields three attempts"
        );
        assert_eq!(
            renew_interval_nanos(1, beat),
            NANOS_PER_MS,
            "the 1 ms floor"
        );
    }

    /// The margin theorem the WERO/demotion work leans on: a member's
    /// deadline (anchored on its own SEND instant) plus its purge
    /// completes at least `2·skew_max` before the owner's re-grant
    /// instant — the round trip only widens the margin.
    #[test]
    fn member_deadline_plus_purge_precedes_the_owner_regrant() {
        let (t_owner, skew, purge) = (45_000u64, 25u64, 2_000u64);
        let t_self = t_self_ms(t_owner, skew, purge);
        for rtt in [0u64, 1, 50, 400] {
            let send = 100_000u64;
            let owner_grant = send + rtt; // the owner anchors at receipt
            let member_deadline = send + t_self;
            let owner_regrant = owner_grant + t_owner;
            assert!(
                member_deadline + purge + 2 * skew <= owner_regrant,
                "rtt {rtt}: the member must finish fencing 2·skew before re-grant"
            );
        }
    }
}
