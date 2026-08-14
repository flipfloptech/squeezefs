//! Adaptive service-thread spin governor (client-topology campaign,
//! 2026-08-14 — `.benchmarks/2026-08-14-client-topology-census.md`).
//!
//! The campaign's round-4 attribution counted the svc **park-cycle
//! latency** term: at 32-proc fan-in the svc lanes park ~50k/s, every
//! park makes the next burst pay a wake→run cycle (~10 µs/slice
//! runqueue wait at fan-in), and a static `SQUEEZEFS_IPC_SPIN_US=100`
//! bought +6.7 % median (A-B-B-A, engagement exact) — on a box that was
//! ~17 % busy, where the lever's documented CPU tax (the 2026-07-26
//! reap-economy verdict: every ambient nonzero window stole protected
//! sync-lane CPU) is free.
//!
//! The governor makes that win ambient WITHOUT re-introducing the
//! ambient tax, by deriving the window per lane from two live signals:
//!
//! 1. **Park churn** (the regime signal): the lane's own EWMA of park
//!    DURATION. Short parks (the wake arrived almost immediately) mean
//!    a spin of that length would have absorbed the park/wake cycle;
//!    long parks mean the mount is idle and spinning is pure waste.
//!    The window is `min(2 × ewma_park, RAIL)` — sized by the lane's
//!    own observed wake gap, never a constant.
//! 2. **CPU headroom** (the theft guard): the spin population must fit
//!    inside the box's idle capacity with 2× margin —
//!    `busy_pct ≤ 100 − 2 · (lanes/cores) · 100` (all lanes spinning
//!    twice over still leave the former workload's CPU untouched).
//!    Derived from lanes and cores per the resource-derivation law; the
//!    busy sample rides a coarse cadence (one `/proc/stat` read per
//!    [`HEADROOM_SAMPLE`] across all lanes).
//!
//! Saturation bleeds the window to 0 (the ceiling check), idleness
//! bleeds it to 0 (the EWMA grows past the rail), and the qd1 latency
//! shapes never engage it meaningfully (a qd1 mount's parks are long —
//! device-RTT-spaced — so the EWMA sits far above the rail).
//!
//! **Explicit wins verbatim** (the ipc-cap law): a set
//! `SQUEEZEFS_IPC_SPIN_US` (including `0`) is the static window exactly
//! as shipped, governor OFF. `SQUEEZEFS_IPC_SPIN_ADAPTIVE=0` is the A/B
//! control (window 0 = the pre-campaign shipped posture).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// The churn-regime bound, µs: parks at or below this duration are the
/// park-churn regime (a spin can absorb them); longer parks are idle /
/// device-RTT spacing where spinning is waste. The counted dose-response
/// plateau's outer edge (2026-08-14 census round 4). A MEASURED class
/// constant (the `fold_fill = 16` pattern), not a tunable.
pub const SPIN_RAIL_US: u64 = 200;

/// The ENGAGED window, µs: the counted A-B-B-A optimum (100 µs → +6.7 %
/// median at 32×8; 20 µs → +2 %, 200 µs → +4 %, 400+ flat — the win
/// spans inter-burst TAIL gaps, so an EWMA-proportional window
/// under-sizes it: the first governor derived 2×EWMA ≈ 10–20 µs at
/// fan-in and counted a WASH against its own control, the falsification
/// that landed this constant). Regime membership stays derived; the
/// magnitude is measured.
pub const SPIN_WINDOW_US: u64 = 100;

/// Headroom sample cadence: one `/proc/stat` read per this window across
/// ALL lanes (a gauge read, not a hot-path cost).
pub const HEADROOM_SAMPLE: Duration = Duration::from_millis(100);

/// EWMA weight (α = 1/8, the standing smoothing constant class): new
/// park observations move the estimate an eighth of the way.
const EWMA_SHIFT: u32 = 3;

