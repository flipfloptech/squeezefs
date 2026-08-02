//! Write-pipeline depth — the 2026-07-27 campaign's contract suite
//! (`src/write_pipeline.rs`; evidence
//! `.benchmarks/2026-07-27-write-pipeline-depth.md`).
//!
//! The convicted mechanism: `upload_full_block` awaited INLINE in the
//! WRITE handler made every writer a closed loop (throughput = threads ÷
//! per-block pipeline latency; device aqu-sz < 2 in the field capture,
//! 0.33× of the raw ceiling on the nvmet-tcp devsub rig). Contracts
//! pinned here:
//!
//! 1. **The design law (no fixed depth)**: the depth target is derived at
//!    runtime from measured per-backend service time × achieved bandwidth
//!    (Little's law × headroom), grows while the device drains faster
//!    than arrival, and is bounded ONLY by the R5 budget cap and honest
//!    writer backpressure. Pure-math pins + governor growth pins.
//! 2. **Admission is honest backpressure**: at target, `admit` parks and
//!    is woken by completions; an empty pipe always admits; Red clamps
//!    the target to its floor (drain posture).
//! 3. **ACK-before-upload with never-lossy custody**: a coverage-complete
//!    write ACKs with custody parked; the detached upload publishes
//!    durably; fsync drains whatever the pipeline has not; a mid-flight
//!    fencing expiry DROPS custody loudly (the remount law, FIND-M11-A)
//!    and never publishes.
//! 4. **Teardown quiesce**: unmount drains in-flight uploads.
//!
//! RED (2026-07-27, against dev 78b9498 + the governor scaffold): the
//! T1 ack-before-upload pin fails while the handler still awaits the
//! upload inline (no admission ever parks, and the block map is already
//! published at ACK).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::error::SqueezefsError;
use squeezefs::fuse_client::{block_lock_acquire, BlockLockSite, SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use squeezefs::write_pipeline::{
    depth_override, lane_target_bytes, pipeline_disposition, rolled_bw_peak, rolled_lat_floor,
    set_depth_override, sync_inline, PipelineDisposition, ProbeCore, WritePipeline,
    BUDGET_CAP_DIVISOR, FLOOR_BLOCKS_PER_LANE, HEADROOM, PROBE_COOLDOWN_EPOCHS, PROBE_EPOCH_MS,
    PROBE_MUL_MAX, PROBE_MUL_ONE,
};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;

/// Process-global knobs + METRICS deltas: suite serializes (house
/// pattern, `write_through_coverage_tests`).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore the adaptive default on scope exit (knob hygiene).
struct OverrideGuard;
impl Drop for OverrideGuard {
    fn drop(&mut self) {
        set_depth_override(None);
    }
}

// =========================================================================
// 1. Pure governor math — the design-law arithmetic.
// =========================================================================

#[test]
fn depth_override_lever_resolves_sync_pinned_adaptive() {
    let _g = OverrideGuard;
    set_depth_override(None);
    assert_eq!(depth_override(), None, "unset = adaptive governor");
    assert!(!sync_inline());
    set_depth_override(Some(0));
    assert_eq!(depth_override(), Some(0), "0 = the A/B sync-inline lever");
    assert!(sync_inline());
    set_depth_override(Some(12));
    assert_eq!(depth_override(), Some(12), "N = pinned blocks, verbatim");
    assert!(!sync_inline());
}

#[test]
fn lane_target_is_floor_while_unlearned_and_bdp_headroom_once_measured() {
    // Unlearned lane (no bandwidth yet): the cold-start floor.
    assert_eq!(
        lane_target_bytes(0, 0, BS),
        FLOOR_BLOCKS_PER_LANE * BS,
        "cold lane must sit at the floor — the pipe stays fed while the \
         estimates learn"
    );
    // Measured lane: BDP × HEADROOM. 1 GB/s × 10 ms = 10 MB BDP.
    let bw = 1_000_000_000u64;
    let lat = 10_000_000u64; // 10 ms in ns
    assert_eq!(
        lane_target_bytes(bw, lat, BS),
        (bw as u128 * lat as u128 / 1_000_000_000) as u64 * HEADROOM,
        "learned lane target must be measured-BDP × HEADROOM — depth \
         derived at runtime, never a constant (the design law)"
    );
    // 800GbE-class arithmetic must not wrap: 100 GB/s × 1 s.
    let big = lane_target_bytes(100_000_000_000, 1_000_000_000, 4 << 20);
    assert!(
        big >= 100_000_000_000,
        "u128 intermediate: an 800GbE-class BDP must not overflow (got {big})"
    );
}

#[test]
fn bw_peak_learns_fast_up_and_decays_slow_down() {
    // A window measuring above the peak adopts it verbatim.
    assert_eq!(
        rolled_bw_peak(1_000, 10_000_000, 250),
        40_000_000,
        "peak must adopt a faster window immediately (fast up)"
    );
    // A quiet window decays the peak by 1/8 — not to zero.
    let decayed = rolled_bw_peak(40_000_000, 0, 250);
    assert_eq!(
        decayed,
        40_000_000 - 40_000_000 / 8,
        "peak must decay by exactly 1/8 per quiet window (slow down)"
    );
}

#[test]
fn lat_floor_decays_upward_but_never_past_the_ewma() {
    // Decay: +1/8 per window, re-learning a genuinely slower device…
    assert_eq!(rolled_lat_floor(8_000_000, 100_000_000), 9_000_000);
    // …but NEVER past the EWMA: congested-queue latency must not feed the
    // BDP (the runaway-growth guard — at saturation inflight ≡ bw × lat,
    // so a congested-latency BDP would chase its own tail).
    assert_eq!(
        rolled_lat_floor(8_000_000, 8_100_000),
        8_100_000,
        "floor decay is bounded by the latency EWMA"
    );
}

#[test]
fn disposition_mapping_is_the_task_counting_contract() {
    assert_eq!(pipeline_disposition(&Ok(())), PipelineDisposition::Done);
    assert_eq!(
        pipeline_disposition(&Err(SqueezefsError::FencingTokenExpired {
            token: 1,
            expected: 2
        })),
        PipelineDisposition::FenceDrop,
        "fencing expiry = custody dropped loudly (the remount law)"
    );
    assert_eq!(
        pipeline_disposition(&Err(SqueezefsError::InvalidOperation("io".into()))),
        PipelineDisposition::StayParked,
        "any other failure = never-lossy parked custody (fsync owns retry)"
    );
}

// =========================================================================
// 2. Governor behavior — growth, Red clamp, budget cap.
// =========================================================================

fn never_red() -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(|| false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn depth_target_grows_with_measured_bandwidth_and_stays_budget_bounded() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(None);
    let pipe = WritePipeline::with_caps(never_red(), Some(1 << 40));
    let floor = FLOOR_BLOCKS_PER_LANE * BS;
    assert_eq!(
        pipe.depth_target_bytes(BS),
        floor,
        "cold pipeline must start at the aggregate floor"
    );

    // Feed one lane: 64 KiB uploads at 1 ms service time, 2 GB/s windowed
    // rate (driven clock — window rolls at +250 ms and +500 ms).
    for i in 0..8u64 {
        pipe.record_completion_at("vol-a", 500_000_000, 1_000_000, 10 + i);
    }
    pipe.record_completion_at("vol-a", 0, 1_000_000, 300); // roll window 1
    pipe.record_completion_at("vol-a", 0, 1_000_000, 600); // roll window 2
    let learned = pipe.depth_target_bytes(BS);
    assert!(
        learned > floor,
        "the depth target must GROW once measured bandwidth × service time \
         exceeds the floor (design law: derived at runtime; got {learned} \
         vs floor {floor})"
    );

    // The R5 budget cap bounds everything (never below one block).
    let capped = WritePipeline::with_caps(never_red(), Some(2 * BS));
    for i in 0..8u64 {
        capped.record_completion_at("vol-a", 500_000_000, 1_000_000, 10 + i);
    }
    capped.record_completion_at("vol-a", 0, 1_000_000, 300);
    capped.record_completion_at("vol-a", 0, 1_000_000, 600);
    assert_eq!(
        capped.depth_target_bytes(BS),
        2 * BS,
        "the budget cap (budget ÷ {BUDGET_CAP_DIVISOR} in production) must \
         bound the learned target"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn red_clamps_target_to_floor_and_pinned_override_wins_verbatim() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(None);
    let red = Arc::new(AtomicBool::new(false));
    let red2 = red.clone();
    let pipe = WritePipeline::with_caps(
        Arc::new(move || red2.load(Ordering::Relaxed)),
        Some(1 << 40),
    );
    for i in 0..8u64 {
        pipe.record_completion_at("vol-a", 500_000_000, 1_000_000, 10 + i);
    }
    pipe.record_completion_at("vol-a", 0, 1_000_000, 300);
    pipe.record_completion_at("vol-a", 0, 1_000_000, 600);
    let learned = pipe.depth_target_bytes(BS);
    assert!(learned > FLOOR_BLOCKS_PER_LANE * BS);

    red.store(true, Ordering::Relaxed);
    assert_eq!(
        pipe.depth_target_bytes(BS),
        FLOOR_BLOCKS_PER_LANE * BS,
        "Red must clamp the target to the floor — honest backpressure while \
         in-flight custody converges by completion, never OOM"
    );
    red.store(false, Ordering::Relaxed);

    set_depth_override(Some(5));
    assert_eq!(
        pipe.depth_target_bytes(BS),
        5 * BS,
        "the pinned A/B override wins verbatim over the governor"
    );
}

// =========================================================================
// 2b. Probe-up governor — the 2026-07-29 campaign (field finding: the
//     pure-BDP target converges to sustaining the CURRENT operating point;
//     forced depth 64 bought +18 % on the 4-node cluster. BBR-flavored
//     law: probe up when throughput responds, retreat when marginal gain
//     dies, never inflate on low offered load).
// =========================================================================

/// Probe launch + adoption: a saturated epoch with headroom launches a
/// probe (+1/4 target); a probe epoch whose delivery responds ADOPTS the
/// raised multiplier and may probe again — exponential headroom discovery,
/// never a constant.
#[test]
fn probe_core_probes_up_on_responsive_saturated_epochs() {
    let p = ProbeCore::new();
    assert_eq!(p.mul_q6(), PROBE_MUL_ONE, "cold probe multiplier is 1.0");
    // First call only opens the epoch window.
    p.on_bytes(1_000_000);
    assert!(!p.roll(10, true, true), "first roll call opens the epoch");
    // Epoch 1 closes saturated with headroom: the probe LAUNCHES.
    p.on_bytes(100_000_000);
    assert!(p.roll(10 + PROBE_EPOCH_MS + 10, true, true));
    assert_eq!(p.probe_ups(), 1, "saturated + headroom must launch a probe");
    assert_eq!(
        p.mul_q6(),
        PROBE_MUL_ONE + PROBE_MUL_ONE / 4,
        "a probe raises the target by 1/4 (the probe gain)"
    );
    // The probe epoch delivers +25 %: ADOPT (multiplier kept)…
    p.on_bytes(125_000_000);
    assert!(p.roll(10 + 2 * (PROBE_EPOCH_MS + 10), true, true));
    assert_eq!(
        p.mul_q6(),
        PROBE_MUL_ONE + PROBE_MUL_ONE / 4,
        "responsive delivery must ADOPT the probed multiplier"
    );
    assert_eq!(p.probe_backoffs(), 0);
    // …and the next saturated epoch probes AGAIN from the adopted level.
    p.on_bytes(160_000_000);
    assert!(p.roll(10 + 3 * (PROBE_EPOCH_MS + 10), true, true));
    assert_eq!(p.probe_ups(), 2, "adoption re-arms the probe immediately");
    assert!(
        p.mul_q6() > PROBE_MUL_ONE + PROBE_MUL_ONE / 4,
        "discovery compounds while the backend keeps responding"
    );
}

/// Backoff + cool-down: a probe whose delivery does NOT respond retreats
/// to the pre-probe multiplier (the BDP posture) and cools down — the
/// dead-gain latency tax is bounded to ~1/(cooldown+1) of epochs.
#[test]
fn probe_core_backs_off_when_gain_dies_and_cools_down() {
    let p = ProbeCore::new();
    let step = PROBE_EPOCH_MS + 10;
    p.roll(10, true, true); // open
    p.on_bytes(100_000_000);
    assert!(p.roll(10 + step, true, true));
    assert_eq!(p.probe_ups(), 1);
    // Probe epoch: +1 % only — below the adoption threshold.
    p.on_bytes(101_000_000);
    assert!(p.roll(10 + 2 * step, true, true));
    assert_eq!(
        p.mul_q6(),
        PROBE_MUL_ONE,
        "a dead-gain probe must RETREAT to the pre-probe multiplier"
    );
    assert_eq!(p.probe_backoffs(), 1, "the retreat must count");
    // Cool-down: the next PROBE_COOLDOWN_EPOCHS saturated epochs hold.
    for i in 0..PROBE_COOLDOWN_EPOCHS {
        p.on_bytes(100_000_000);
        assert!(p.roll(10 + (3 + i) * step, true, true));
        assert_eq!(p.probe_ups(), 1, "cool-down must hold at epoch {i}");
    }
    // After the cool-down the governor may probe again.
    p.on_bytes(100_000_000);
    assert!(p.roll(10 + (3 + PROBE_COOLDOWN_EPOCHS) * step, true, true));
    assert_eq!(p.probe_ups(), 2, "cool-down expiry re-arms the probe");
}

/// The latency guard: unsaturated epochs never launch probes, and an
/// elevated multiplier DECAYS back to 1.0 once offered load stops filling
/// the pipe — low-offered-load / latency-sensitive workloads must never
/// inherit streaming-era queue depth.
#[test]
fn probe_core_latency_guard_never_inflates_without_saturation_and_decays() {
    let p = ProbeCore::new();
    let step = PROBE_EPOCH_MS + 10;
    p.roll(10, false, true); // open
    for i in 1..=20u64 {
        p.on_bytes(10_000_000);
        p.roll(10 + i * step, false, true);
    }
    assert_eq!(p.probe_ups(), 0, "no saturation ⇒ no probe, ever");
    assert_eq!(p.mul_q6(), PROBE_MUL_ONE);

    // Elevate: launch + adopt twice (saturated, responsive).
    p.on_bytes(100_000_000);
    p.roll(10 + 21 * step, true, true); // launch 1
    p.on_bytes(130_000_000);
    p.roll(10 + 22 * step, true, true); // adopt 1
    p.on_bytes(130_000_000);
    p.roll(10 + 23 * step, true, true); // launch 2
    p.on_bytes(170_000_000);
    p.roll(10 + 24 * step, true, true); // adopt 2
    let elevated = p.mul_q6();
    assert!(elevated > PROBE_MUL_ONE, "fixture: multiplier elevated");

    // Starve: unsaturated epochs bleed the multiplier back to 1.0.
    for i in 25..80u64 {
        p.on_bytes(1_000);
        p.roll(10 + i * step, false, true);
    }
    assert_eq!(
        p.mul_q6(),
        PROBE_MUL_ONE,
        "an idle/low-load pipe must bleed the probe multiplier back to \
         the BDP posture (qd1 RTT rows stay flat)"
    );
}

/// Hold re-validation: an ADOPTED multiplier keeps paying rent — if
/// delivery collapses while holding elevated depth, the governor steps
/// back toward the BDP (the unresponsive-backend retreat).
#[test]
fn probe_core_hold_revalidation_retreats_when_delivery_collapses() {
    let p = ProbeCore::new();
    let step = PROBE_EPOCH_MS + 10;
    p.roll(10, true, true); // open
    p.on_bytes(100_000_000);
    p.roll(10 + step, true, true); // launch
    p.on_bytes(130_000_000);
    p.roll(10 + 2 * step, true, true); // adopt at ~260 MB/s
    let adopted = p.mul_q6();
    assert!(adopted > PROBE_MUL_ONE);
    // Saturated HOLD epochs at collapsed delivery (~120 MB/s « adopted).
    p.on_bytes(60_000_000);
    assert!(p.roll(10 + 3 * step, true, true));
    assert!(
        p.mul_q6() < adopted,
        "collapsed delivery under an adopted multiplier must step the \
         target back toward the BDP"
    );
    assert!(p.probe_backoffs() >= 1, "the step-down must count");
}

/// Bounds: the multiplier never exceeds PROBE_MUL_MAX however responsive
/// the backend, and a headroom-less epoch (R5 cap / Red) never launches.
#[test]
fn probe_core_is_bounded_and_never_probes_without_headroom() {
    let p = ProbeCore::new();
    let step = PROBE_EPOCH_MS + 10;
    p.roll(10, true, true); // open
    let mut bytes = 1_000_000u64;
    for i in 1..=200u64 {
        p.on_bytes(bytes);
        p.roll(10 + i * step, true, true);
        bytes += bytes / 3; // always-responsive backend
        if bytes > 1 << 60 {
            bytes = 1 << 60;
        }
        assert!(
            p.mul_q6() <= PROBE_MUL_MAX,
            "the probe multiplier must stay bounded (got {})",
            p.mul_q6()
        );
    }
    assert_eq!(p.mul_q6(), PROBE_MUL_MAX, "fixture: rode to the bound");

    let q = ProbeCore::new();
    q.roll(10, true, false); // open
    for i in 1..=10u64 {
        q.on_bytes(100_000_000 * i);
        q.roll(10 + i * step, true, false);
    }
    assert_eq!(
        q.probe_ups(),
        0,
        "no headroom (R5 cap reached / Red) ⇒ the probe never launches"
    );
    assert_eq!(q.mul_q6(), PROBE_MUL_ONE);
}

/// End-to-end: on a saturated, responsive pipe the DEFAULT governor's
/// depth target must GROW BEYOND the pure-BDP/floor equilibrium (the
/// field's self-limiting conviction), the probe gauges must move, and the
/// target must RETREAT to the floor once offered load stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipeline_probe_grows_depth_target_beyond_bdp_and_retreats() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(None);
    let pipe = WritePipeline::with_caps(never_red(), Some(1 << 40));
    let floor = FLOOR_BLOCKS_PER_LANE * BS;

    // Saturating writer fleet: 32 loops of admit → hold → release against
    // a floor-sized pipe (admission_waits grows every epoch — the
    // production saturation signature).
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..32)
        .map(|_| {
            let pipe = pipe.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    let permit = pipe.admit(BS).await;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    drop(permit);
                }
            })
        })
        .collect();

    // Drive epochs with an ever-responsive delivery script (tiny service
    // time keeps the pure-BDP term far below the floor: any growth beyond
    // the floor is the PROBE, not the BDP).
    let mut now = 10u64;
    let mut bytes = 50_000_000u64;
    pipe.record_completion_at("vol-a", 1, 1_000, now);
    let mut grown = false;
    for _ in 0..60 {
        now += PROBE_EPOCH_MS + 10;
        pipe.record_completion_at("vol-a", bytes, 1_000, now);
        bytes += bytes / 4;
        if pipe.depth_target_bytes(BS) >= 2 * floor {
            grown = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        grown,
        "the DEFAULT governor must discover headroom past the BDP/floor \
         equilibrium on a saturated responsive pipe (the field's +18 % \
         forced-depth conviction; target {} floor {floor})",
        pipe.depth_target_bytes(BS)
    );
    assert!(
        pipe.depth_probe_ups() >= 1,
        "probe engagement must gauge (write_pipeline_depth_probe_ups)"
    );
    assert!(
        pipe.depth_target_base_bytes(BS) < pipe.depth_target_bytes(BS),
        "the base (pure-BDP) target gauge must sit below the probed target"
    );

    // Stop the offered load: the target must retreat to the floor.
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.await.unwrap();
    }
    assert!(
        pipe.quiesce(Duration::from_secs(5)).await,
        "writer fleet must drain"
    );
    for _ in 0..60 {
        now += PROBE_EPOCH_MS + 10;
        pipe.record_completion_at("vol-a", 1_000, 1_000, now);
        if pipe.depth_target_bytes(BS) == floor {
            break;
        }
    }
    assert_eq!(
        pipe.depth_target_bytes(BS),
        floor,
        "the probe multiplier must bleed off once offered load vanishes \
         (the latency guard — qd1 rows stay flat)"
    );
}

