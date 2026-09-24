//! C-2 — the journal ring write's completion hop (e2e perf audit
//! `docs/design-e2e-perf-audit.md` §3 DLM board #3; baseline
//! `.benchmarks/2026-09-03-d2-two-stage-conveyor.md`).
//!
//! **The finding**: D-2 moved the journal write's completion off the
//! serialized apply pass and isolated it as THE term on the co-located
//! fleet — `journal_ring_write` (submission → observed completion) reads a
//! 1.0–1.3 ms MEAN with a 50–200 µs MODE per window, for a ~1 KiB page-cache
//! write. The span is a chain of thread hops: the pass pushes the request
//! on the `uring_fs` pool's queue (→ a parked worker wakes), the worker
//! submits and waits in its ring (→ the kernel's io-wq punt for a buffered
//! block-device write wakes it back), the worker sends the outcome on a
//! oneshot whose waker enqueues the durability-lane task on one of the two
//! `sqz-meta` lanes (→ that lane thread wakes and dispatches it behind
//! whatever the owner's shipped-verb serves queued ahead). Under load the
//! chain inflates the mean 5–10× over the mode and HOL-blocks the in-order
//! durability lane behind it.
//!
//! **The instrument** (`uring_fs_write_phase_ns`, exact-sum, zero-alloc):
//! `queue_hop` (submit → worker admit), `device` (admit → CQE reaped +
//! outcome sent), `wake_hop` (sent → the durability lane observed it),
//! `total` (≡ `journal_ring_write`). The rows below read it against a
//! synthetic serve load on the `sqz-meta` lanes (the owner's ~7 k/s
//! `spawn_meta_join` serves made a controlled hog) and against whole-box
//! CPU saturation (the fleet's 200 runnable threads on 32 cores), on the
//! D-2 sandbox at D = 0 (the field's page-cache write, no device latency
//! arm).
//!
//! Suite runs `--test-threads=1` (process-global stats + lane hogs).

use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, KvMetaBackend, TEST_CONVEYOR_HOLD_STAGE,
};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::{
    META_CONVEYOR_LEADER_PASSES, META_CONVEYOR_WINDOWS_INFLIGHT, META_KV_JOURNAL_ENTRIES,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::uring_fs::{uring_fs_write_phase_totals, UringFsWritePhase};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 128 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
/// Wide enough that a row never parks on ring admission.
const RING_LEN: u64 = 8 * 1024 * 1024;

/// `SQUEEZEFS_JOURNAL_LANE` is read at the open, so the CONTROL sandbox
/// (the shipped D-2 chain, the lever off) sets it around its open under one
/// lock the isolated sandboxes share — a parallel test run never opens an
/// isolated volume through a control's env word.
static SANDBOX_OPEN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn sandbox() -> (Arc<RoutedMetaBackend>, Arc<KvMetaBackend>, NamedTempFile) {
    sandbox_with_lane(true).await
}

/// The same volume opened with the journal lane OFF: the shipped D-2 chain
/// (both stages on the shared `sqz-meta` pool) — the same-binary control
/// the structural row is judged against.
async fn control_sandbox() -> (Arc<RoutedMetaBackend>, Arc<KvMetaBackend>, NamedTempFile) {
    sandbox_with_lane(false).await
}

async fn sandbox_with_lane(
    lane: bool,
) -> (Arc<RoutedMetaBackend>, Arc<KvMetaBackend>, NamedTempFile) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL_LEN).unwrap();
    format_v3(
        file.path(),
        VOL_LEN,
        &FormatV3Options {
            node_size: NODE_SIZE,
            journal_len_override: Some(RING_LEN),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let kv = {
        let _open = SANDBOX_OPEN.lock().await;
        if !lane {
            std::env::set_var("SQUEEZEFS_JOURNAL_LANE", "0");
        }
        let opened = KvMetaBackend::open(file.path()).await;
        if !lane {
            std::env::remove_var("SQUEEZEFS_JOURNAL_LANE");
        }
        opened.expect("open")
    };
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv.clone()]));
    (routed, kv, file)
}

