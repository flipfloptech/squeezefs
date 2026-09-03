//! Write-pipeline depth governor — the 2026-07-27 write-pipeline-depth
//! campaign (`.benchmarks/2026-07-27-write-pipeline-depth.md`).
//!
//! ## The convicted mechanism
//!
//! Sequential large writes walled at a fraction of the substrate because
//! the complete-block write-through (`upload_full_block`: crypto →
//! allocate → DMA → block-map merge) was **awaited inline in the WRITE
//! handler**: every writer thread ran its per-block upload as a closed
//! loop, so aggregate block throughput = threads ÷ per-block pipeline
//! latency, and the data devices sat at aqu-sz < 2 (field capture: 933 w/s
//! × 4 MiB × ~1.9 ms w_await per namespace = the observed 7.5–8.3 GB/s
//! wall on a 2×200GbE nvme-tcp cluster; reproduced on the nvmet-tcp devsub
//! rig at 0.33× of the raw ceiling). Concurrency above the funnel just
//! queued: 64→128 threads moved throughput ~7 % while latency doubled.
//!
//! ## The design law (USER DIRECTIVE — no fixed pipeline depth anywhere)
//!
//! The write pipeline must sustain **at least the bandwidth-delay product
//! in flight per backend**, with the depth **derived at runtime** from the
//! measured per-backend service time and achievable bandwidth — never a
//! constant. A constant that saturates a 2×200GbE fabric today is wrong at
//! 800GbE by 10×. The depth target is bounded ONLY by:
//!
//! 1. **The R5 memory budget** — in-flight write-block bytes are the
//!    gauged `write_pipeline_inflight` component (a non-sheddable drain
//!    like `transport_payload_buffers`: Red clamps the admission target to
//!    the un-headroomed measured-BDP sum — floored by the cold posture —
//!    so the queueing bytes shed while the measured drain rate sustains
//!    and the gauge converges by completion — honest backpressure, never
//!    OOM and never a starved drain; finding 39), and the target is
//!    hard-capped at budget ÷ [`BUDGET_CAP_DIVISOR`].
//! 2. **Honest backpressure to the writer** — admission is awaited in the
//!    WRITE handler before the completing write ACKs, so a full pipe
//!    stalls the writer exactly like every other admission gate.
//!
//! ## The governor
//!
//! Per backend lane, two decaying estimates:
//!
//! * `lat_floor_ns` — the **uncongested service time**: a running minimum
//!   of observed upload durations that decays UPWARD by ⅛ per window
//!   (bounded by the latency EWMA), so queueing inflation never feeds the
//!   BDP arithmetic (the runaway-growth guard: at saturation, inflight ≡
//!   bw × latency by Little's law, so a congested-latency BDP would chase
//!   its own tail).
//! * `bw_peak_bps` — the **achieved bandwidth peak**: the windowed
//!   completion rate, decaying by ⅛ per window so the target adapts down
//!   when the device genuinely slows and re-learns fast when it does not.
//!
//! `lane target = max(floor blocks, bw_peak × lat_floor × HEADROOM)` and
//! the aggregate target is the sum over lanes. From cold the per-lane
//! floor ([`FLOOR_BLOCKS_PER_LANE`]) keeps the pipe fed; measured
//! bandwidth then grows the target exponentially until the device
//! plateaus (bw_peak stops growing) — "depth grows while the device
//! drains faster than arrival", bounded as above.
//!
//! ## The probe-up governor (2026-07-29 campaign)
//!
//! The pure-BDP target is a **self-fulfilling equilibrium**: it targets
//! measured bandwidth × measured service time, which SUSTAINS the current
//! operating point instead of discovering headroom (field conviction,
//! 4-node 2×200GbE cluster: default 11.6 GB/s at aqu-sz ≈ its own BDP;
//! forced depth 64 = 13.7 GB/s, +18 %, against a 16.6 GB/s raw ceiling
//! with client CPU at ~28 % — `.benchmarks/2026-07-29-probe-up-governor
//! .md`). A BBR-flavored probe layer ([`ProbeCore`], loom-included core)
//! multiplies the governed sum: on **saturated** epochs (admission parked
//! or the pipe at target) with **headroom** (below the R5 cap, not Red)
//! it raises the target by ¼ and measures the delivery-rate response —
//! responsive ⇒ ADOPT and compound; dead gain ⇒ RETREAT to the pre-probe
//! multiplier and cool down (the dead-gain latency tax is duty-cycle
//! bounded); adopted depth that stops delivering steps back down; and
//! **unsaturated epochs bleed the multiplier back to 1.0** — the latency
//! guard: low-offered-load workloads never inherit streaming queue depth
//! (qd1 RTT stays flat). Still no constants anywhere: the multiplier is
//! dimensionless, the multiplicand is the measured BDP, and the R5
//! budget cap + Red clamp stay senior to every probe. Gauges:
//! `write_pipeline_depth_probe_{ups,backoffs}` (engagement/retreat) and
//! `write_pipeline_depth_target_base` (the un-probed BDP target —
//! current-vs-base shows the probe's contribution live).
//!
//! ## A/B lever (measurement only — never an operational escape)
//!
//! `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS`: unset = the adaptive governor
//! (the shipped default); `0` = synchronous inline write-through (the
//! pre-campaign posture — the A/B baseline lever, exactly the
//! `SQUEEZEFS_PATCH_MAX_BYTES=0` pattern); `N ≥ 1` = pin the aggregate
//! depth target to N blocks verbatim (bracket runs). Runtime-settable via
//! [`set_depth_override`] for tests/acceptance.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub use crate::write_pipeline_core::{
    ProbeCore, PROBE_COOLDOWN_EPOCHS, PROBE_EPOCH_MS, PROBE_MUL_MAX, PROBE_MUL_ONE,
};

