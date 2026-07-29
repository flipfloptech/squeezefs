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
//!    its floor so the gauge converges by completion — honest
//!    backpressure, never OOM), and the target is hard-capped at
//!    budget ÷ [`BUDGET_CAP_DIVISOR`].
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

/// Cold-start / Red-clamp floor: blocks per known backend lane (and the
/// aggregate floor while no lane has reported). Small on purpose — the
/// measured BDP owns the depth; the floor only keeps the pipe fed while
/// the estimates learn (and is the honest-backpressure posture under Red).
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

/// Pure lane-target arithmetic: `max(floor, BDP × HEADROOM)` where
/// `BDP = bw_peak × lat_floor` (u128 intermediate — a 100 GB/s × 1 s
/// product must not wrap).
pub fn lane_target_bytes(bw_peak_bps: u64, lat_floor_ns: u64, block_size: u64) -> u64 {
    let floor = FLOOR_BLOCKS_PER_LANE.saturating_mul(block_size);
    let bdp = ((bw_peak_bps as u128) * (lat_floor_ns as u128) / 1_000_000_000u128) as u64;
    floor.max(bdp.saturating_mul(HEADROOM))
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
        Err(crate::error::SqueezefsError::FencingTokenExpired { .. }) => {
            PipelineDisposition::FenceDrop
        }
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
    lanes: scc::HashMap<String, Arc<Lane>>,
    /// Probe-up layer over the BDP target (2026-07-29 campaign — see
    /// module docs §"The probe-up governor").
    probe: ProbeCore,
    /// `admission_waits` snapshot at the last probe-epoch roll: waits
    /// growth within an epoch is the saturation signal.
    probe_waits_snap: AtomicU64,
    completions: tokio::sync::Notify,
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
            lanes: scc::HashMap::new(),
            probe: ProbeCore::new(),
            probe_waits_snap: AtomicU64::new(0),
            completions: tokio::sync::Notify::new(),
            budget_cap_bytes,
            red,
        })
    }

    /// Record one completed upload for `lane` (backend id): feeds the
    /// governor's service-time / bandwidth estimates.
    pub fn record_completion(&self, lane: &str, bytes: u64, dur: Duration) {
        self.record_completion_at(lane, bytes, dur.as_nanos() as u64, coarse_ms());
    }

    /// [`Self::record_completion`] with an explicit window clock — the
    /// governor-math test hook (window rolls are wall-clock-driven in
    /// production).
    pub fn record_completion_at(&self, lane: &str, bytes: u64, dur_ns: u64, now_ms: u64) {
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
            let bs = bytes.max(1);
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
    /// clamps to the floor (drain posture: honest writer backpressure
    /// while in-flight custody converges by completion), and the R5
    /// budget cap bounds everything (never below one block: admission
    /// must always be able to make progress).
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
        let raw = if (self.red)() { raw.min(floor) } else { raw };
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

    /// The R5 hard bound (never below one block: admission must always be
    /// able to make progress).
    fn cap_bytes(&self, bs: u64) -> u64 {
        self.budget_cap_bytes
            .unwrap_or_else(|| crate::mem_budget::MEM_BUDGET.budget_bytes() / BUDGET_CAP_DIVISOR)
            .max(bs)
    }

    /// Admit `block_bytes` of upload custody into the pipeline — **the
    /// honest-backpressure gate**, awaited by the WRITE handler before the
    /// completing write ACKs. Parks while the pipe is at target (woken by
    /// completions; re-polls on a short tick so Red/target changes are
    /// observed). Progress guarantee: an empty pipe always admits.
    pub async fn admit(self: &Arc<Self>, block_bytes: u64) -> PipelinePermit {
        let mut waited = false;
        loop {
            let target = self.depth_target_bytes(block_bytes.max(1));
            match self.core.try_admit_once(block_bytes, target) {
                crate::write_pipeline_core::AdmitAttempt::Admitted => {
                    return PipelinePermit {
                        pipe: self.clone(),
                        bytes: block_bytes,
                    };
                }
                crate::write_pipeline_core::AdmitAttempt::Raced => {
                    continue; // CAS raced a completion/admission — re-evaluate.
                }
                crate::write_pipeline_core::AdmitAttempt::Full => {}
            }
            if !waited {
                waited = true;
                self.admission_waits.fetch_add(1, Ordering::Relaxed);
            }
            // Park: `notify_waiters` stores no permit, so a wake can be
            // lost to a not-yet-parked waiter — the 5 ms tick is the
            // liveness backstop BY DESIGN (also how Red/target changes
            // are observed). See write_pipeline_core.rs module docs.
            tokio::select! {
                _ = self.completions.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
    }

    /// Wait until every admitted upload has completed (unmount teardown /
    /// tests). `false` = the deadline elapsed with custody still in
    /// flight.
    pub async fn quiesce(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.core.inflight_blocks() != 0 {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::select! {
                _ = self.completions.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
        true
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
}

impl Drop for PipelinePermit {
    fn drop(&mut self) {
        self.pipe.core.release(self.bytes);
        self.pipe.completions.notify_waiters();
    }
}