/// RAII: the fault shim and the conveyor seams never leak across tests.
struct FaultGuard;
impl Drop for FaultGuard {
    fn drop(&mut self) {
        squeezefs::uring_fs::clear_faults();
        TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
        test_conveyor_hold_release();
    }
}

fn spin_for(d: Duration) {
    let t0 = Instant::now();
    while t0.elapsed() < d {
        std::hint::spin_loop();
    }
}

/// The owner's serve load made a controlled hog: `tasks` detached tasks
/// on the `sqz-meta` pool, each spinning `burst` of CPU per poll and
/// yielding (the shape of a `spawn_meta_join` verb serve: a short CPU
/// burst, then the lane moves on). Every wake delivered onto a hogged
/// lane waits behind the bursts queued ahead of it — the FIFO run-queue
/// hop the finding names.
struct LaneHog {
    stop: Arc<AtomicBool>,
}

impl LaneHog {
    fn start(tasks: usize, burst: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        for _ in 0..tasks {
            let stop = stop.clone();
            squeezefs::meta_exec::spawn_meta("c2_lane_hog", async move {
                while !stop.load(Ordering::Relaxed) {
                    spin_for(burst);
                    tokio::task::yield_now().await;
                }
            });
        }
        Self { stop }
    }
}

impl Drop for LaneHog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Whole-box CPU saturation: `threads` OS threads spinning at the default
/// priority for the row's duration — the fleet's runnable-thread surplus
/// (192 dd + 9 daemons on 32 cores), so every cross-thread wake in the
/// chain pays the scheduler's dispatch latency.
struct BoxHog {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl BoxHog {
    fn start(threads: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handles = (0..threads)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        spin_for(Duration::from_micros(200));
                    }
                })
            })
            .collect();
        Self {
            stop,
            threads: handles,
        }
    }
}

impl Drop for BoxHog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[derive(Default, Clone, Copy)]
struct UfsSnap {
    queue_hop: (u64, u64),
    device: (u64, u64),
    wake_hop: (u64, u64),
    total: (u64, u64),
}

fn ufs_snap() -> UfsSnap {
    UfsSnap {
        queue_hop: uring_fs_write_phase_totals(UringFsWritePhase::QueueHop),
        device: uring_fs_write_phase_totals(UringFsWritePhase::Device),
        wake_hop: uring_fs_write_phase_totals(UringFsWritePhase::WakeHop),
        total: uring_fs_write_phase_totals(UringFsWritePhase::Total),
    }
}

/// The `uring_fs_write_phase_ns` `wake_hop` bucket counts (the mode's
/// input — see `mode_us`).
fn ufs_wake_hop_buckets() -> Vec<u64> {
    let j = squeezefs::uring_fs::uring_fs_write_phase_json();
    let p = &j["wake_hop"];
    squeezefs::latency_core::LATENCY_BUCKET_LABELS
        .iter()
        .map(|l| p["buckets"][*l].as_u64().unwrap_or(0))
        .collect()
}

fn mean_us(after: (u64, u64), before: (u64, u64)) -> f64 {
    let sum = after.0 - before.0;
    let n = after.1 - before.1;
    if n == 0 {
        0.0
    } else {
        sum as f64 / n as f64 / 1e3
    }
}

/// One `meta_txpass_phase_ns` phase's exact `(sum_ns, count)` + bucket
/// counts.
fn txpass_phase(name: &str) -> ((u64, u64), Vec<u64>) {
    let j = squeezefs::fuse_client::meta_txpass_phase_json();
    let p = &j[name];
    let buckets = squeezefs::latency_core::LATENCY_BUCKET_LABELS
        .iter()
        .map(|l| p["buckets"][*l].as_u64().unwrap_or(0))
        .collect();
    (
        (
            p["sum_ns"].as_u64().unwrap_or(0),
            p["count"].as_u64().unwrap_or(0),
        ),
        buckets,
    )
}