/// Cold-start floor: blocks per known backend lane (and the aggregate
/// floor while no lane has reported). Small on purpose — the measured BDP
/// owns the depth; the floor only keeps the pipe fed while the estimates
/// learn. Under Red it is the LOWER bound of the derived drain clamp,
/// never the clamp itself (finding 39: a fixed aggregate clamp starved a
/// 10-lane fabric to ⅛ of its measured drain).
pub const FLOOR_BLOCKS_PER_LANE: u64 = 8;

/// BDP multiplier: absorbs service-time variance and the non-device legs
/// of the upload pipeline so the DEVICE leg stays at ≥ 1×BDP occupancy.
pub const HEADROOM: u64 = 3;

/// The R5 hard bound: the aggregate depth target never exceeds
/// budget ÷ this (the `transport_payload_buffers` cap shape).
pub const BUDGET_CAP_DIVISOR: u64 = 4;

/// Bandwidth-measurement window (ms) — two orders above the service-time
/// scale so windowed rates are meaningful, small enough to re-learn fast.
pub const WINDOW_MS: u64 = 250;

/// Admission-park liveness tick: the backstop for target changes that
/// carry NO completion (R5 Red clearing, governor/probe growth) — never
/// the wake path itself (PERF-13: completions are captured by the
/// enrolled `Notified`, and a tick that resumes a park while completions
/// flow counts in `write_pipeline_admission_tick_wakes`).
const ADMIT_TICK: Duration = Duration::from_millis(5);

/// PERF-13 test seam — microseconds to stall between the admission
/// re-check and the park (0 = off, the production value). Set only by
/// `tests/write_pipeline_tests.rs` to place a completion deterministically
/// inside the window the registration order closes.
static TEST_PREPARK_STALL_US: AtomicU64 = AtomicU64::new(0);

/// Wedge-census mirrors (zc-bridge-cqe-wedge, 2026-08-07): the
/// process-global twins of the per-instance pipeline gauges, readable
/// from the op watchdog with no instance in hand. Permit-count
/// semantics: minted at admission, returned at `PipelinePermit::drop` —
/// the loom-modeled `AdmissionCore` stays untouched.
pub static PIPELINE_INFLIGHT_PERMITS: AtomicU64 = AtomicU64::new(0);
/// Process-global mirror of `admission_waits` (writers that parked at
/// least once behind the depth target).
pub static PIPELINE_ADMISSION_WAITS: AtomicU64 = AtomicU64::new(0);

/// Set the `TEST_PREPARK_STALL_US` seam (tests only).
pub fn set_test_prepark_stall_us(us: u64) {
    TEST_PREPARK_STALL_US.store(us, Ordering::Relaxed);
}

