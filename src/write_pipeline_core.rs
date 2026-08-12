//! Write-pipeline admission accounting core (2026-07-27 depth campaign).
//!
//! The lock-free heart of [`crate::write_pipeline::WritePipeline`]: the
//! in-flight byte/block gauges and the single-CAS admission attempt the
//! `admit` loop drives. Self-contained so `loom-models/` can
//! `#[path]`-include it and exhaustively check the admission/release
//! interleavings (over-admission, empty-pipe bypass, release underflow,
//! settle-to-zero). The main build never sets `cfg(loom)`.
//!
//! **Wake-liveness precondition (stated because the model cannot see its
//! violation):** the shipped `admit` parks on `tokio::sync::Notify::
//! notified()` raced against a 5 ms tick. `notify_waiters` stores no
//! permit, so a release's wake CAN be lost to a not-yet-parked waiter —
//! BY DESIGN the tick is the liveness backstop (the waiter re-polls the
//! predicate at most 5 ms late; Red/target changes are observed the same
//! way). The loom model therefore treats parking as a spurious-wake loop
//! (tick semantics) and checks the ACCOUNTING invariants exhaustively;
//! it deliberately does not certify permit-style wake delivery, which
//! the implementation does not claim.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU64, Ordering};
}

use atomic::{AtomicU64, Ordering};

/// Outcome of one admission attempt (one predicate evaluation + at most
/// one CAS) against a caller-supplied byte target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitAttempt {
    /// Charged: the caller owns `bytes` of in-flight custody (release
    /// exactly once).
    Admitted,
    /// The CAS raced a concurrent admission/completion — re-evaluate
    /// (the caller's loop recomputes the target first).
    Raced,
    /// The pipe is at target — park and retry.
    Full,
}

/// In-flight admission gauges. Invariants (loom-checked):
///
/// 1. **Bounded admission**: every successful CAS observed
///    `cur + bytes <= target` (or the empty-pipe bypass below), so at the
///    instant of admission `inflight_bytes <= max(target, bypass_bytes)`.
/// 2. **Single oversized bypass**: `blocks == 0` admits one block larger
///    than the target (progress guarantee); the CAS on `inflight_bytes`
///    serializes racing bypassers — at most ONE oversized admission can
///    land on an empty pipe, the loser re-observes a non-empty pipe.
/// 3. **Exact settle**: blocks/bytes match outstanding admissions at all
///    times and return to exactly zero once every admission released.
#[derive(Default)]
pub struct AdmissionCore {
    inflight_bytes: AtomicU64,
    inflight_blocks: AtomicU64,
}

impl AdmissionCore {
    pub fn new() -> Self {
        Self {
            inflight_bytes: AtomicU64::new(0),
            inflight_blocks: AtomicU64::new(0),
        }
    }

    /// One admission attempt for `bytes` against `target` (see
    /// [`AdmitAttempt`]). The empty-pipe bypass keeps oversized blocks
    /// admissible (progress guarantee: an empty pipe always admits).
    ///
    /// The bypass predicate is `cur == 0` on the BYTES gauge — the same
    /// word the CAS charges — never the blocks counter: the counter is
    /// incremented after the CAS, so it LAGS, and a predicate reading it
    /// admitted TWO racing oversized bypassers onto an empty pipe (loom
    /// `write_pipeline_empty_pipe_bypass_is_single`, red against the
    /// blocks-counter shape this replaced). With `cur == 0` the CAS
    /// itself serializes bypassers: the loser re-observes a non-zero
    /// gauge and parks.
    pub fn try_admit_once(&self, bytes: u64, target: u64) -> AdmitAttempt {
        let cur = self.inflight_bytes.load(Ordering::Relaxed);
        if cur != 0 && cur.saturating_add(bytes) > target {
            return AdmitAttempt::Full;
        }
        if self
            .inflight_bytes
            .compare_exchange(cur, cur + bytes, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            self.inflight_blocks.fetch_add(1, Ordering::AcqRel);
            AdmitAttempt::Admitted
        } else {
            AdmitAttempt::Raced
        }
    }