/// The R5 postures are senior to the probe: a budget-capped pipe never
/// probes past the cap, and Red clamps a probed target to the floor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipeline_probe_respects_budget_cap_and_red_clamp() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(None);

    // Cap below the floor: the target must never exceed the cap and the
    // probe must never launch (no headroom).
    let capped = WritePipeline::with_caps(never_red(), Some(2 * BS));
    let mut now = 10u64;
    capped.record_completion_at("vol-a", 1, 1_000, now);
    let mut bytes = 50_000_000u64;
    for _ in 0..10 {
        now += PROBE_EPOCH_MS + 10;
        capped.record_completion_at("vol-a", bytes, 1_000, now);
        bytes += bytes / 4;
        assert_eq!(
            capped.depth_target_bytes(BS),
            2 * BS,
            "the R5 budget cap bounds the probed target"
        );
    }
    assert_eq!(
        capped.depth_probe_ups(),
        0,
        "a capped pipe has no headroom — the probe must not launch"
    );

    // Red: a probed-up pipe clamps to the floor while Red holds. Elevate
    // via a parking writer fleet (the production saturation signal) +
    // responsive delivery, then flip Red.
    let red = Arc::new(AtomicBool::new(false));
    let red2 = red.clone();
    let pipe = WritePipeline::with_caps(
        Arc::new(move || red2.load(Ordering::Relaxed)),
        Some(1 << 40),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..16)
        .map(|_| {
            let pipe = pipe.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    let permit = pipe.admit(BS).await;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    drop(permit);
                }
            })
        })
        .collect();
    let mut now = 10u64;
    let mut bytes = 50_000_000u64;
    pipe.record_completion_at("vol-a", 1, 1_000, now);
    for _ in 0..60 {
        now += PROBE_EPOCH_MS + 10;
        pipe.record_completion_at("vol-a", bytes, 1_000, now);
        bytes += bytes / 4;
        if pipe.depth_target_bytes(BS) > FLOOR_BLOCKS_PER_LANE * BS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        pipe.depth_target_bytes(BS) > FLOOR_BLOCKS_PER_LANE * BS,
        "fixture: probed above the floor"
    );
    red.store(true, Ordering::Relaxed);
    assert_eq!(
        pipe.depth_target_bytes(BS),
        FLOOR_BLOCKS_PER_LANE * BS,
        "Red must clamp a probed target to the floor — honest backpressure, \
         never OOM"
    );
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.await.unwrap();
    }
}