/// Coarse monotonic milliseconds since process start (window clock).
fn coarse_ms() -> u64 {
    static START: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);
    START.elapsed().as_millis() as u64
}

/// `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS` cell: `-1` = adaptive (unset),
/// `0` = sync-inline, `n > 0` = pinned blocks. Env read once (memoized);
/// runtime-settable via [`set_depth_override`] (the A/B lever).
fn depth_override_cell() -> &'static AtomicI64 {
    static CELL: std::sync::OnceLock<AtomicI64> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let v = std::env::var("SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS")
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|v| *v >= 0)
            .unwrap_or(-1);
        AtomicI64::new(v)
    })
}

/// Current depth override: `None` = adaptive governor, `Some(0)` =
/// synchronous inline write-through, `Some(n)` = pinned n-block target.
pub fn depth_override() -> Option<u64> {
    let v = depth_override_cell().load(Ordering::Relaxed);
    (v >= 0).then_some(v as u64)
}

/// Set the depth override (tests / A-B acceptance runs — the
/// `set_patch_max_bytes` pattern). `None` restores the adaptive governor.
pub fn set_depth_override(v: Option<u64>) {
    depth_override_cell().store(v.map(|n| n as i64).unwrap_or(-1), Ordering::Relaxed);
}

/// Whether the A/B lever pins the pre-campaign synchronous inline
/// write-through (`SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS=0`).
pub fn sync_inline() -> bool {
    depth_override() == Some(0)
}

/// Pure raw-BDP arithmetic: `bw_peak × lat_floor` (u128 intermediate — a
/// 100 GB/s × 1 s product must not wrap). The measured minimum in-flight
/// that sustains the measured drain rate — the Red clamp's per-lane term.
pub fn lane_bdp_bytes(bw_peak_bps: u64, lat_floor_ns: u64) -> u64 {
    ((bw_peak_bps as u128) * (lat_floor_ns as u128) / 1_000_000_000u128) as u64
}

/// Pure lane-target arithmetic: `max(floor, BDP × HEADROOM)`.
pub fn lane_target_bytes(bw_peak_bps: u64, lat_floor_ns: u64, block_size: u64) -> u64 {
    let floor = FLOOR_BLOCKS_PER_LANE.saturating_mul(block_size);
    floor.max(lane_bdp_bytes(bw_peak_bps, lat_floor_ns).saturating_mul(HEADROOM))
}

/// Pure window roll for the bandwidth peak: the windowed rate, or the
/// previous peak decayed by ⅛ — whichever is larger (fast up, slow down).
pub fn rolled_bw_peak(prev_peak_bps: u64, win_bytes: u64, elapsed_ms: u64) -> u64 {
    let bw = win_bytes.saturating_mul(1000) / elapsed_ms.max(1);
    bw.max(prev_peak_bps - prev_peak_bps / 8)
}

/// Pure window roll for the latency floor: decay UPWARD by ⅛ (re-learn a
/// genuinely slower device) but never past the latency EWMA — queueing
/// inflation must not feed the BDP (the runaway guard; see module docs).
pub fn rolled_lat_floor(prev_floor_ns: u64, ewma_lat_ns: u64) -> u64 {
    (prev_floor_ns + prev_floor_ns / 8)
        .max(1)
        .min(ewma_lat_ns.max(1))
}

/// Terminal disposition of one pipeline upload attempt — the counting
/// contract of the detached upload task (pure; pinned by
/// `tests/write_pipeline_tests.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineDisposition {
    /// Durably published (or a no-op against already-drained custody).
    Done,
    /// Fenced mid-flight: custody dropped loudly (the remount law,
    /// FIND-M11-A) — counted in `write_pipeline_fence_drops`.
    FenceDrop,
    /// Transient failure: custody stays parked/staged (never-lossy) — the
    /// fsync/drain machinery owns the retry.
    StayParked,
}