/// The share (0..=1) of a bucketed histogram delta's samples that lie in
/// buckets whose LOWER bound is ≥ `threshold_us` — i.e. samples that took
/// at least `threshold_us` (bucket `i` covers `(2^(i-1), 2^i]` µs).
fn share_at_or_above_us(after: &[u64], before: &[u64], threshold_us: u64) -> f64 {
    let mut total = 0u64;
    let mut above = 0u64;
    for (i, (a, b)) in after.iter().zip(before).enumerate() {
        let d = a - b;
        total += d;
        let lower_us = if i == 0 { 0 } else { 1u64 << (i - 1) };
        if lower_us >= threshold_us {
            above += d;
        }
    }
    if total == 0 {
        0.0
    } else {
        above as f64 / total as f64
    }
}

/// The mode of a bucketed histogram delta: the upper bound (µs) of the
/// most-populated bucket (bucket `i` covers `(2^(i-1), 2^i]` µs).
fn mode_us(after: &[u64], before: &[u64]) -> u64 {
    let mut best = (0usize, 0u64);
    for (i, (a, b)) in after.iter().zip(before).enumerate() {
        let d = a - b;
        if d > best.1 {
            best = (i, d);
        }
    }
    1u64 << best.0
}

/// One row's readings.
struct Row {
    txs: u64,
    passes: u64,
    entries: u64,
    wall: Duration,
    queue_wait_us: f64,
    /// `journal_ring_write` (submit → observed): mean / mode / count.
    ring_write_mean_us: f64,
    ring_write_mode_us: u64,
    ring_write_n: u64,
    /// `uring_fs_write_phase_ns` means.
    queue_hop_us: f64,
    device_us: f64,
    wake_hop_us: f64,
    ufs_total_us: f64,
    /// Exact-sum residual: |Σ(queue_hop, device, wake_hop) − total| ns.
    sum_residual_ns: u64,
    lane_wait_us: f64,
    /// The structural laws' statistic: the SHARE of windows whose
    /// `wake_hop` / `window_lane_wait` / `tx_queue_wait` took at least one
    /// full serve burst. A hop that lands behind the serve lanes puts
    /// (nearly) EVERY window a burst late — share ≈ 1; an isolated lane on
    /// a loaded host shows a few percent of scheduler stalls. Neither a
    /// mean (skewed by one multi-ms stall — the all-features gate flipped
    /// it 1-in-4 with the topology correct) nor a mode (noisy over a few
    /// hundred windows whose sub-burst mass is flat) discriminates as
    /// cleanly.
    wake_hop_burst_share: f64,
    lane_wait_burst_share: f64,
    queue_wait_burst_share: f64,
}

impl Row {
    fn txs_per_s(&self) -> f64 {
        self.txs as f64 / self.wall.as_secs_f64()
    }
}

