//! **Spin-before-park on the FUSE-over-io_uring queue worker** (e2e perf
//! audit R-4, reap-thread economy — `.benchmarks/2026-09-03-r4-reap-thread-economy.md`).
//!
//! # The term
//!
//! R-2 and R-3 each moved the kern 4 KiB random read's cross-thread hops
//! around the queue worker and converged on the same wall: every remaining
//! large term of the op is a WAKE latency — the lane's message waiting for
//! a parked worker (`msg_hop` 28 µs), the device CQE waiting for a parked
//! worker (`device_cq` − device ≈ 50 µs), the worker's oneshot waiting for
//! a parked lane (`wake_hop` 36 µs) — while the worker's own pass work is
//! 6 µs (`transport_reap_gap_ns.blind` mean). The worker parks 0.76×/op,
//! and 84 % of those parks last ≤ 32 µs: it sleeps for tens of µs at a
//! time and pays the scheduler's wake latency every time.
//!
//! # The mechanism
//!
//! At the pass bottom, where the worker would block in `submit_and_wait`,
//! it may first SPIN for a bounded, derived window (the shared
//! `spin_governor_core::worker_window_ns` law): while spinning it polls
//! `IORING_SQ_TASKRUN` (the ring's "task work landed" flag — under
//! `DEFER_TASKRUN` every completion from another context lands as local
//! task work and raises it; rings built with `IORING_SETUP_TASKRUN_FLAG`),
//! the CQ tail (plain rings post directly), the wake coalescer's armed
//! flag (a lane published a message / a lease dropped — the eventfd write
//! is in flight), and the shutdown word. An event caught inside the window
//! is surfaced by a NON-BLOCKING GETEVENTS enter — no sleep, no wake, no
//! scheduler round trip; a window that expires falls into the blocking
//! enter exactly as before.
//!
//! The window derives per worker from its own observed gaps (2 × EWMA,
//! the reaction clock), engages only while this worker's queues hold ops
//! in flight (a delivered-not-committed slot or a bridge pend — an idle
//! queue never spins), and is refused past the box's queueing knee
//! (`WORKER_BUSY_CEILING_PCT`, `/proc/stat` sampled at the shared 100 ms
//! cadence). `SQUEEZEFS_FUSE_IO_URING_SPIN_US` is the cap: `0` = never
//! spin (the A/B control), an explicit value = the operator's verbatim
//! cap (the box gauge does not refuse it), absent = derived.
//!
//! Engagement ledger (stats inode): `transport_spin_absorbed` (spins that
//! caught an event — each one a park + wake that never happened),
//! `transport_spin_expired` (spins that ran the window out and parked),
//! `transport_spin_ns` (the CPU the lever bought with — the cost column),
//! `transport_spin_refused_busy` (regime hits the box gauge refused),
//! `transport_spin_window_us` (the last derived window on any worker).
//! Closure: `absorbed + expired ≡` the spins run; `absorbed` is the count
//! of parks the lever deleted.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::raw::read_phase::ShardedCounter;
use crate::spin_governor_core::{
    fold_park, read_proc_stat_busy, worker_window_ns, HeadroomGauge, WORKER_BUSY_CEILING_PCT,
};

static SPIN_ABSORBED: ShardedCounter = ShardedCounter::new();
static SPIN_EXPIRED: ShardedCounter = ShardedCounter::new();
static SPIN_NS: ShardedCounter = ShardedCounter::new();
static SPIN_REFUSED_BUSY: ShardedCounter = ShardedCounter::new();
static SPIN_WINDOW_NS: AtomicU64 = AtomicU64::new(0);

/// The knob (`SQUEEZEFS_FUSE_IO_URING_SPIN_US`): `Some(cap_ns)` when set
/// (`Some(0)` = never spin), `None` = the derived cap. Read once per
/// process; the daemon's startup registry gate refused a malformed or
/// out-of-range value before any worker spawned, so an unparseable value
/// here keeps the default.
pub fn spin_cap_ns() -> Option<u64> {
    static CELL: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        spin_cap_ns_from(
            std::env::var("SQUEEZEFS_FUSE_IO_URING_SPIN_US")
                .ok()
                .as_deref(),
        )
    })
}

/// The pure form of [`spin_cap_ns`] (pinned by the unit tests): the knob
/// value in µs → cap in ns; absent/blank/unparseable → derived.
pub fn spin_cap_ns_from(raw: Option<&str>) -> Option<u64> {
    crate::env_knob_core::parse_int_in::<u64>("SQUEEZEFS_FUSE_IO_URING_SPIN_US", raw, 0, 100_000)
        .ok()
        .flatten()
        .map(|us| us * 1_000)
}

/// One worker's spin state: its gap EWMA and the resolved cap.
pub(crate) struct WorkerSpin {
    cap_ns: Option<u64>,
    gap_ewma_ns: u64,
}

impl WorkerSpin {
    pub(crate) fn new(cap_ns: Option<u64>) -> Self {
        Self {
            cap_ns,
            gap_ewma_ns: 0,
        }
    }

    /// Fold one observed gap (a park's wall time, or a spin-absorbed
    /// wait, or spin + the park that followed it) into the EWMA.
    #[inline]
    pub(crate) fn observe_gap(&mut self, ns: u64) {
        self.gap_ewma_ns = fold_park(self.gap_ewma_ns, ns);
    }