/// Map an upload outcome to its [`PipelineDisposition`].
pub fn pipeline_disposition(res: &Result<(), crate::error::SqueezefsError>) -> PipelineDisposition {
    match res {
        Ok(()) => PipelineDisposition::Done,
        // The 2026-08-06 fsync-vs-writeback tail-loss fix: a process-local
        // fencing rotation on live parked custody CONVERGES inside
        // `write_through_complete_block` (`fencing_retry_token`), so an
        // escaping `FencingTokenExpired` means the non-progress arm kept
        // the custody PARKED — counting it as a custody drop would
        // misreport the never-lossy posture. The genuine fence is the
        // `WriterGuardFenced` class below.
        Err(crate::error::SqueezefsError::FencingTokenExpired { .. }) => {
            PipelineDisposition::StayParked
        }
        // RES-6: the D0 latch's face. A fenced holder that retried
        // forever would park custody until the R5 budget went Red and
        // stayed there; the W5 law is publish nothing, free nothing.
        Err(crate::error::SqueezefsError::WriterGuardFenced) => PipelineDisposition::FenceDrop,
        Err(_) => PipelineDisposition::StayParked,
    }
}

/// Per-backend service estimates (see module docs). All fields are
/// approximate, latch-free gauges — a torn window roll pairs two rolls
/// microseconds apart and self-heals within one window.
struct Lane {
    ewma_lat_ns: AtomicU64,
    lat_floor_ns: AtomicU64,
    bw_peak_bps: AtomicU64,
    win_start_ms: AtomicU64,
    win_bytes: AtomicU64,
}

impl Lane {
    fn new() -> Self {
        Self {
            ewma_lat_ns: AtomicU64::new(0),
            lat_floor_ns: AtomicU64::new(0),
            bw_peak_bps: AtomicU64::new(0),
            win_start_ms: AtomicU64::new(0),
            win_bytes: AtomicU64::new(0),
        }
    }