/// `committers` concurrent tasks × `per` creates in the root directory
/// (regular files take the parent SHARED — co-queueable), D = 0.
async fn hop_row(routed: &Arc<RoutedMetaBackend>, tag: &str, committers: usize, per: usize) -> Row {
    let u0 = ufs_snap();
    let whb0 = ufs_wake_hop_buckets();
    let (rw0, rwb0) = txpass_phase("journal_ring_write");
    let (lw0, lwb0) = txpass_phase("window_lane_wait");
    let (qw0, qwb0) = txpass_phase("tx_queue_wait");
    let passes0 = META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed);
    let entries0 = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);

    let t0 = Instant::now();
    let mut tasks = Vec::with_capacity(committers);
    for c in 0..committers {
        let routed = routed.clone();
        let tag = tag.to_string();
        tasks.push(tokio::spawn(async move {
            for i in 0..per {
                routed
                    .create(1, &format!("{tag}_c{c}_{i}"), libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap_or_else(|e| panic!("create c{c}/{i} failed: {e}"));
            }
        }));
    }
    for t in tasks {
        t.await.expect("committer task");
    }
    let wall = t0.elapsed();

    let u1 = ufs_snap();
    let whb1 = ufs_wake_hop_buckets();
    let (rw1, rwb1) = txpass_phase("journal_ring_write");
    let (lw1, lwb1) = txpass_phase("window_lane_wait");
    let (qw1, qwb1) = txpass_phase("tx_queue_wait");
    let parts = (u1.queue_hop.0 - u0.queue_hop.0)
        + (u1.device.0 - u0.device.0)
        + (u1.wake_hop.0 - u0.wake_hop.0);
    let total = u1.total.0 - u0.total.0;
    Row {
        txs: (committers * per) as u64,
        passes: META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed) - passes0,
        entries: META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - entries0,
        wall,
        queue_wait_us: mean_us(qw1, qw0),
        ring_write_mean_us: mean_us(rw1, rw0),
        ring_write_mode_us: mode_us(&rwb1, &rwb0),
        ring_write_n: rw1.1 - rw0.1,
        queue_hop_us: mean_us(u1.queue_hop, u0.queue_hop),
        device_us: mean_us(u1.device, u0.device),
        wake_hop_us: mean_us(u1.wake_hop, u0.wake_hop),
        ufs_total_us: mean_us(u1.total, u0.total),
        sum_residual_ns: parts.abs_diff(total),
        lane_wait_us: mean_us(lw1, lw0),
        wake_hop_burst_share: share_at_or_above_us(&whb1, &whb0, STRUCTURAL_BURST_US),
        lane_wait_burst_share: share_at_or_above_us(&lwb1, &lwb0, STRUCTURAL_BURST_US),
        queue_wait_burst_share: share_at_or_above_us(&qwb1, &qwb0, STRUCTURAL_BURST_US),
    }
}

fn build_label() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

fn print_header(title: &str) {
    println!("{title}");
    println!(
        "{:<22}{:>8}{:>7}{:>9}{:>9}{:>10}{:>9}{:>7}{:>10}{:>9}{:>10}{:>9}",
        "row",
        "txs/s",
        "passes",
        "tx/pass",
        "txq_us",
        "write_us",
        "mode_us",
        "m/m",
        "queue_us",
        "dev_us",
        "wake_us",
        "lane_us"
    );
}

fn print_row(label: &str, row: &Row) {
    println!(
        "{:<22}{:>8.0}{:>7}{:>9.1}{:>9.1}{:>10.1}{:>9}{:>7.1}{:>10.1}{:>9.1}{:>10.1}{:>9.1}",
        label,
        row.txs_per_s(),
        row.passes,
        row.txs as f64 / row.passes.max(1) as f64,
        row.queue_wait_us,
        row.ring_write_mean_us,
        row.ring_write_mode_us,
        row.ring_write_mean_us / row.ring_write_mode_us.max(1) as f64,
        row.queue_hop_us,
        row.device_us,
        row.wake_hop_us,
        row.lane_wait_us,
    );
}

/// The row's validity laws (every row, every posture): one tx = one
/// checksummed journal entry; the in-flight gauge closes; the hop
/// decomposition sums EXACTLY to the observed round trip (the three spans
/// share the four instants — a residual would mean a stamp was lost);
/// `total` and `journal_ring_write` count the same windows.
fn assert_row_valid(label: &str, row: &Row) {
    assert!(
        row.entries >= row.txs && row.entries <= row.txs + 16,
        "[{label}] one tx = one checksummed journal entry: {} entries for {} txs",
        row.entries,
        row.txs
    );
    assert_eq!(
        META_CONVEYOR_WINDOWS_INFLIGHT.load(Ordering::Relaxed),
        0,
        "[{label}] no window left in flight after every committer returned"
    );
    assert!(
        row.ring_write_n >= 1,
        "[{label}] the row must have observed at least one ring write"
    );
    assert!(
        row.sum_residual_ns <= row.ring_write_n,
        "[{label}] the hop decomposition is exact-sum: |Σ parts − total| = {} ns over {} \
         writes",
        row.sum_residual_ns,
        row.ring_write_n
    );
    // The two histograms record the SAME windows from two threads (the
    // reactor stamps `total` at the CQE; the durability lane stamps
    // `journal_ring_write` when it observes the completion), so their means
    // agree within a scheduling residue: 5 % on an unloaded box, wider
    // under a deliberate 2×-cpus spinning hog — the 1.2.3 chain's laptop
    // read −7.2 % on the "lane + box hog" row (691.9 vs 745.9 µs), a
    // run-queue wait between the two stamps, not a lost window. The hog
    // rows exist for the hop ATTRIBUTION; the identity is pinned tight on
    // the rows without a box hog.
    let band = if label.contains("box") { 0.25 } else { 0.05 };
    assert!(
        (row.ufs_total_us - row.ring_write_mean_us).abs() <= row.ring_write_mean_us * band + 2.0,
        "[{label}] uring_fs `total` ({:.1} us) is the journal write's own span \
         (`journal_ring_write` {:.1} us; band {:.0} %)",
        row.ufs_total_us,
        row.ring_write_mean_us,
        band * 100.0
    );
}