    /// Return `bytes` of custody (exactly once per admission; the RAII
    /// permit in `write_pipeline.rs` owns the exactly-once).
    pub fn release(&self, bytes: u64) {
        self.inflight_bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.inflight_blocks.fetch_sub(1, Ordering::AcqRel);
    }

    pub fn inflight_bytes(&self) -> u64 {
        self.inflight_bytes.load(Ordering::Relaxed)
    }

    pub fn inflight_blocks(&self) -> u64 {
        self.inflight_blocks.load(Ordering::Acquire)
    }
}

// =========================================================================
// Probe-up governor core (2026-07-29 campaign)
// =========================================================================

/// Probe epoch length (ms) — two governor bandwidth windows: long enough
/// that a windowed delivery rate is meaningful, short enough that a full
/// headroom ramp (floor → 32× at +1/4 per adopted epoch, ~15 epochs)
/// completes in seconds on a saturated stream.
pub const PROBE_EPOCH_MS: u64 = 500;

/// Fixed-point unit of the probe multiplier (Q6: 64 = ×1.0).
pub const PROBE_MUL_ONE: u64 = 64;

/// Probe multiplier bound: ×32 over the BDP-governed target. Purely a
/// runaway guard — the R5 budget cap is the operative absolute bound
/// (never a depth constant: the multiplicand is the measured BDP).
pub const PROBE_MUL_MAX: u64 = PROBE_MUL_ONE * 32;

/// Saturated epochs to hold after a DEAD-GAIN retreat before probing
/// again: bounds the dead-gain latency tax to ~1/(N+1) of epochs (the
/// BBR probe duty-cycle shape).
pub const PROBE_COOLDOWN_EPOCHS: u64 = 8;

/// Probe state: multiplier held (probe may launch).
const PROBE_STATE_HOLD: u64 = 0;
/// Probe state: a raised multiplier is being measured against baseline.
const PROBE_STATE_PROBING: u64 = 1;

/// The BBR-flavored probe-up layer over the pure-BDP depth target
/// (2026-07-29 campaign; field conviction: a target of measured
/// bandwidth × measured service time converges to sustaining the CURRENT
/// operating point — a self-fulfilling equilibrium that left 18 % on a
/// 4-node 2×200GbE cluster).
///
/// Cycle, per [`PROBE_EPOCH_MS`] epoch (rolled by whichever completion
/// thread wins the epoch CAS — the `Lane` window shape):
///
/// * **HOLD, unsaturated** — decay the multiplier by 1/8 toward 1.0 (the
///   latency guard: low offered load never inherits streaming depth).
/// * **HOLD, saturated, elevated, delivery collapsed** (< 7/8 of the
///   adopted rate) — step back one gain (×0.8): adopted depth keeps
///   paying rent. Senior to launching — a collapsed baseline must never
///   seed a fresh probe.
/// * **HOLD, saturated, headroom, cooled** — LAUNCH: baseline = this
///   epoch's delivery rate, multiplier += 1/4.
/// * **PROBING → delivery ≥ baseline + 1/16** — ADOPT (keep the raised
///   multiplier, re-arm immediately: discovery compounds).
/// * **PROBING → otherwise** — RETREAT to the pre-probe multiplier;
///   dead gain also COOLS DOWN ([`PROBE_COOLDOWN_EPOCHS`]).
///
/// All fields are approximate latch-free gauges (the `Lane` posture): a
/// torn roll self-heals within one epoch. Only the epoch CAS is
/// serialization-bearing — and only its ATOMICITY (single roller), not
/// its ordering; the loom model verifies the single-roll and bound
/// invariants and was weakening-verified against a non-CAS roll.
#[derive(Default)]
pub struct ProbeCore {
    mul_q6: AtomicU64,
    prev_mul_q6: AtomicU64,
    state: AtomicU64,
    /// Delivery rate (B/s) of the epoch that launched the running probe.
    baseline_bps: AtomicU64,
    /// Delivery rate (B/s) that justified the current adopted multiplier.
    adopted_bps: AtomicU64,
    cooldown: AtomicU64,
    epoch_start_ms: AtomicU64,
    epoch_bytes: AtomicU64,
    ups: AtomicU64,
    backoffs: AtomicU64,
}