    fn record(&self, bytes: u64, dur_ns: u64, now_ms: u64) {
        let dur_ns = dur_ns.max(1);
        let prev = self.ewma_lat_ns.load(Ordering::Relaxed);
        let ewma = if prev == 0 {
            dur_ns
        } else {
            prev - prev / 8 + dur_ns / 8
        };
        self.ewma_lat_ns.store(ewma, Ordering::Relaxed);
        let floor = self.lat_floor_ns.load(Ordering::Relaxed);
        if floor == 0 || dur_ns < floor {
            self.lat_floor_ns.store(dur_ns, Ordering::Relaxed);
        }
        self.win_bytes.fetch_add(bytes, Ordering::Relaxed);

        let ws = self.win_start_ms.load(Ordering::Relaxed);
        if ws == 0 {
            let _ = self.win_start_ms.compare_exchange(
                0,
                now_ms.max(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            return;
        }
        let elapsed = now_ms.saturating_sub(ws);
        if elapsed >= WINDOW_MS
            && self
                .win_start_ms
                .compare_exchange(ws, now_ms, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            // This thread rolls the window.
            let wb = self.win_bytes.swap(0, Ordering::Relaxed);
            let peak = rolled_bw_peak(self.bw_peak_bps.load(Ordering::Relaxed), wb, elapsed);
            self.bw_peak_bps.store(peak, Ordering::Relaxed);
            let f = rolled_lat_floor(
                self.lat_floor_ns.load(Ordering::Relaxed),
                self.ewma_lat_ns.load(Ordering::Relaxed),
            );
            self.lat_floor_ns.store(f, Ordering::Relaxed);
        }
    }
}

/// The per-mount write-pipeline authority: admission gate + depth
/// governor + in-flight gauges. Shared `Arc` across handler clones.
/// The lock-free admission accounting lives in
/// `crate::write_pipeline_core::AdmissionCore` — loom-modeled
/// (`loom-models/`, `write_pipeline_admission_*`): bounded admission,
/// single oversized empty-pipe bypass, exact settle-to-zero.
pub struct WritePipeline {
    core: crate::write_pipeline_core::AdmissionCore,
    admission_waits: AtomicU64,
    /// Parks resumed by the liveness TICK instead of a completion wake
    /// (`write_pipeline_admission_tick_wakes`) — the PERF-13 tripwire.
    ///
    /// After the registration-order fix the tick exists only for target
    /// changes that carry no completion (R5 Red clearing, governor/probe
    /// growth) and for genuinely stalled devices, so growth of this
    /// counter while completions are flowing means a wake was LOST — the
    /// exact defect that pinned p99 `admit_wait` at the 5 ms tick.
    admission_tick_wakes: AtomicU64,
    lanes: scc::HashMap<String, Arc<Lane>>,
    /// Probe-up layer over the BDP target (2026-07-29 campaign — see
    /// module docs §"The probe-up governor").
    probe: ProbeCore,
    /// `admission_waits` snapshot at the last probe-epoch roll: waits
    /// growth within an epoch is the saturation signal.
    probe_waits_snap: AtomicU64,
    completions: squeezefs_ipc::sqz_notify::Notify,
    /// `None` = live `MEM_BUDGET ÷ BUDGET_CAP_DIVISOR`; `Some` = explicit
    /// (tests).
    budget_cap_bytes: Option<u64>,
    /// Red-band probe (prod: the R5 authority level; injectable for tests).
    red: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl WritePipeline {
    /// Production construction: R5-budget cap, R5 Red probe.
    pub fn for_mount() -> Arc<Self> {
        Self::with_caps(
            Arc::new(|| crate::mem_budget::level() == crate::mem_budget::Level::Red),
            None,
        )
    }

    /// Test/bench construction with an injectable Red probe and an
    /// explicit budget cap (`None` = live budget ÷ [`BUDGET_CAP_DIVISOR`]).
    pub fn with_caps(
        red: Arc<dyn Fn() -> bool + Send + Sync>,
        budget_cap_bytes: Option<u64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            core: crate::write_pipeline_core::AdmissionCore::new(),
            admission_waits: AtomicU64::new(0),
            admission_tick_wakes: AtomicU64::new(0),
            lanes: scc::HashMap::new(),
            probe: ProbeCore::new(),
            probe_waits_snap: AtomicU64::new(0),
            completions: squeezefs_ipc::sqz_notify::Notify::new(),
            budget_cap_bytes,
            red,
        })
    }

    /// Record one completed upload for `lane` (backend id): feeds the
    /// governor's service-time / bandwidth estimates. `bytes` is a whole
    /// block on the accumulation path, so it doubles as the target's
    /// block scale; sub-block vehicles use [`Self::record_completion_sized`].
    pub fn record_completion(&self, lane: &str, bytes: u64, dur: Duration) {
        self.record_completion_at(lane, bytes, dur.as_nanos() as u64, coarse_ms());
    }

    /// [`Self::record_completion`] for a SUB-BLOCK completion (W-3: the
    /// device-overlay segment store) — `block_size` scales the
    /// saturation/headroom probe inputs so a 1 MiB segment reads the
    /// same target a 4 MiB block does.
    pub fn record_completion_sized(&self, lane: &str, bytes: u64, block_size: u64, dur: Duration) {
        self.record_at(lane, bytes, block_size, dur.as_nanos() as u64, coarse_ms());
    }

    /// [`Self::record_completion`] with an explicit window clock — the
    /// governor-math test hook (window rolls are wall-clock-driven in
    /// production).
    pub fn record_completion_at(&self, lane: &str, bytes: u64, dur_ns: u64, now_ms: u64) {
        self.record_at(lane, bytes, bytes, dur_ns, now_ms);
    }

    fn record_at(&self, lane: &str, bytes: u64, block_size: u64, dur_ns: u64, now_ms: u64) {
        let l = match self.lanes.read_sync(lane, |_, l| l.clone()) {
            Some(l) => l,
            None => {
                let fresh = Arc::new(Lane::new());
                match self.lanes.entry_sync(lane.to_string()) {
                    scc::hash_map::Entry::Occupied(occ) => occ.get().clone(),
                    scc::hash_map::Entry::Vacant(vac) => {
                        vac.insert_entry(fresh.clone());
                        fresh
                    }
                }
            }
        };
        l.record(bytes, dur_ns, now_ms);

        // Probe-up layer (dormant under a pinned A/B override — the
        // lever stays verbatim): count delivery, and roll the probe epoch
        // with this epoch's saturation + headroom observations.
        if depth_override().is_none() {
            self.probe.on_bytes(bytes);
            let waits = self.admission_waits.load(Ordering::Relaxed);
            let bs = block_size.max(1);
            let target = self.depth_target_bytes(bs);
            // Saturated = writers parked this epoch (waits grew), or the
            // pipe sits at/above target right now. No saturation ⇒ extra
            // depth serves nothing — the latency guard's input.
            let saturated = waits > self.probe_waits_snap.load(Ordering::Relaxed)
                || self.core.inflight_bytes() >= target;
            // Headroom = below the R5 cap and not Red: probing where the
            // budget cannot follow is pure latency tax.
            let headroom = !(self.red)() && target < self.cap_bytes(bs);
            if self.probe.roll(now_ms, saturated, headroom) {
                self.probe_waits_snap.store(waits, Ordering::Relaxed);
            }
        }
    }

    /// The aggregate depth target in bytes (see module docs): pinned
    /// override verbatim, else Σ per-lane BDP targets (floored) — Red
    /// clamps to the measured drain bound (drain posture: honest writer
    /// backpressure while in-flight custody converges by completion), and
    /// the R5 budget cap bounds everything (never below one block:
    /// admission must always be able to make progress).
    pub fn depth_target_bytes(&self, block_size: u64) -> u64 {
        let bs = block_size.max(1);
        let floor = FLOOR_BLOCKS_PER_LANE.saturating_mul(bs);
        let raw = match depth_override() {
            Some(n) if n > 0 => n.saturating_mul(bs),
            _ => {
                // The probe multiplier scales the whole governed sum
                // (BDP-learned lanes AND cold-lane floors — cold streams
                // discover headroom the same way; u128: 32× of an
                // 800GbE-class sum must not wrap).
                let sum = self.governed_sum_bytes(bs);
                let scaled = ((sum as u128 * self.probe.mul_q6() as u128) / PROBE_MUL_ONE as u128)
                    .min(u64::MAX as u128) as u64;
                scaled.max(floor)
            }
        };
        // Finding 39 (EXA field capture, 2026-08-31): Red used to clamp to
        // the FIXED aggregate floor — 32 MiB in flight across a 10-lane
        // ~12 ms fabric is ≈ 2.4 GB/s by Little's law, an 8× collapse that
        // starved the very drain that converges the gauge (multi-second
        // admission tails, parked_gate_waits +19,375). The clamp now
        // DERIVES from the measured drain: the un-headroomed raw-BDP sum —
        // shedding exactly the headroom/probe queueing bytes (≥ ⅔ of the
        // component under the ×3 HEADROOM) while sustaining the measured
        // completion rate. The fixed floor survives only as the cold
        // posture (nothing learned) and the progress guarantee.
        let raw = if (self.red)() {
            raw.min(self.red_drain_sum_bytes().max(floor))
        } else {
            raw
        };
        raw.min(self.cap_bytes(bs))
    }

    /// The pure-BDP governed target (probe multiplier at 1.0) — the
    /// `write_pipeline_depth_target_base` gauge: current-vs-base is the
    /// probe-engagement instrument.
    pub fn depth_target_base_bytes(&self, block_size: u64) -> u64 {
        let bs = block_size.max(1);
        let floor = FLOOR_BLOCKS_PER_LANE.saturating_mul(bs);
        self.governed_sum_bytes(bs)
            .max(floor)
            .min(self.cap_bytes(bs))
    }

    /// Σ per-lane BDP targets (each floored) — the probe multiplicand.
    fn governed_sum_bytes(&self, bs: u64) -> u64 {
        let mut sum = 0u64;
        self.lanes.iter_sync(|_, l| {
            sum = sum.saturating_add(lane_target_bytes(
                l.bw_peak_bps.load(Ordering::Relaxed),
                l.lat_floor_ns.load(Ordering::Relaxed),
                bs,
            ));
            true
        });
        sum
    }

    /// Σ per-lane RAW BDP (no HEADROOM, no probe multiplier) — the Red
    /// clamp bound: the measured minimum in-flight that still sustains the
    /// measured drain rate (finding 39; see `depth_target_bytes`). Cold
    /// lanes contribute nothing — the aggregate floor covers them.
    fn red_drain_sum_bytes(&self) -> u64 {
        let mut sum = 0u64;
        self.lanes.iter_sync(|_, l| {
            sum = sum.saturating_add(lane_bdp_bytes(
                l.bw_peak_bps.load(Ordering::Relaxed),
                l.lat_floor_ns.load(Ordering::Relaxed),
            ));
            true
        });
        sum
    }

    /// The R5 hard bound (never below one block: admission must always be
    /// able to make progress).
    fn cap_bytes(&self, bs: u64) -> u64 {
        self.budget_cap_bytes
            .unwrap_or_else(|| crate::mem_budget::MEM_BUDGET.budget_bytes() / BUDGET_CAP_DIVISOR)
            .max(bs)
    }

    /// One admission attempt against the CURRENT depth target. `None` =
    /// the pipe is at target (park). A CAS race means "the counters moved
    /// under us", never "no room", so it retries in place.
    fn try_admit_step(self: &Arc<Self>, bytes: u64, block_size: u64) -> Option<PipelinePermit> {
        loop {
            let target = self.depth_target_bytes(block_size.max(1));
            match self.core.try_admit_once(bytes, target) {
                crate::write_pipeline_core::AdmitAttempt::Admitted => {
                    // Wedge-census mirror (2026-08-07): the process-global
                    // twin of `core.inflight_blocks()` — permit-count
                    // semantics, maintained at the permit's mint/drop so
                    // the loom-modeled core stays untouched.
                    PIPELINE_INFLIGHT_PERMITS.fetch_add(1, Ordering::Relaxed);
                    return Some(PipelinePermit {
                        pipe: self.clone(),
                        bytes,
                        // DLM S7: custody is established HERE (admission
                        // is the honest-backpressure gate the WRITE
                        // handler awaits before the ACK), and the DMA
                        // happens later on a detached task — so the
                        // permit is the epoch CARRIER, and the device
                        // gate refuses it if custody moved in between.
                        auth: crate::data_custody::current_epoch(),
                    });
                }
                crate::write_pipeline_core::AdmitAttempt::Raced => continue,
                crate::write_pipeline_core::AdmitAttempt::Full => return None,
            }
        }
    }

    /// Admit `block_bytes` of upload custody into the pipeline — **the
    /// honest-backpressure gate**, awaited by the WRITE handler before the
    /// completing write ACKs. Parks while the pipe is at target (woken by
    /// completions; re-polls on a short tick so Red/target changes are
    /// observed). Progress guarantee: an empty pipe always admits.
    pub async fn admit(self: &Arc<Self>, block_bytes: u64) -> PipelinePermit {
        self.admit_segment(block_bytes, block_bytes).await
    }

    /// [`Self::admit`] for a SUB-BLOCK segment (W-3, e2e perf audit write
    /// board #3 — the device-overlay store): `bytes` of in-flight DMA
    /// custody counted against the target at the volume's `block_size`
    /// scale, so the overlay's segments and the accumulation path's whole
    /// blocks share ONE gauge, ONE target and ONE backpressure gate. The
    /// caller's ACK still detaches from the CQE; only ADMISSION waits.
    pub async fn admit_segment(self: &Arc<Self>, bytes: u64, block_size: u64) -> PipelinePermit {
        let mut waited = false;
        loop {
            // Fast attempt: an admitting pipe pays NO wait-list traffic
            // (the registration below is a `Notify` wait-list push/pop
            // pair — free on the park path, pure cost on the common one).
            if let Some(permit) = self.try_admit_step(bytes, block_size) {
                return permit;
            }
            // PERF-13 — **register the wake BEFORE the admission
            // re-check.** `notify_waiters` stores no permit, so a
            // completion landing between a Full re-check and the park's
            // first poll used to be lost outright, leaving the 5 ms
            // liveness tick as the ONLY thing that resumed the writer
            // (p99 `admit_wait` pinned at the tick under a saturated
            // pipe). `Notified::enable` enrolls this waiter in the wait
            // list synchronously — before the re-check reads the
            // counters — so any completion from here on either admits us
            // on the re-check or is captured by the enrolled waiter.
            // The tick stays as the liveness backstop for target changes
            // that are NOT paired with a completion (R5 Red clearing,
            // governor/probe growth), which is all it was ever needed
            // for.
            let mut park = self.completions.notified_raw();
            park.enable();

            if let Some(permit) = self.try_admit_step(bytes, block_size) {
                return permit;
            }
            if !waited {
                waited = true;
                self.admission_waits.fetch_add(1, Ordering::Relaxed);
                // Wedge-census mirror (2026-08-07).
                PIPELINE_ADMISSION_WAITS.fetch_add(1, Ordering::Relaxed);
            }
            // PERF-13 test seam: one relaxed load per park, zero cost when
            // unset. Lets a test place a completion exactly inside the
            // window between the re-check and the park — the window the
            // registration order above closes. Never set in production.
            let stall_us = TEST_PREPARK_STALL_US.load(Ordering::Relaxed);
            if stall_us > 0 {
                squeezefs_ipc::sqz_time::sleep(Duration::from_micros(stall_us)).await;
            }
            match squeezefs_ipc::sqz_future::race2(park, squeezefs_ipc::sqz_time::sleep(ADMIT_TICK))
                .await
            {
                squeezefs_ipc::sqz_future::Either::Left(()) => {}
                squeezefs_ipc::sqz_future::Either::Right(()) => {
                    self.admission_tick_wakes.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Wait until every admitted upload has completed (unmount teardown /
    /// tests). `false` = the deadline elapsed with custody still in
    /// flight.
    pub async fn quiesce(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // Same PERF-13 registration order as `admit`: enroll first,
            // then read the gauge, so a completion cannot slip between.
            let mut park = self.completions.notified_raw();
            park.enable();
            if self.core.inflight_blocks() == 0 {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            let _ =
                squeezefs_ipc::sqz_future::race2(park, squeezefs_ipc::sqz_time::sleep(ADMIT_TICK))
                    .await;
        }
    }

    /// In-flight admitted upload custody, bytes (the
    /// `write_pipeline_inflight_bytes` gauge and the R5
    /// `write_pipeline_inflight` component source).
    pub fn inflight_bytes(&self) -> u64 {
        self.core.inflight_bytes()
    }

    /// In-flight admitted uploads, blocks (`write_pipeline_inflight_blocks`).
    pub fn inflight_blocks(&self) -> u64 {
        self.core.inflight_blocks()
    }

    /// Admissions that parked at least once (`write_pipeline_admission_waits`
    /// — the writer-backpressure gauge).
    pub fn admission_waits(&self) -> u64 {
        self.admission_waits.load(Ordering::Relaxed)
    }

    /// Parks resumed by the liveness tick rather than a completion wake
    /// (`write_pipeline_admission_tick_wakes` — the PERF-13 tripwire; see
    /// the field docs on [`WritePipeline::admission_tick_wakes`]).
    pub fn admission_tick_wakes(&self) -> u64 {
        self.admission_tick_wakes.load(Ordering::Relaxed)
    }

    /// Probes launched (`write_pipeline_depth_probe_ups` — the probe
    /// engagement gauge; 0 on latency-sensitive/low-offered-load mounts).
    pub fn depth_probe_ups(&self) -> u64 {
        self.probe.probe_ups()
    }

    /// Probe retreats + hold step-downs
    /// (`write_pipeline_depth_probe_backoffs` — dead marginal gain).
    pub fn depth_probe_backoffs(&self) -> u64 {
        self.probe.probe_backoffs()
    }
}

/// RAII admission permit: dropping it returns the custody to the pipe and
/// wakes parked admissions/quiescers. Held by the detached upload task for
/// its whole lifetime (including the never-lossy fallback arms).
pub struct PipelinePermit {
    pipe: Arc<WritePipeline>,
    bytes: u64,
    /// DLM **S7**: the data-plane custody epoch this upload was authorized
    /// under (captured at admission — see [`PipelinePermit::auth`]).
    auth: crate::data_custody::CustodyEpoch,
}

impl PipelinePermit {
    /// The custody epoch this admission was authorized under. The upload
    /// task presents it at every DMA submission
    /// ([`crate::nvme_dev::NvmeBlockDev::write_block_authorized`]), so an
    /// upload that outlived its mount's custody is refused at the
    /// authorization point instead of landing on offsets the successor
    /// writer has already reallocated.
    pub fn auth(&self) -> crate::data_custody::CustodyEpoch {
        self.auth
    }
}

impl Drop for PipelinePermit {
    fn drop(&mut self) {
        PIPELINE_INFLIGHT_PERMITS.fetch_sub(1, Ordering::Relaxed);
        self.pipe.core.release(self.bytes);
        self.pipe.completions.notify_waiters();
    }
}