/// The busy-percent ceiling above which the governor disengages: the
/// spin population (every lane spinning) must fit in the box's idle
/// capacity — `busy + lanes/cores ≤ 100 %`. `lanes = 12, cores = 32`
/// (the field box) → 62 %. (The first cut used a 2× margin → 25 %,
/// and its engagement probe counted 1.64M of 1.82M park cycles refused
/// on the very venue where the static window WON — /proc/stat busy
/// includes the nvme-tcp softirq the workload itself generates, so the
/// 2× margin disengaged the governor exactly where its win lives. 1×
/// still refuses the 2026-07-26 loss venue: a saturated fleet runs
/// busy ≥ 80 %.)
pub fn busy_ceiling_pct(lanes: usize, cores: usize) -> u32 {
    let cores = cores.max(1);
    // The 2026-08-14 rounding doctrine, PROTECTIVE-BOUND direction:
    // allocations round up, but this is a safety ceiling — the spin
    // SHARE rounds UP so the ceiling rounds DOWN (a fractional share
    // must count fully against headroom, never partially).
    let spin_share = (100 * lanes).div_ceil(cores);
    100u32.saturating_sub(spin_share.min(100) as u32)
}

/// Fold one observed park duration into the lane's EWMA (ns).
/// First observation seeds the estimate verbatim.
pub fn fold_park(ewma_ns: u64, observed_ns: u64) -> u64 {
    if ewma_ns == 0 {
        return observed_ns.max(1);
    }
    ewma_ns - (ewma_ns >> EWMA_SHIFT) + (observed_ns >> EWMA_SHIFT)
}

/// The derived window for one lane: the measured plateau window
/// ([`SPIN_WINDOW_US`]) when the lane is in the churn regime with CPU
/// headroom; else 0.
///
/// Regime membership is TWO structural conditions, both derived:
/// 1. **Fan-in** (`lane_sessions ≥ 2`): the spin absorbs CROSS-CLIENT
///    arrival interleave — with one session, a park is the productive
///    RTT wait of the sync lane, and spinning taxes exactly the
///    protected row the 2026-07-26 verdict named. The temporal test
///    alone was FALSIFIED by the qd1 guard row (2026-08-14: qd1 parks
///    are RTT-spaced ≈ 44 µs — INSIDE the rail — and the engaged
///    governor cost qd1 2.5×, p99 70 → 157 µs; the fleet's parks and a
///    fast device's qd1 parks are temporally indistinguishable, but
///    structurally opposite).
/// 2. **Churn** (park EWMA ≤ [`SPIN_RAIL_US`]): idle mounts park long
///    and derive 0.
///
/// A lane that has never parked (ewma 0) derives 0 — the governor
/// engages only on OBSERVED churn, never speculatively.
pub fn window(
    ewma_park_ns: u64,
    busy_pct: u32,
    lanes: usize,
    cores: usize,
    lane_sessions: usize,
) -> Duration {
    if lane_sessions < 2 {
        // Single-session lane: the sync-RTT regime — never spin.
        return Duration::ZERO;
    }
    if ewma_park_ns == 0 {
        return Duration::ZERO;
    }
    if ewma_park_ns > SPIN_RAIL_US * 1_000 {
        // Idle regime: parks are long (device-RTT- or idle-spaced);
        // spinning cannot absorb them.
        return Duration::ZERO;
    }
    if busy_pct > busy_ceiling_pct(lanes, cores) {
        // Saturation: the 2026-07-26 theft verdict governs — bleed to 0.
        return Duration::ZERO;
    }
    Duration::from_micros(SPIN_WINDOW_US)
}

/// The shared headroom gauge: any lane may sample, rate-limited by a
/// monotonic-ns CAS; every lane reads the published percent. Readings
/// are injected by the caller (`/proc/stat` deltas in production, test
/// values in the suite) — the gauge itself is pure state.
pub struct HeadroomGauge {
    busy_pct: AtomicU32,
    last_sample_ns: AtomicU64,
    /// Previous (busy_jiffies, total_jiffies) packed — the delta base.
    prev: AtomicU64,
}