// =========================================================================
// 3. Admission — park at target, wake on completion, quiesce.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_parks_at_target_and_wakes_on_completion() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(Some(2)); // pin the target: 2 blocks
    let pipe = WritePipeline::with_caps(never_red(), Some(1 << 40));

    let p1 = pipe.admit(BS).await;
    let _p2 = pipe.admit(BS).await;
    assert_eq!(pipe.inflight_blocks(), 2);
    assert_eq!(pipe.inflight_bytes(), 2 * BS);

    // The third admission must PARK (honest backpressure).
    let pipe2 = pipe.clone();
    let third = tokio::spawn(async move { pipe2.admit(BS).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !third.is_finished(),
        "admission past the depth target must park the writer"
    );
    assert!(
        pipe.admission_waits() >= 1,
        "parked admissions must count (write_pipeline_admission_waits)"
    );

    // A completion (permit drop) wakes it.
    drop(p1);
    let p3 = tokio::time::timeout(Duration::from_secs(5), third)
        .await
        .expect("completion must wake a parked admission")
        .unwrap();
    assert_eq!(pipe.inflight_blocks(), 2);
    drop(p3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_pipe_always_admits_and_quiesce_waits_for_drain() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(Some(1));
    let pipe = WritePipeline::with_caps(never_red(), Some(1 << 40));

    // Progress guarantee: a block larger than the target still admits on
    // an empty pipe.
    let p = pipe.admit(64 * BS).await;
    assert_eq!(pipe.inflight_blocks(), 1);

    // Quiesce times out while custody is in flight…
    assert!(
        !pipe.quiesce(Duration::from_millis(50)).await,
        "quiesce must not report drained while a permit is live"
    );
    // …and completes once it drains.
    drop(p);
    assert!(
        pipe.quiesce(Duration::from_secs(5)).await,
        "quiesce must observe the drain"
    );
    assert_eq!(pipe.inflight_bytes(), 0);
}

// =========================================================================
// 4. Integration — the mounted-write contracts (harness: the
//    write_through_coverage_tests pattern).
// =========================================================================

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // Pin the W1 patch path OFF (same reason as the coverage suite: the
    // downscaled BS would make sub-block segments patch-eligible and
    // bypass the machinery under test).
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_1234_5678,
            uuid,
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

/// A striped fixture: > 1 block so the layout classifies striped, then
/// fsync so the map is published and the pipeline is drained.
async fn striped_fixture(h: &H, name: &str) -> u64 {
    let ino = create(h, name).await;
    let base = pattern(2 * BS as usize, 0x11);
    write_at(h, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "fixture pipeline must drain"
    );
    ino
}

async fn block_map_has(h: &H, ino: u64, b: u32) -> bool {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router
        .fetch_metadata(&path)
        .await
        .ok()
        .and_then(|m| m.block_map.as_ref().map(|bm| bm.contains_key(&b)))
        .unwrap_or(false)
}

async fn eventually(mut cond: impl AsyncFnMut() -> bool, what: &str) {
    for _ in 0..600 {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition never held within 6s: {what}");
}

/// T1 — THE headline pin (RED against the inline-await tree): a
/// coverage-completing WRITE must ACK while its upload is still admitted
/// custody (pipe blocked ⇒ nothing published), and the detached upload
/// must then publish durably with the overlay retired — read-back
/// identical throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t1_completing_write_acks_before_upload_and_drains_durably() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(Some(1)); // one slot — the test owns it first
    let h = make(*b"wpd-t1-ackfirst!", "wpd_ns_t1").await;
    let ino = striped_fixture(&h, "t1").await;
    let waits0 = h.fs.write_pipeline.admission_waits();
    let wt0 = METRICS.write_through_blocks.load(Ordering::Relaxed);

    // Fill the single admission slot so the completing write PARKS at
    // admission (the deterministic rendezvous).
    let blocker = h.fs.write_pipeline.admit(BS).await;

    // Overwrite block 2 (fresh block, beyond the fixture) in one covering
    // write, on a spawned task — it must park at admission, not error.
    let data = pattern(BS as usize, 0x5A);
    let fs2 = h.fs.clone();
    let req = h.req;
    let d2 = bytes::Bytes::copy_from_slice(&data);
    let w = tokio::spawn(async move {
        fs2.write(req, ino, 0, 2 * BS, d2, 0, 0).await.unwrap();
    });

    // Rendezvous: the write is parked at admission.
    let pipe = h.fs.write_pipeline.clone();
    eventually(
        async || pipe.admission_waits() > waits0,
        "the completing write must park at the admission gate \
         (RED: the inline-await tree never admits)",
    )
    .await;
    assert!(
        !w.is_finished(),
        "the WRITE must be parked at admission while the pipe is full"
    );
    assert!(
        !block_map_has(&h, ino, 2).await,
        "nothing may publish while the upload is not even admitted"
    );

    // Custody is READABLE while parked (overlay serves the acked bytes
    // even before the ACK — same law as every parked partial).
    // Release the slot: the write must ACK…
    drop(blocker);
    tokio::time::timeout(Duration::from_secs(10), w)
        .await
        .expect("releasing the pipe must let the write ACK")
        .unwrap();

    // …and the DETACHED upload must publish durably.
    let h_ref = &h;
    eventually(
        async || block_map_has(h_ref, ino, 2).await,
        "the detached pipeline upload must publish the block map entry",
    )
    .await;
    eventually(
        async || METRICS.write_through_blocks.load(Ordering::Relaxed) > wt0,
        "write_through_blocks must count the detached upload",
    )
    .await;
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(10)).await,
        "the pipeline must drain after the upload"
    );
    let got = read_at(&h, ino, 2 * BS, BS as usize).await;
    assert_eq!(got, data, "read-back after the detached publish");
}