    /// The window (ns) for the park decision at hand; 0 = park now.
    /// Counts a regime hit the box gauge refused and publishes the
    /// derived window gauge.
    #[inline]
    pub(crate) fn window_ns(&self, in_flight: bool, busy_pct: u32) -> u64 {
        let w = worker_window_ns(self.cap_ns, self.gap_ewma_ns, in_flight, busy_pct);
        if w == 0
            && in_flight
            && self.cap_ns.is_none()
            && self.gap_ewma_ns > 0
            && self.gap_ewma_ns <= crate::spin_governor_core::SPIN_RAIL_US * 1_000
            && busy_pct > WORKER_BUSY_CEILING_PCT
        {
            SPIN_REFUSED_BUSY.add(1);
        }
        if SPIN_WINDOW_NS.load(Ordering::Relaxed) != w {
            SPIN_WINDOW_NS.store(w, Ordering::Relaxed);
        }
        w
    }

    /// The gap EWMA (tests).
    #[cfg(test)]
    pub(crate) fn gap_ewma_ns(&self) -> u64 {
        self.gap_ewma_ns
    }
}

/// The pool's shared box-headroom gauge: any worker claims the 100 ms
/// sampling slot on its park decision and publishes a `/proc/stat`
/// delta; every worker reads the percent.
pub(crate) struct SpinHeadroom {
    gauge: HeadroomGauge,
}

impl SpinHeadroom {
    pub(crate) const fn new() -> Self {
        Self {
            gauge: HeadroomGauge::new(),
        }
    }

    /// The current box busy percent, sampling on the shared cadence when
    /// this call wins the slot (`now_ns` = the caller's transport-epoch
    /// stamp — no clock read here).
    #[inline]
    pub(crate) fn busy_pct(&self, now_ns: u64) -> u32 {
        if self.gauge.should_sample(now_ns) {
            if let Some((busy, total)) = read_proc_stat_busy() {
                self.gauge.publish(busy, total);
            }
        }
        self.gauge.busy_pct()
    }
}

/// Account one spin that caught an event inside its window after
/// `spent_ns` of spinning.
#[inline]
pub(crate) fn note_absorbed(spent_ns: u64) {
    SPIN_ABSORBED.add(1);
    SPIN_NS.add(spent_ns);
}

/// Account one spin that ran its window out (the blocking park follows).
#[inline]
pub(crate) fn note_expired(spent_ns: u64) {
    SPIN_EXPIRED.add(1);
    SPIN_NS.add(spent_ns);
}

/// Stats-inode export: `(absorbed, expired, spin_ns, refused_busy,
/// window_us)`.
pub fn transport_spin_stats() -> (u64, u64, u64, u64, u64) {
    (
        SPIN_ABSORBED.load(),
        SPIN_EXPIRED.load(),
        SPIN_NS.load(),
        SPIN_REFUSED_BUSY.load(),
        SPIN_WINDOW_NS.load(Ordering::Relaxed) / 1_000,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The knob's three postures: absent = derived, `0` = off, a value =
    /// the cap in ns; out-of-range/malformed keep the derived posture
    /// (the startup gate is what refuses them).
    #[test]
    fn knob_resolves_absent_off_and_explicit() {
        assert_eq!(spin_cap_ns_from(None), None);
        assert_eq!(spin_cap_ns_from(Some("")), None);
        assert_eq!(spin_cap_ns_from(Some("0")), Some(0));
        assert_eq!(spin_cap_ns_from(Some("50")), Some(50_000));
        assert_eq!(spin_cap_ns_from(Some("bogus")), None);
        assert_eq!(spin_cap_ns_from(Some("999999")), None);
    }

    /// A never-parked worker never spins; observed short gaps open a
    /// window of twice their EWMA while ops are in flight; an idle queue
    /// closes it whatever the EWMA says; `Some(0)` closes it always.
    #[test]
    fn worker_spin_engages_only_on_observed_gaps_with_ops_in_flight() {
        let mut w = WorkerSpin::new(None);
        assert_eq!(w.window_ns(true, 0), 0, "unseeded");
        w.observe_gap(12_000);
        assert_eq!(w.gap_ewma_ns(), 12_000, "seeded verbatim");
        assert_eq!(w.window_ns(true, 0), 24_000);
        assert_eq!(w.window_ns(false, 0), 0, "idle queue");
        assert_eq!(w.window_ns(true, 99), 0, "past the knee");
        let off = {
            let mut o = WorkerSpin::new(Some(0));
            o.observe_gap(12_000);
            o
        };
        assert_eq!(off.window_ns(true, 0), 0);
    }

    /// Long parks (an idle mount) pull the EWMA past the rail and close
    /// the window; sustained short gaps re-open it — the governor
    /// follows the queue's regime, never a constant.
    #[test]
    fn idle_then_busy_regime_transition() {
        let mut w = WorkerSpin::new(None);
        w.observe_gap(10_000);
        for _ in 0..16 {
            w.observe_gap(5_000_000);
        }
        assert_eq!(w.window_ns(true, 0), 0, "idle-spaced gaps close the window");
        for _ in 0..64 {
            w.observe_gap(15_000);
        }
        let win = w.window_ns(true, 0);
        assert!(
            (20_000..=40_000).contains(&win),
            "sustained 15 µs gaps re-open ≈ 2 × EWMA (got {win})"
        );
    }

    /// The ledger closes: absorbed + expired count the spins run, and
    /// `spin_ns` is their summed wall.
    #[test]
    fn ledger_closes_over_absorbed_and_expired() {
        let (a0, e0, n0, _, _) = transport_spin_stats();
        note_absorbed(1_500);
        note_expired(30_000);
        note_absorbed(500);
        let (a1, e1, n1, _, _) = transport_spin_stats();
        assert_eq!((a1 - a0, e1 - e0, n1 - n0), (2, 1, 32_000));
    }
}