const COMMITTERS: usize = 16;
const PER: usize = 32;
/// The owner's serve shape: ~100 µs of CPU per verb (`meta_ship_owner_
/// phase_ns.execute` on the fleet), one hog task per lane plus one so the
/// round-robin cannot leave a lane idle.
const HOG_TASKS: usize = 3;
const HOG_BURST: Duration = Duration::from_micros(100);

// ===========================================================================
// 1. The measurement rows — the campaign's in-process instrument
// ===========================================================================

/// **The completion hop under lane load and under box saturation.** The
/// D-2 sandbox at D = 0, 16 committers × 32 creates: quiet; with the
/// `sqz-meta` lanes hogged by the serve-shaped load; with the box
/// saturated by `2 × cpus` spinning threads; both. Each row prints the
/// `journal_ring_write` mean vs its mode and the exact `uring_fs` hop
/// split (queue_hop / device / wake_hop).
///
/// The contract on the SHIPPED path (this file's red form, §2 below):
/// under the lane hog the mean must track the mode within 2× — a hop
/// chain that crosses the hogged lane cannot hold it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn measurement_rows_completion_hop_under_lane_and_box_load() {
    let _faults = FaultGuard;
    let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
    print_header(&format!(
        "C-2 in-process rows ({} build, file-backed KV sandbox on {}, {COMMITTERS} committers x \
         {PER} creates, D = 0; lane hog = {HOG_TASKS} x {HOG_BURST:?} bursts on sqz-meta; box hog \
         = {} spinning threads):",
        build_label(),
        std::env::temp_dir().display(),
        2 * cpus
    ));
    let rows: [(&str, bool, bool); 4] = [
        ("quiet", false, false),
        ("lane hog", true, false),
        ("box hog", false, true),
        ("lane + box hog", true, true),
    ];
    for (label, lane, boxed) in rows {
        let (routed, kv, _file) = sandbox().await;
        let lane_hog = lane.then(|| LaneHog::start(HOG_TASKS, HOG_BURST));
        let box_hog = boxed.then(|| BoxHog::start(2 * cpus));
        // Let the hogs occupy their lanes/cores before the row starts.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let row = hop_row(
            &routed,
            &label.replace(' ', "_").replace('+', "and"),
            COMMITTERS,
            PER,
        )
        .await;
        drop(lane_hog);
        drop(box_hog);
        print_row(label, &row);
        assert_row_valid(label, &row);
        kv.shutdown().await.expect("shutdown");
        drop(routed);
    }
}

// ===========================================================================
// 2. The contract (red against the shipped chain)
// ===========================================================================