/// T2 — the fencing custody-drop law (FIND-M11-A applied to detached
/// uploads): a stale-era write-through must retire the parked overlay,
/// publish NOTHING, and propagate `FencingTokenExpired`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t2_fenced_write_through_drops_custody_and_never_publishes() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(Some(1));
    let h = make(*b"wpd-t2-fencedrop", "wpd_ns_t2").await;
    let ino = striped_fixture(&h, "t2").await;
    let waits0 = h.fs.write_pipeline.admission_waits();

    // Park a completing write at admission: its custody sits complete in
    // the overlay map with NO upload admitted.
    let blocker = h.fs.write_pipeline.admit(BS).await;
    let data = pattern(BS as usize, 0x77);
    let fs2 = h.fs.clone();
    let req = h.req;
    let d2 = bytes::Bytes::copy_from_slice(&data);
    let w = tokio::spawn(async move {
        fs2.write(req, ino, 0, 3 * BS, d2, 0, 0).await.unwrap();
    });
    let pipe = h.fs.write_pipeline.clone();
    eventually(
        async || pipe.admission_waits() > waits0,
        "the write must park at admission",
    )
    .await;

    // Drive the write-through unit DIRECTLY with a stale-era token (the
    // write_through_tests stale-token pattern): custody must drop, and
    // nothing may publish.
    let cache_key = squeezefs::keys::active_block(ino, 3).to_string();
    let current = h.fs.dlm().get_fencing_token_ino(ino);
    assert!(current > 0, "the write path must have acquired a lease");
    let guard = block_lock_acquire(ino, 3, BlockLockSite::PipelineUpload).await;
    let res =
        h.fs.write_through_complete_block(ino, 3, &cache_key, current - 1, guard)
            .await;
    assert!(
        matches!(res, Err(SqueezefsError::FencingTokenExpired { .. })),
        "a superseded era must be refused loud (got {res:?})"
    );
    assert!(
        !block_map_has(&h, ino, 3).await,
        "a fenced write-through must publish NOTHING"
    );

    // Release the parked write: it ACKs, and its detached upload must
    // resolve as a clean no-op against the dropped custody.
    drop(blocker);
    tokio::time::timeout(Duration::from_secs(10), w)
        .await
        .expect("the parked write must ACK once admitted")
        .unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(10)).await,
        "the no-op task must drain"
    );
    assert!(
        !block_map_has(&h, ino, 3).await,
        "dropped custody must stay unpublished (the remount law)"
    );
}