impl HeadroomGauge {
    pub const fn new() -> Self {
        Self {
            busy_pct: AtomicU32::new(0),
            last_sample_ns: AtomicU64::new(0),
            prev: AtomicU64::new(0),
        }
    }

    /// Published busy percent (0–100).
    pub fn busy_pct(&self) -> u32 {
        self.busy_pct.load(Ordering::Relaxed)
    }

    /// Claim the sampling slot if the cadence has elapsed: returns true
    /// for exactly one caller per window (that caller then reads
    /// `/proc/stat` and publishes via [`Self::publish`]).
    pub fn should_sample(&self, now_ns: u64) -> bool {
        let last = self.last_sample_ns.load(Ordering::Relaxed);
        if now_ns.saturating_sub(last) < HEADROOM_SAMPLE.as_nanos() as u64 {
            return false;
        }
        self.last_sample_ns
            .compare_exchange(last, now_ns, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// Publish a `(busy_jiffies, total_jiffies)` reading; the percent is
    /// computed over the delta from the previous reading (first reading
    /// only seeds the base). Jiffy counters are packed 32/32 — /proc/stat
    /// aggregates wrap far beyond any sampling horizon at 32 bits of
    /// centi-seconds, and a wrapped delta simply re-seeds.
    pub fn publish(&self, busy_jiffies: u64, total_jiffies: u64) {
        let packed = (busy_jiffies & 0xFFFF_FFFF) << 32 | (total_jiffies & 0xFFFF_FFFF);
        let prev = self.prev.swap(packed, Ordering::Relaxed);
        if prev == 0 {
            return; // seeded
        }
        let (pb, pt) = (prev >> 32, prev & 0xFFFF_FFFF);
        let db = (busy_jiffies & 0xFFFF_FFFF).wrapping_sub(pb) & 0xFFFF_FFFF;
        let dt = (total_jiffies & 0xFFFF_FFFF).wrapping_sub(pt) & 0xFFFF_FFFF;
        if dt == 0 || db > dt {
            return; // wrap/degenerate: keep the last honest percent
        }
        self.busy_pct
            .store(((db * 100) / dt) as u32, Ordering::Relaxed);
    }
}

impl Default for HeadroomGauge {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The churn regime engages the MEASURED plateau window (magnitude
    /// is counted, membership is derived — the 2×EWMA sizing was
    /// falsified by its own A/B: micro-parks derived micro-windows and
    /// counted a wash).
    #[test]
    fn churn_regime_engages_the_measured_window() {
        assert_eq!(
            window(5_000, 10, 12, 32, 3),
            Duration::from_micros(SPIN_WINDOW_US),
            "micro-parks (fan-in churn): the full plateau window"
        );
        assert_eq!(
            window(150_000, 10, 12, 32, 3),
            Duration::from_micros(SPIN_WINDOW_US),
            "anywhere in the regime: the same measured magnitude"
        );
    }

    /// The idle regime disengages: parks longer than the rail mean the
    /// mount is idle — spinning is pure waste (the qd1 shape's guard:
    /// device-RTT-spaced parks sit far above the rail).
    #[test]
    fn idle_regime_disengages() {
        assert_eq!(window(500_000, 0, 12, 32, 3), Duration::ZERO);
        assert_eq!(window(5_000_000, 0, 12, 32, 3), Duration::ZERO);
    }

    /// Saturation disengages (the 2026-07-26 theft verdict): busy past
    /// the derived ceiling bleeds the window to 0 regardless of churn.
    #[test]
    fn saturation_disengages() {
        let ceiling = busy_ceiling_pct(12, 32);
        assert_eq!(ceiling, 62, "12 lanes / 32 cores: share ceils to 38 -> 62");
        assert_eq!(
            window(50_000, ceiling, 12, 32, 3),
            Duration::from_micros(SPIN_WINDOW_US),
            "at the ceiling: still engaged"
        );
        assert_eq!(
            window(50_000, ceiling + 1, 12, 32, 3),
            Duration::ZERO,
            "past the ceiling: bled to 0"
        );
    }

    /// The ceiling DERIVES from lanes and cores (the resource-derivation
    /// law): wider lane populations get less headroom, tiny boxes floor
    /// at 0 (never spin on a box the lanes already saturate).
    #[test]
    fn ceiling_derives_from_lanes_and_cores() {
        assert_eq!(
            busy_ceiling_pct(2, 32),
            93,
            "2 lanes on 32 cores (share ceils)"
        );
        assert_eq!(
            busy_ceiling_pct(24, 64),
            62,
            "3xcpus/8 at 64 cores (share ceils)"
        );
        assert_eq!(busy_ceiling_pct(2, 2), 0, "lanes == cores: never");
        assert_eq!(busy_ceiling_pct(64, 4), 0, "saturating, no underflow");
    }

    /// Never-parked lanes derive 0: the governor engages on OBSERVED
    /// churn only, never speculatively (a fresh mount does not spin).
    #[test]
    fn unseeded_lane_never_spins() {
        assert_eq!(window(0, 0, 12, 32, 3), Duration::ZERO);
    }

    /// The single-session guard (the qd1 falsification, 2026-08-14):
    /// one session on the lane = the sync-RTT regime — parks are
    /// productive waits, and the counted 2.5× qd1 loss is what an
    /// engaged spin costs there. Never spin, whatever the EWMA says.
    #[test]
    fn single_session_lane_never_spins() {
        assert_eq!(
            window(5_000, 0, 12, 32, 1),
            Duration::ZERO,
            "RTT-spaced parks are inside the rail — the structural              guard, not the temporal one, is what protects qd1"
        );
        assert_eq!(window(5_000, 0, 12, 32, 0), Duration::ZERO);
        assert_eq!(
            window(5_000, 0, 12, 32, 2),
            Duration::from_micros(SPIN_WINDOW_US),
            "two sessions interleave: fan-in, engaged"
        );
    }

    /// EWMA folding: seeds verbatim, then moves 1/8 per observation —
    /// an idle transition (one long park) starts pulling the estimate
    /// up immediately, and eight short parks pull it back into regime.
    #[test]
    fn ewma_folds_toward_observations() {
        let seeded = fold_park(0, 50_000);
        assert_eq!(seeded, 50_000, "first observation seeds verbatim");
        let after_long = fold_park(seeded, 5_000_000);
        assert!(
            after_long > seeded && after_long < 5_000_000,
            "moves toward the observation, not to it"
        );
        let mut e = 5_000_000u64;
        for _ in 0..32 {
            e = fold_park(e, 20_000);
        }
        assert!(
            e < SPIN_RAIL_US * 1_000,
            "sustained churn re-enters the regime within a bounded fold count (got {e})"
        );
    }

    /// The headroom gauge: cadence-gated single sampler, delta-computed
    /// percent, seed and degenerate readings never publish garbage.
    #[test]
    fn headroom_gauge_samples_and_publishes() {
        let g = HeadroomGauge::new();
        assert!(g.should_sample(1_000_000_000), "first claim wins");
        assert!(
            !g.should_sample(1_000_000_100),
            "within the cadence: refused"
        );
        assert!(
            g.should_sample(1_000_000_000 + HEADROOM_SAMPLE.as_nanos() as u64),
            "next window: claimable"
        );
        g.publish(100, 1000); // seed
        assert_eq!(g.busy_pct(), 0, "seed publishes nothing");
        g.publish(150, 1100); // +50 busy of +100 total
        assert_eq!(g.busy_pct(), 50);
        g.publish(150, 1100); // zero delta: keep the last honest value
        assert_eq!(g.busy_pct(), 50);
    }
}