/// The exaggerated serve burst the structural contract arms: long enough
/// that a hop landing behind ONE burst is unmistakable against the write's
/// own ~100 µs round trip.
const STRUCTURAL_BURST: Duration = Duration::from_millis(2);
/// The burst as a histogram threshold: samples in the `(1024, 2048]` µs
/// bucket and above took ≥ one full burst (the octave floor at/above
/// the burst's magnitude).
const STRUCTURAL_BURST_US: u64 = 1024;
/// A hop behind the serve lanes puts ~every window a burst late; an
/// isolated lane on a loaded host shows a few percent of scheduler
/// stalls. 25 % keeps a ≥ 4× discrimination from the red shape.
const BURST_SHARE_LIMIT: f64 = 0.25;

/// **The commit plane's completion delivery is isolated from the serve
/// plane.** Four hog tasks × 2 ms bursts saturate both `sqz-meta` lanes —
/// the STRUCTURAL form of the fleet's owner serves on the lanes the
/// durability task shares with them. 16 × 32 creates at D = 0. Laws:
///
/// * `wake_hop` (the reaper observed the CQE → the durability lane
///   observed the outcome) does not wait behind a serve burst: mean
///   < burst/4. On the shipped chain the oneshot's waker enqueues the
///   lane task behind the hogs (mean ≈ 1–2 bursts).
/// * `window_lane_wait` (handoff → the lane picked the window up) does not
///   wait behind a serve burst: mean < burst/4 — the in-order lane's
///   pickup is not HOL-blocked by unrelated work.
/// * `tx_queue_wait` (enqueue → drained) does not wait behind a serve
///   burst: mean < burst/2 — the apply pass is not dispatched behind them
///   either.
/// * validity as every row: one tx = one entry, gauge closes, exact-sum.
///
/// The mean-vs-mode of `journal_ring_write` is printed for the record (the
/// audit's phrasing of the finding); the octave buckets make the burst-
/// relative laws above the pinnable form.
/// The contract prices a fixed 2 ms burst against wall time, so the host's
/// CLOCK is part of its venue: while a thermal governor caps the clock the
/// bursts run long and the shares drift past the limit on a healthy
/// product (the batch gate read this contract red three times on
/// `thermald-ng`'s throttle right after its compile burst, the same code
/// green once cool). A draw taken under a capped clock is VOID — logged
/// with the cap and redrawn once the clock is back — never a verdict; a
/// host that stays capped for the whole bound has no verdict and skips
/// (ledgered, the `Capability` class: the host lacks the capacity the
/// contract needs).
const CLOCK_DRAWS_MAX: usize = 3;
const CLOCK_UNCAP_WAIT: Duration = Duration::from_secs(90);