/// T3 — fsync owns the drain: durability on return regardless of where
/// the pipeline is (races the detached tasks; both sides are idempotent).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t3_fsync_drains_pending_pipeline_uploads_durably() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(None); // the shipped adaptive default
    let h = make(*b"wpd-t3-fsyncdrn!", "wpd_ns_t3").await;
    let ino = create(&h, "t3").await;
    let data = pattern(4 * BS as usize, 0x33);
    write_at(&h, ino, 0, &data).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "pipeline must drain around fsync"
    );
    for b in 0..4u32 {
        assert!(
            block_map_has(&h, ino, b).await,
            "block {b} must be durably mapped after fsync"
        );
    }
    assert_eq!(
        h.fs.parked_overlay_gate_count(),
        0,
        "no parked overlay may survive fsync + drain"
    );
    let got = read_at(&h, ino, 0, data.len()).await;
    assert_eq!(got, data, "read-back after fsync");
}

/// T5 — the fsync-steal economy law: fsync draining a pipe-blocked
/// coverage-complete block must ride the WRITE-THROUGH leg (one durable
/// upload, counted in `write_through_blocks`), never the staging +
/// writeback detour — that is the parked-straggler RMW pipeline the RW3b
/// coverage suite killed, and it would resurface on every fsync that wins
/// the race against a detached upload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t5_fsync_steal_rides_write_through_not_staging() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(Some(1)); // one slot — the test owns it first
    let h = make(*b"wpd-t5-fsyncstl!", "wpd_ns_t5").await;
    let ino = striped_fixture(&h, "t5").await;
    let waits0 = h.fs.write_pipeline.admission_waits();

    // Block the pipe; a covering write of a fresh block parks at admission
    // with its COMPLETE custody in the overlay (readable, unpublished).
    let blocker = h.fs.write_pipeline.admit(BS).await;
    let data = pattern(BS as usize, 0x66);
    let fs2 = h.fs.clone();
    let req = h.req;
    let d2 = bytes::Bytes::copy_from_slice(&data);
    let w = tokio::spawn(async move {
        fs2.write(req, ino, 0, 2 * BS, d2, 0, 0).await.unwrap();
    });
    let pipe = h.fs.write_pipeline.clone();
    eventually(
        async || pipe.admission_waits() > waits0,
        "the write must park at admission",
    )
    .await;

    // fsync steals the complete parked block: exactly one write-through,
    // ZERO staging puts / writeback enqueues (the economy pin), durably
    // mapped at fsync return.
    let wt0 = METRICS.write_through_blocks.load(Ordering::Relaxed);
    let sp0 = METRICS.staging_put_bytes_flush.load(Ordering::Relaxed);
    let wb0 = METRICS.writeback_enqueued_flush.load(Ordering::Relaxed);
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert_eq!(
        METRICS.write_through_blocks.load(Ordering::Relaxed) - wt0,
        1,
        "fsync stealing a coverage-complete parked block must count as a \
         write-through (the flush write-through leg)"
    );
    assert_eq!(
        METRICS.staging_put_bytes_flush.load(Ordering::Relaxed) - sp0,
        0,
        "a coverage-complete block must NEVER take the fsync staging detour \
         (the parked-straggler RMW pipeline is dead — RW3b)"
    );
    assert_eq!(
        METRICS.writeback_enqueued_flush.load(Ordering::Relaxed) - wb0,
        0,
        "no writeback unit may be conjured for a stolen complete block"
    );
    assert!(
        block_map_has(&h, ino, 2).await,
        "the stolen block must be durably mapped at fsync return"
    );

    // The parked write then ACKs and its detached upload resolves as a
    // clean no-op against the already-drained custody.
    drop(blocker);
    tokio::time::timeout(Duration::from_secs(10), w)
        .await
        .expect("the parked write must ACK once admitted")
        .unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(10)).await,
        "the no-op task must drain"
    );
    let got = read_at(&h, ino, 2 * BS, BS as usize).await;
    assert_eq!(got, data, "read-back after the fsync steal");
}

/// T4 — teardown quiesce: the dismount path drains detached uploads
/// before shutting the volumes down (custody never stranded).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t4_teardown_drains_pipeline_custody() {
    let _s = serial().await;
    let _g = OverrideGuard;
    set_depth_override(None);
    let h = make(*b"wpd-t4-teardown!", "wpd_ns_t4").await;
    let ino = create(&h, "t4").await;
    let data = pattern(3 * BS as usize, 0x44);
    write_at(&h, ino, 0, &data).await;
    // No fsync: teardown must own whatever the pipeline still holds.
    h.fs.force_flush_all_staged_data().await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "teardown-order drain must converge"
    );
    for b in 0..3u32 {
        assert!(
            block_map_has(&h, ino, b).await,
            "block {b} must be durable after the teardown flush"
        );
    }
    let got = read_at(&h, ino, 0, data.len()).await;
    assert_eq!(got, data, "read-back after teardown flush");
}