impl ProbeCore {
    pub fn new() -> Self {
        Self {
            mul_q6: AtomicU64::new(PROBE_MUL_ONE),
            prev_mul_q6: AtomicU64::new(PROBE_MUL_ONE),
            state: AtomicU64::new(PROBE_STATE_HOLD),
            baseline_bps: AtomicU64::new(0),
            adopted_bps: AtomicU64::new(0),
            cooldown: AtomicU64::new(0),
            epoch_start_ms: AtomicU64::new(0),
            epoch_bytes: AtomicU64::new(0),
            ups: AtomicU64::new(0),
            backoffs: AtomicU64::new(0),
        }
    }

    /// Count `bytes` of completed upload delivery into the running epoch.
    pub fn on_bytes(&self, bytes: u64) {
        self.epoch_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Roll the probe epoch if due. `saturated` = offered load filled the
    /// current target this epoch (admission parked, or the pipe sat at
    /// target); `headroom` = the effective target is below the R5 cap and
    /// the budget is not Red. Returns `true` iff THIS call rolled (the
    /// caller then refreshes its saturation snapshot).
    pub fn roll(&self, now_ms: u64, saturated: bool, headroom: bool) -> bool {
        let ws = self.epoch_start_ms.load(Ordering::Relaxed);
        if ws == 0 {
            let _ = self.epoch_start_ms.compare_exchange(
                0,
                now_ms.max(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            return false;
        }
        let elapsed = now_ms.saturating_sub(ws);
        if elapsed < PROBE_EPOCH_MS
            || self
                .epoch_start_ms
                .compare_exchange(ws, now_ms, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
        {
            return false;
        }
        // This thread rolls the epoch.
        let bps = self
            .epoch_bytes
            .swap(0, Ordering::Relaxed)
            .saturating_mul(1000)
            / elapsed.max(1);
        let mul = self.mul_q6.load(Ordering::Relaxed);
        match self.state.load(Ordering::Relaxed) {
            PROBE_STATE_PROBING => {
                let baseline = self.baseline_bps.load(Ordering::Relaxed);
                if saturated && bps >= baseline.saturating_add((baseline / 16).max(1)) {
                    // ADOPT: delivery responded to the raised depth. Keep
                    // the multiplier and re-arm immediately (discovery
                    // compounds while the backend keeps responding).
                    self.adopted_bps.store(bps, Ordering::Relaxed);
                    self.state.store(PROBE_STATE_HOLD, Ordering::Relaxed);
                } else {
                    // RETREAT to the pre-probe multiplier (the BDP
                    // posture). Dead gain under saturation also cools
                    // down; an unsaturated probe epoch is merely
                    // inconclusive (no cool-down — load may return).
                    self.mul_q6
                        .store(self.prev_mul_q6.load(Ordering::Relaxed), Ordering::Relaxed);
                    self.state.store(PROBE_STATE_HOLD, Ordering::Relaxed);
                    if saturated {
                        self.cooldown
                            .store(PROBE_COOLDOWN_EPOCHS, Ordering::Relaxed);
                    }
                    self.backoffs.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {
                let adopted = self.adopted_bps.load(Ordering::Relaxed);
                let collapsed =
                    mul > PROBE_MUL_ONE && adopted > 0 && bps < adopted.saturating_sub(adopted / 8);
                if !saturated {
                    // The latency guard: bleed the multiplier toward 1.0
                    // while offered load does not fill the pipe.
                    let decayed = (mul - mul / 8).max(PROBE_MUL_ONE);
                    self.mul_q6.store(decayed, Ordering::Relaxed);
                } else if collapsed {
                    // HOLD re-validation (senior to launching — a
                    // collapsed baseline must never seed a fresh probe):
                    // adopted depth must keep paying rent, so a collapsed
                    // delivery rate steps back one probe gain (×0.8)
                    // toward the BDP.
                    self.mul_q6
                        .store((mul - mul / 5).max(PROBE_MUL_ONE), Ordering::Relaxed);
                    self.adopted_bps.store(bps, Ordering::Relaxed);
                    self.backoffs.fetch_add(1, Ordering::Relaxed);
                } else if self.cooldown.load(Ordering::Relaxed) > 0 {
                    self.cooldown.fetch_sub(1, Ordering::Relaxed);
                } else if headroom && mul < PROBE_MUL_MAX && bps > 0 {
                    // LAUNCH: baseline this epoch's delivery, raise the
                    // target by the probe gain (+1/4).
                    self.prev_mul_q6.store(mul, Ordering::Relaxed);
                    self.baseline_bps.store(bps, Ordering::Relaxed);
                    self.mul_q6
                        .store((mul + mul / 4).min(PROBE_MUL_MAX), Ordering::Relaxed);
                    self.state.store(PROBE_STATE_PROBING, Ordering::Relaxed);
                    self.ups.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        true
    }

    /// Current probe multiplier (Q6 fixed point; [`PROBE_MUL_ONE`] = ×1.0).
    pub fn mul_q6(&self) -> u64 {
        self.mul_q6.load(Ordering::Relaxed)
    }

    /// Probes launched (`write_pipeline_depth_probe_ups`).
    pub fn probe_ups(&self) -> u64 {
        self.ups.load(Ordering::Relaxed)
    }

    /// Retreats/step-downs (`write_pipeline_depth_probe_backoffs`).
    pub fn probe_backoffs(&self) -> u64 {
        self.backoffs.load(Ordering::Relaxed)
    }
}

// =========================================================================
// Issue-cadence governor core (write-wall Addendum 7, 2026-08-12)
// =========================================================================

/// The smallest meaningful eager threshold — a structural minimum, not
/// tuning: below 2 the cadence IS per-op issue (the counted K=1 tax:
/// −18 % at qd32, every enter amortizing over nothing).
pub const CADENCE_K_FLOOR: u64 = 2;

/// The dd issue-cadence governor (the `ProbeCore` law with the search
/// DIRECTION inverted and the entry point MEASURED): the eager-flush
/// threshold K is probed DOWNWARD by halving from the observed sweep
/// claim size — K = 0 is sweep-only (the OFF posture), each probe cuts
/// K in half, and every step is delivery-gated exactly like the depth
/// governor (adopt on ≥ +1/16 ops response under saturation, retreat +
/// cool down on dead gain, step back toward OFF when adopted delivery
/// collapses, decay to OFF on unsaturated epochs). Addendum 7's basis:
/// K = 16 was a counted +4–8 % at qd32 but every closed-form derivation
/// failed (`inflight/2` falsified in-bracket), so the threshold must be
/// DISCOVERED against live delivery — one law for every regime, no
/// shape-specific pathway, no constant (the floor is the per-op-issue
/// physical minimum; the OFF boundary is the ring size, above which a
/// threshold structurally cannot fire before the sweep does).
///
/// Saturation is computed from the governor's own claim census —
/// `epoch_claim_max > CADENCE_K_FLOOR` (sweeps batch beyond the floor,
/// so cadence has something to act on) — never a knob. The epoch roll
/// is the `ProbeCore` single-winner CAS verbatim (loom:
/// `dd_cadence_epoch_roll_is_single_winner`).
#[derive(Default)]
pub struct CadenceCore {
    /// Current threshold; 0 = sweep-only (OFF).
    k: AtomicU64,
    prev_k: AtomicU64,
    state: AtomicU64,
    baseline_ops_s: AtomicU64,
    adopted_ops_s: AtomicU64,
    cooldown: AtomicU64,
    epoch_start_ms: AtomicU64,
    epoch_ops: AtomicU64,
    epoch_claim_max: AtomicU64,
    ups: AtomicU64,
    backoffs: AtomicU64,
}

impl CadenceCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one flush claim of `n` staged SQEs (the sweep-size census).
    pub fn on_claim(&self, n: u64) {
        self.epoch_claim_max.fetch_max(n, Ordering::Relaxed);
    }

    /// Count `n` completed ops into the running epoch.
    pub fn on_ops(&self, n: u64) {
        self.epoch_ops.fetch_add(n, Ordering::Relaxed);
    }

    /// Current governed threshold: 0 = sweep-only.
    pub fn k(&self) -> u64 {
        self.k.load(Ordering::Relaxed)
    }

    pub fn probe_ups(&self) -> u64 {
        self.ups.load(Ordering::Relaxed)
    }

    pub fn probe_backoffs(&self) -> u64 {
        self.backoffs.load(Ordering::Relaxed)
    }

    /// One step toward OFF: doubling past the ring's capacity is
    /// structurally sweep-only (a threshold ≥ ring cannot fire first).
    fn step_off(k: u64, ring: u64) -> u64 {
        let next = k.saturating_mul(2);
        if next >= ring {
            0
        } else {
            next
        }
    }

    /// Roll the cadence epoch if due (single-winner CAS — the
    /// `ProbeCore` shape). `ring` = the consumer's ring depth (the OFF
    /// boundary). Returns `true` iff THIS call rolled.
    pub fn roll(&self, now_ms: u64, ring: u64) -> bool {
        let ws = self.epoch_start_ms.load(Ordering::Relaxed);
        if ws == 0 {
            let _ = self.epoch_start_ms.compare_exchange(
                0,
                now_ms.max(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            return false;
        }
        let elapsed = now_ms.saturating_sub(ws);
        if elapsed < PROBE_EPOCH_MS
            || self
                .epoch_start_ms
                .compare_exchange(ws, now_ms, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
        {
            return false;
        }
        let ops_s = self
            .epoch_ops
            .swap(0, Ordering::Relaxed)
            .saturating_mul(1000)
            / elapsed.max(1);
        let claim_max = self.epoch_claim_max.swap(0, Ordering::Relaxed);
        let saturated = claim_max > CADENCE_K_FLOOR;
        let k = self.k.load(Ordering::Relaxed);
        match self.state.load(Ordering::Relaxed) {
            PROBE_STATE_PROBING => {
                let baseline = self.baseline_ops_s.load(Ordering::Relaxed);
                if saturated && ops_s >= baseline.saturating_add((baseline / 16).max(1)) {
                    // ADOPT: delivery responded to the tighter cadence.
                    self.adopted_ops_s.store(ops_s, Ordering::Relaxed);
                    self.state.store(PROBE_STATE_HOLD, Ordering::Relaxed);
                } else {
                    // RETREAT; dead gain under saturation cools down.
                    self.k
                        .store(self.prev_k.load(Ordering::Relaxed), Ordering::Relaxed);
                    self.state.store(PROBE_STATE_HOLD, Ordering::Relaxed);
                    if saturated {
                        self.cooldown
                            .store(PROBE_COOLDOWN_EPOCHS, Ordering::Relaxed);
                    }
                    self.backoffs.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {
                let adopted = self.adopted_ops_s.load(Ordering::Relaxed);
                let collapsed = k > 0 && adopted > 0 && ops_s < adopted.saturating_sub(adopted / 8);
                if !saturated {
                    // Latency/idle guard: bleed one notch toward OFF.
                    if k > 0 {
                        self.k.store(Self::step_off(k, ring), Ordering::Relaxed);
                    }
                } else if collapsed {
                    // Adopted cadence must keep paying rent.
                    self.k.store(Self::step_off(k, ring), Ordering::Relaxed);
                    self.adopted_ops_s.store(ops_s, Ordering::Relaxed);
                    self.backoffs.fetch_add(1, Ordering::Relaxed);
                } else if self.cooldown.load(Ordering::Relaxed) > 0 {
                    self.cooldown.fetch_sub(1, Ordering::Relaxed);
                } else if ops_s > 0 {
                    // LAUNCH: halve toward the floor; the entry point is
                    // the MEASURED claim size (sweep→claim/2 is where
                    // the counted optimum lived).
                    let cand = if k == 0 { claim_max / 2 } else { k / 2 }.max(CADENCE_K_FLOOR);
                    if cand != k && cand < claim_max {
                        self.prev_k.store(k, Ordering::Relaxed);
                        self.baseline_ops_s.store(ops_s, Ordering::Relaxed);
                        self.k.store(cand, Ordering::Relaxed);
                        self.state.store(PROBE_STATE_PROBING, Ordering::Relaxed);
                        self.ups.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        true
    }
}

#[cfg(all(test, not(loom)))]
mod cadence_tests {
    use super::*;

    const RING: u64 = 512;
    const E: u64 = PROBE_EPOCH_MS + 1;

    /// Drive one epoch: claims of max size `claim`, `ops` completions.
    fn epoch(c: &CadenceCore, t: &mut u64, claim: u64, ops: u64) {
        c.on_claim(claim);
        c.on_ops(ops);
        *t += E;
        assert!(c.roll(*t, RING), "the due epoch must roll");
    }

    #[test]
    fn off_by_default_and_probes_down_from_the_measured_claim() {
        let c = CadenceCore::new();
        let mut t = 1;
        assert_eq!(c.k(), 0, "fresh governor is sweep-only");
        assert!(!c.roll(t, RING), "first call opens the epoch window");
        epoch(&c, &mut t, 32, 400_000);
        assert_eq!(c.k(), 16, "first probe = measured claim size / 2");
        assert_eq!(c.probe_ups(), 1);
    }

    #[test]
    fn adopts_on_delivery_response_and_compounds() {
        let c = CadenceCore::new();
        let mut t = 1;
        assert!(!c.roll(t, RING));
        epoch(&c, &mut t, 32, 400_000); // launch k=16
        epoch(&c, &mut t, 32, 440_000); // +10% ⇒ adopt
        assert_eq!(c.k(), 16, "adopted");
        epoch(&c, &mut t, 32, 440_000); // launch again ⇒ k=8
        assert_eq!(c.k(), 8, "discovery compounds by halving");
        assert_eq!(c.probe_ups(), 2);
    }

    #[test]
    fn dead_gain_retreats_and_cools_down() {
        let c = CadenceCore::new();
        let mut t = 1;
        assert!(!c.roll(t, RING));
        epoch(&c, &mut t, 32, 400_000); // launch k=16
        epoch(&c, &mut t, 32, 400_000); // flat ⇒ retreat
        assert_eq!(c.k(), 0, "retreat restores the pre-probe posture");
        assert_eq!(c.probe_backoffs(), 1);
        for _ in 0..PROBE_COOLDOWN_EPOCHS {
            epoch(&c, &mut t, 32, 400_000);
            assert_eq!(c.k(), 0, "cooldown holds the posture");
        }
        epoch(&c, &mut t, 32, 400_000);
        assert_eq!(c.k(), 16, "cooled down ⇒ probing resumes");
    }

    #[test]
    fn collapse_steps_toward_off_and_idle_decays_to_off() {
        let c = CadenceCore::new();
        let mut t = 1;
        assert!(!c.roll(t, RING));
        epoch(&c, &mut t, 32, 400_000); // launch k=16
        epoch(&c, &mut t, 32, 440_000); // adopt k=16 @440k
        epoch(&c, &mut t, 32, 300_000); // collapse (< 7/8 of adopted)…
                                        // (that epoch LAUNCHED or collapsed depending on order: the
                                        // collapse guard is senior — k stepped toward off.)
        assert_eq!(c.k(), 32, "collapse steps one notch toward OFF");
        // Idle epochs decay the rest of the way to OFF.
        let mut idle = 0;
        while c.k() != 0 {
            epoch(&c, &mut t, 0, 0);
            idle += 1;
            assert!(idle < 12, "idle decay must reach OFF");
        }
    }

    #[test]
    fn floor_is_the_per_op_issue_minimum() {
        let c = CadenceCore::new();
        let mut t = 1;
        assert!(!c.roll(t, RING));
        epoch(&c, &mut t, 8, 400_000); // launch k=4
        assert_eq!(c.k(), 4);
        epoch(&c, &mut t, 8, 440_000); // adopt
        epoch(&c, &mut t, 8, 480_000); // launch k=2 (the floor)
        assert_eq!(c.k(), 2);
        epoch(&c, &mut t, 8, 520_000); // adopt
        epoch(&c, &mut t, 8, 560_000); // floor reached: no further probe
        assert_eq!(c.k(), 2, "the floor is terminal for downward probes");
    }
}