async fn await_uncapped_clock() -> bool {
    let start = Instant::now();
    while squeezefs_testkit::host_clock_throttled() {
        if start.elapsed() >= CLOCK_UNCAP_WAIT {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    true
}

/// The apply pass's queue-wait law is RELATIVE to the same-binary control
/// (the lever off): on a debug build the pass's own service time already
/// holds a share of a 16 × 32 storm's txs at or past one burst (20–28 % on
/// the dev box, with no hog behind them — the batch gate read this
/// contract red at 25–28 % against a fixed 25 % limit on a healthy
/// product, three gates running), so the fixed share cannot separate the
/// mechanism from the venue; the CONTROL's share (the pass dispatched
/// behind the hogs on the shared pool) reads ≈ 100 % with a mean of
/// 1.5 bursts, and the isolated chain must hold the share and the mean to
/// at most HALF of it — the D-2 → C-2 claim in the form the venue cannot
/// blur. The two hop laws stay absolute (µs against a 2 ms burst).
const QUEUE_WAIT_CONTROL_RATIO: f64 = 0.5;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn completion_delivery_is_isolated_from_the_serve_lanes() {
    let _faults = FaultGuard;
    let (control_routed, _control_kv, _control_file) = control_sandbox().await;
    let control = {
        if !await_uncapped_clock().await {
            squeezefs_testkit::skip!(
                Capability,
                "the host's clock stayed capped at {:.2} of hardware max for {:?} — the burst \
                 contract has no verdict on a throttled host",
                squeezefs_testkit::host_clock_cap_ratio().unwrap_or(0.0),
                CLOCK_UNCAP_WAIT
            );
        }
        let hog = LaneHog::start(4, STRUCTURAL_BURST);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let drawn = hop_row(&control_routed, "ctl", COMMITTERS, PER).await;
        drop(hog);
        drawn
    };
    drop(control_routed);
    let (routed, kv, _file) = sandbox().await;
    let mut row = None;
    for draw in 0..CLOCK_DRAWS_MAX {
        if !await_uncapped_clock().await {
            squeezefs_testkit::skip!(
                Capability,
                "the host's clock stayed capped at {:.2} of hardware max for {:?} — the burst \
                 contract has no verdict on a throttled host",
                squeezefs_testkit::host_clock_cap_ratio().unwrap_or(0.0),
                CLOCK_UNCAP_WAIT
            );
        }
        let hog = LaneHog::start(4, STRUCTURAL_BURST);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let drawn = hop_row(&routed, "iso", COMMITTERS, PER).await;
        drop(hog);
        if squeezefs_testkit::host_clock_throttled() {
            eprintln!(
                "C-2 structural row: draw {draw} VOID — the governor capped the clock to {:.2} of \
                 hardware max during the row (queue-wait share {:.0}%); redrawing",
                squeezefs_testkit::host_clock_cap_ratio().unwrap_or(0.0),
                drawn.queue_wait_burst_share * 100.0
            );
            continue;
        }
        row = Some(drawn);
        break;
    }
    let Some(row) = row else {
        squeezefs_testkit::skip!(
            Capability,
            "every one of {CLOCK_DRAWS_MAX} draws ran under a capped clock — no verdict on this host"
        );
    };
    print_header(&format!(
        "C-2 structural row ({} build, 4 x {STRUCTURAL_BURST:?} bursts on sqz-meta):",
        build_label()
    ));
    print_row("control: lane off", &control);
    print_row("serve-lane saturated", &row);
    eprintln!(
        "tx_queue_wait at-or-above-a-burst share: control {:.0}% (mean {:.0} us) vs isolated \
         {:.0}% (mean {:.0} us)",
        control.queue_wait_burst_share * 100.0,
        control.queue_wait_us,
        row.queue_wait_burst_share * 100.0,
        row.queue_wait_us
    );
    assert_row_valid("control: lane off", &control);
    assert_row_valid("serve-lane saturated", &row);
    let burst_us = STRUCTURAL_BURST.as_micros() as f64;
    assert!(
        row.wake_hop_burst_share < BURST_SHARE_LIMIT,
        "the journal write's completion waited behind the serve lanes' bursts: {:.0}% of windows' \
         wake_hop took >= a {burst_us:.0} us burst (mean {:.0} us) — the durability lane learns \
         the completion through a hop onto a lane it shares with the serve plane",
        row.wake_hop_burst_share * 100.0,
        row.wake_hop_us
    );
    assert!(
        row.lane_wait_burst_share < BURST_SHARE_LIMIT,
        "the in-order durability lane's pickup of the next window waited behind the serve \
         lanes' bursts: {:.0}% of windows' window_lane_wait took >= a {burst_us:.0} us burst \
         (mean {:.0} us)",
        row.lane_wait_burst_share * 100.0,
        row.lane_wait_us
    );
    assert!(
        row.queue_wait_burst_share <= control.queue_wait_burst_share * QUEUE_WAIT_CONTROL_RATIO
            && row.queue_wait_us <= control.queue_wait_us * QUEUE_WAIT_CONTROL_RATIO,
        "the apply pass was dispatched behind the serve lanes' bursts: {:.0}% of txs' \
         tx_queue_wait took >= a {burst_us:.0} us burst (mean {:.0} us) against the lane-off \
         control's {:.0}% (mean {:.0} us) — the isolated chain must hold both to at most \
         {QUEUE_WAIT_CONTROL_RATIO} of the control",
        row.queue_wait_burst_share * 100.0,
        row.queue_wait_us,
        control.queue_wait_burst_share * 100.0,
        control.queue_wait_us
    );
    kv.shutdown().await.expect("shutdown");
}
