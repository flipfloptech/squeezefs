//! D-2 — the two-stage commit conveyor (e2e perf audit
//! `docs/design-e2e-perf-audit.md` §3 DLM board #2 ≡ write #5;
//! baseline `.benchmarks/2026-09-02-d1b-publish-plane-batching.md`).
//!
//! **The finding**: the M7 per-volume pass is ONE serialized server that
//! does drain → admission → union leaf locks (RAM apply) → unlock →
//! journal ring write → completed-prefix wait → (strict) barrier →
//! fan-out. The device write's completion sits INSIDE that server's
//! service time, so while a pass waits on the device the next batch's RAM
//! apply cannot start: on the D-1b fleet row the authority's conveyor ran
//! at ρ ≈ 0.97 with `journal_ring_write` 650–672 µs of a 752–776 µs pass
//! (the leaf-lock window was 86–91 µs) and every committer queued
//! (`tx_queue_wait` 764–801 µs).
//!
//! **The instrument** (this file's measurement rows): a file-backed KV
//! sandbox whose journal device latency is a CONTROLLED constant — the
//! `uring_fs::arm_device_latency` seam holds every write / barrier on the
//! volume path for `D` before admitting it, so the device term is `D`
//! exactly and the conveyor's shape is read off `meta_txpass_phase_ns`
//! (sum/count exact — audit A1): throughput, `tx_queue_wait`, the pass's
//! serialized service time, ρ = Σ pass_total ÷ wall, and the D-2 gauge
//! `meta_conveyor_windows_inflight_hwm` (1 = serialized, ≥ 2 =
//! overlapped).
//!
//! Suite runs `--test-threads=1` (process-global stats + fault shim).

use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, KvMetaBackend, TEST_CONVEYOR_HOLD_PRE_DRAIN,
    TEST_CONVEYOR_HOLD_STAGE,
};
use squeezefs::meta_backend::kv::builder::{digest_backend, format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::{
    META_CONVEYOR_DURABILITY_PASSES, META_CONVEYOR_LEADER_PASSES, META_CONVEYOR_WINDOWS_INFLIGHT,
    META_CONVEYOR_WINDOWS_INFLIGHT_HWM, META_KV_JOURNAL_ENTRIES,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 128 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
/// Wide enough that a saturation row never parks on ring admission (the
/// rows below write ≲ 200 KiB of entries) — the conveyor is the only
/// server under test.
const RING_LEN: u64 = 8 * 1024 * 1024;

/// Fresh formatted volume + mounted routed backend (single volume: global
/// inos == local inos).
async fn sandbox() -> (Arc<RoutedMetaBackend>, Arc<KvMetaBackend>, NamedTempFile) {
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
    let kv = KvMetaBackend::open(file.path()).await.expect("open");
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv.clone()]));
    (routed, kv, file)
}

/// Scoped env override (knobs are read at `open`, so the guard wraps the
/// sandbox construction).
struct EnvVarGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
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

const POLL_DEADLINE: Duration = Duration::from_secs(20);

/// Bounded condition poll (the condition IS the contract; never a sleep
/// used as synchronization).
async fn poll_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + POLL_DEADLINE;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("poll_until({what}): the observable never held");
}

/// `lock_phase_ns.leaf_lock_hold`: (sum_ns, count, samples in buckets at
/// or above `floor_us`) — the "never across device I/O" instrument.
fn leaf_hold_snap(floor_us: u64) -> (u64, u64, u64) {
    let j = squeezefs::fuse_client::lock_phase_json();
    let h = &j["leaf_lock_hold"];
    let floor_idx = squeezefs::latency_core::latency_bucket_index(floor_us);
    let above: u64 = squeezefs::latency_core::LATENCY_BUCKET_LABELS
        .iter()
        .enumerate()
        .filter(|(i, _)| *i >= floor_idx)
        .map(|(_, label)| h["buckets"][*label].as_u64().unwrap_or(0))
        .sum();
    (
        h["sum_ns"].as_u64().unwrap_or(0),
        h["count"].as_u64().unwrap_or(0),
        above,
    )
}

/// One `meta_txpass_phase_ns` phase's exact (sum_ns, count) pair.
fn phase(json: &serde_json::Value, name: &str) -> (u64, u64) {
    let p = &json[name];
    (
        p["sum_ns"].as_u64().unwrap_or(0),
        p["count"].as_u64().unwrap_or(0),
    )
}

#[derive(Default, Clone, Copy)]
struct PhaseSnap {
    queue_wait: (u64, u64),
    leaf_locks: (u64, u64),
    ring_write: (u64, u64),
    barrier: (u64, u64),
    pass_total: (u64, u64),
}

fn phase_snap() -> PhaseSnap {
    let j = squeezefs::fuse_client::meta_txpass_phase_json();
    PhaseSnap {
        queue_wait: phase(&j, "tx_queue_wait"),
        leaf_locks: phase(&j, "pass_leaf_locks"),
        ring_write: phase(&j, "journal_ring_write"),
        barrier: phase(&j, "journal_barrier"),
        pass_total: phase(&j, "pass_total"),
    }
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

/// One saturation row's readings.
struct Row {
    txs: u64,
    passes: u64,
    durability_passes: u64,
    entries: u64,
    wall: Duration,
    queue_wait_us: f64,
    leaf_locks_us: f64,
    ring_write_us: f64,
    barrier_us: f64,
    pass_total_us: f64,
    /// Σ pass_total ÷ wall — the serialized server's utilization.
    rho: f64,
    windows_hwm: u64,
}

impl Row {
    fn txs_per_s(&self) -> f64 {
        self.txs as f64 / self.wall.as_secs_f64()
    }
}

/// `committers` concurrent tasks, each committing `per` creates in the
/// root directory (regular files take the parent SHARED — co-queueable,
/// the arrival concurrency the conveyor turns into group size), against a
/// journal device whose write AND barrier latency is `latency`.
async fn saturation_row(
    routed: &Arc<RoutedMetaBackend>,
    vol: &std::path::Path,
    tag: &str,
    committers: usize,
    per: usize,
    latency: Duration,
) -> Row {
    squeezefs::uring_fs::arm_device_latency(vol, latency, latency);
    META_CONVEYOR_WINDOWS_INFLIGHT_HWM.store(0, Ordering::SeqCst);
    let p0 = phase_snap();
    let passes0 = META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed);
    let dpasses0 = META_CONVEYOR_DURABILITY_PASSES.load(Ordering::Relaxed);
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

    let p1 = phase_snap();
    let passes = META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed) - passes0;
    let durability_passes = META_CONVEYOR_DURABILITY_PASSES.load(Ordering::Relaxed) - dpasses0;
    let entries = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - entries0;
    let windows_hwm = META_CONVEYOR_WINDOWS_INFLIGHT_HWM.load(Ordering::Relaxed);
    squeezefs::uring_fs::disarm_device_latency(vol);
    Row {
        txs: (committers * per) as u64,
        passes,
        durability_passes,
        entries,
        wall,
        queue_wait_us: mean_us(p1.queue_wait, p0.queue_wait),
        leaf_locks_us: mean_us(p1.leaf_locks, p0.leaf_locks),
        ring_write_us: mean_us(p1.ring_write, p0.ring_write),
        barrier_us: mean_us(p1.barrier, p0.barrier),
        pass_total_us: mean_us(p1.pass_total, p0.pass_total),
        rho: (p1.pass_total.0 - p0.pass_total.0) as f64 / 1e9 / wall.as_secs_f64(),
        windows_hwm,
    }
}

fn build_label() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

// ===========================================================================
// 1. The measurement rows — the campaign's in-process instrument
// ===========================================================================

/// **The conveyor's shape under saturation at a controlled device
/// latency.** 16 committers × 32 creates each (512 txs, one directory)
/// against a journal device whose write latency is D ∈ {0, 500 µs, 2 ms}
/// on the deferred cadence, plus one strict-cadence row (barrier latency
/// D too).
///
/// What the serialized conveyor reads (the shape this row was written
/// against, dev tip + D-1b): `windows_hwm == 1` on every row; at D > 0
/// `pass_total ≈ leaf_locks + D` and `txs/s ≈ txs-per-pass ÷ (apply + D)`
/// — the device term multiplies straight into the pass, and ρ sits at
/// ≈ 1.0 with every committer queued behind it. The 500 µs row is the
/// field's `journal_ring_write` (650–672 µs on the D-1b authority) made a
/// constant.
///
/// Validity: every create commits, one tx = one checksummed journal entry
/// (the count that never collapses), the gauge closes (no window left in
/// flight).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn measurement_rows_conveyor_saturation_at_controlled_device_latency() {
    let _faults = FaultGuard;
    const COMMITTERS: usize = 16;
    const PER: usize = 32;
    let rows: [(&str, u64, bool); 4] = [
        ("deferred D=0", 0, false),
        ("deferred D=500us", 500, false),
        ("deferred D=2ms", 2000, false),
        ("strict   D=500us", 500, true),
    ];
    println!(
        "D-2 in-process rows ({} build, file-backed KV sandbox, {COMMITTERS} committers x {PER} \
         creates, journal device latency D via uring_fs::arm_device_latency):",
        build_label()
    );
    println!(
        "{:<18}{:>8}{:>7}{:>7}{:>9}{:>10}{:>9}{:>9}{:>9}{:>9}{:>7}{:>6}",
        "row",
        "txs/s",
        "passes",
        "dpass",
        "tx/pass",
        "queue_us",
        "locks_us",
        "write_us",
        "barr_us",
        "pass_us",
        "rho",
        "hwm"
    );
    for (label, d_us, strict) in rows {
        let _strict = strict.then(|| EnvVarGuard::set("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "0"));
        let (routed, kv, file) = sandbox().await;
        let row = saturation_row(
            &routed,
            file.path(),
            &label.replace(' ', "_").replace('=', ""),
            COMMITTERS,
            PER,
            Duration::from_micros(d_us),
        )
        .await;
        println!(
            "{:<18}{:>8.0}{:>7}{:>7}{:>9.1}{:>10.1}{:>9.1}{:>9.1}{:>9.1}{:>9.1}{:>7.3}{:>6}",
            label,
            row.txs_per_s(),
            row.passes,
            row.durability_passes,
            row.txs as f64 / row.passes.max(1) as f64,
            row.queue_wait_us,
            row.leaf_locks_us,
            row.ring_write_us,
            row.barrier_us,
            row.pass_total_us,
            row.rho,
            row.windows_hwm,
        );
        // One tx = one checksummed journal entry; the slack is the volume's
        // own ambient records (claim heartbeat, SMO claims/frees at 64 KiB
        // nodes) — never a per-tx multiple.
        assert!(
            row.entries >= row.txs && row.entries <= row.txs + 16,
            "[{label}] one tx = one checksummed journal entry: {} entries for {} txs",
            row.entries,
            row.txs
        );
        assert!(
            row.passes >= 1 && row.passes <= row.txs,
            "[{label}] pass count sane"
        );
        assert_eq!(
            META_CONVEYOR_WINDOWS_INFLIGHT.load(Ordering::Relaxed),
            0,
            "[{label}] no window left in flight after every committer returned"
        );
        assert!(
            row.windows_hwm >= 1,
            "[{label}] at least one window was in flight"
        );
        kv.shutdown().await.expect("shutdown");
        drop(routed);
    }
}

// ===========================================================================
// 2. The two-stage contracts (red against the serialized conveyor)
// ===========================================================================

/// The journal device latency the shape contracts arm — two orders of
/// magnitude above the apply floor, so the serialized and the overlapped
/// shapes are unmistakable in wall time.
const SLOW_DEVICE: Duration = Duration::from_millis(20);

/// **Windows overlap when the device is slow.** One tx per batch (cap 1),
/// 8 concurrent committers, a 20 ms journal write: the apply stage must
/// keep applying while earlier windows wait on the device — at least 4
/// windows in flight at once, the whole burst done in ≪ 8 × D, the
/// serialized server's service time (`pass_total`) never containing the
/// device wait, and no leaf lock ever held into the device's latency
/// (the 4b law's instrument: `leaf_lock_hold` has no sample at or above
/// D/2).
///
/// The serialized conveyor reads hwm == 1 and wall ≈ 8 × (apply + D).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_overlap_when_the_journal_device_is_slow() {
    let _faults = FaultGuard;
    let _cap = EnvVarGuard::set("SQUEEZEFS_META_COMMIT_BATCH_TXS", "1");
    let (routed, kv, file) = sandbox().await;
    const COMMITTERS: usize = 8;

    squeezefs::uring_fs::arm_device_latency(file.path(), SLOW_DEVICE, Duration::ZERO);
    META_CONVEYOR_WINDOWS_INFLIGHT_HWM.store(0, Ordering::SeqCst);
    let p0 = phase_snap();
    let hold0 = leaf_hold_snap(SLOW_DEVICE.as_micros() as u64 / 2);

    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for c in 0..COMMITTERS {
        let routed = routed.clone();
        tasks.push(tokio::spawn(async move {
            routed
                .create(1, &format!("overlap_{c}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .expect("create")
        }));
    }
    for t in tasks {
        t.await.expect("committer task");
    }
    let wall = t0.elapsed();
    let p1 = phase_snap();
    let hold1 = leaf_hold_snap(SLOW_DEVICE.as_micros() as u64 / 2);
    let hwm = META_CONVEYOR_WINDOWS_INFLIGHT_HWM.load(Ordering::Relaxed);
    let passes = p1.pass_total.1 - p0.pass_total.1;
    let pass_us = mean_us(p1.pass_total, p0.pass_total);
    println!(
        "overlap row: {COMMITTERS} committers, D = {:?}: wall {:.1} ms, passes {passes}, \
         pass_total mean {pass_us:.0} us, windows hwm {hwm}, leaf-hold mean {:.1} us over {} \
         holds ({} at/above D/2)",
        SLOW_DEVICE,
        wall.as_secs_f64() * 1e3,
        (hold1.0 - hold0.0) as f64 / (hold1.1 - hold0.1).max(1) as f64 / 1e3,
        hold1.1 - hold0.1,
        hold1.2 - hold0.2,
    );

    assert!(
        hwm >= 4,
        "≥ 4 windows must be in flight at once while the device is slow (hwm {hwm}): the \
         apply stage waited on the device"
    );
    assert!(
        wall < SLOW_DEVICE * 3,
        "8 one-tx windows against a {:?} device must complete in ≪ 8 × D (wall {:?}): the \
         device write is serialized inside the pass",
        SLOW_DEVICE,
        wall
    );
    assert!(
        pass_us < SLOW_DEVICE.as_micros() as f64 / 2.0,
        "the serialized server's service time (pass_total mean {pass_us:.0} us) contains the \
         device wait ({:?})",
        SLOW_DEVICE
    );
    assert_eq!(
        hold1.2 - hold0.2,
        0,
        "a leaf lock was held into the device's latency window (samples at/above D/2)"
    );
    assert_eq!(META_CONVEYOR_WINDOWS_INFLIGHT.load(Ordering::Relaxed), 0);
    kv.shutdown().await.expect("shutdown");
}

/// **Throughput tracks the committer population, not one batch per
/// device period.** A closed loop of 16 committers × 8 creates against a
/// 5 ms journal write: the serialized conveyor settles into two ping-pong
/// groups of 8 — every device period commits ONE batch of C/2, so txs/s ×
/// (D + apply) ≈ C/2. With the device wait off the serialized server every
/// committer is in flight during every device period: txs/s × (D + apply)
/// → C. Contract: ≥ 0.75 C (the serialized shape reads 0.5 C by
/// construction — measured 0.49–0.50 across the rows above).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn throughput_tracks_the_committer_population_when_the_device_is_slow() {
    let _faults = FaultGuard;
    let (routed, kv, file) = sandbox().await;
    const COMMITTERS: usize = 16;
    const PER: usize = 8;
    let d = Duration::from_millis(5);
    let row = saturation_row(&routed, file.path(), "tput", COMMITTERS, PER, d).await;
    let in_flight_per_period = row.txs_per_s() * (d.as_secs_f64() + row.leaf_locks_us / 1e6);
    println!(
        "throughput row: {COMMITTERS} x {PER} at D = {d:?}: {:.0} tx/s, {:.2} txs per device \
         period (C = {COMMITTERS}), pass_total mean {:.0} us, queue_wait mean {:.0} us, hwm {}",
        row.txs_per_s(),
        in_flight_per_period,
        row.pass_total_us,
        row.queue_wait_us,
        row.windows_hwm
    );
    assert!(
        row.entries >= row.txs && row.entries <= row.txs + 16,
        "one tx = one entry"
    );
    assert!(
        in_flight_per_period >= 0.75 * COMMITTERS as f64,
        "only {in_flight_per_period:.2} txs complete per device period for {COMMITTERS} \
         committers: the conveyor commits one batch per device round trip"
    );
    assert!(
        row.pass_total_us < d.as_micros() as f64 / 2.0,
        "pass_total mean {:.0} us contains the device wait",
        row.pass_total_us
    );
    kv.shutdown().await.expect("shutdown");
}

/// **Acks follow journal order across overlapped windows.** Eight
/// one-tx windows enqueued in a known order (the pass held pre-drain, one
/// arrival admitted at a time) with the FIRST window's ring write parked
/// (`arm_write_stall` on its reserved range), then released: the seven
/// later windows are applied and submitted — their writes LAND — yet none
/// of them may be answered while their predecessor's entry has not: an
/// entry is chain-reachable only through the entries before it in the
/// ring, so acking a later window ahead of an earlier one would ack a tx
/// a crash could not replay. Releasing the parked write answers all
/// eight; their inos (allocated at staging, before enqueue) are monotone
/// in enqueue order — the order that is drain order (the conveyor core's
/// FIFO) and therefore journal-seq order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acks_follow_journal_order_across_overlapped_windows() {
    let _faults = FaultGuard;
    let _cap = EnvVarGuard::set("SQUEEZEFS_META_COMMIT_BATCH_TXS", "1");
    let (routed, kv, file) = sandbox().await;
    const N: usize = 8;
    META_CONVEYOR_WINDOWS_INFLIGHT_HWM.store(0, Ordering::SeqCst);

    // Park the ring write of whatever entry is reserved NEXT — window 0's.
    let head = kv.journal_ring().core().head();
    let mut arrived = squeezefs::uring_fs::arm_write_stall(
        file.path(),
        kv.journal_ring().physical_offset_of(head),
        8,
    );

    let acked = Arc::new(AtomicU64::new(0));
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);
    let mut tasks = Vec::new();
    for i in 0..N {
        let routed = routed.clone();
        let acked = acked.clone();
        tasks.push(tokio::spawn(async move {
            let ino = routed
                .create(1, &format!("order_{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .expect("create")
                .ino;
            acked.fetch_add(1, Ordering::SeqCst);
            ino
        }));
        let want = i + 1;
        poll_until("committer enqueued", || kv.conveyor_pending_len() >= want).await;
    }
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();

    // Window 0's write reached the stall; the seven behind it are applied
    // and submitted (all eight in flight) — and none is answered.
    tokio::time::timeout(POLL_DEADLINE, arrived.recv())
        .await
        .expect("window 0's ring write must reach the stall")
        .expect("stall arrival channel");
    poll_until("every later window applied and handed off", || {
        META_CONVEYOR_WINDOWS_INFLIGHT.load(Ordering::Relaxed) >= N as u64
    })
    .await;
    assert_eq!(
        acked.load(Ordering::SeqCst),
        0,
        "a later window was acked while its predecessor's entry had not landed — an ack \
         left journal order"
    );
    squeezefs::uring_fs::release_write_stall(file.path());

    let mut inos = Vec::with_capacity(N);
    for t in tasks {
        inos.push(t.await.expect("committer task"));
    }
    let hwm = META_CONVEYOR_WINDOWS_INFLIGHT_HWM.load(Ordering::Relaxed);
    println!("order row: inos by enqueue order {inos:?}, hwm {hwm}");
    assert_eq!(acked.load(Ordering::SeqCst), N as u64);
    for w in inos.windows(2) {
        assert!(w[0] < w[1], "inos are monotone in enqueue order");
    }
    assert!(
        hwm >= N as u64,
        "the eight released windows never overlapped (hwm {hwm})"
    );
    assert_eq!(META_CONVEYOR_WINDOWS_INFLIGHT.load(Ordering::Relaxed), 0);
    kv.shutdown().await.expect("shutdown");
}

/// **An ack never precedes its barrier (strict cadence).** With the
/// volume's barrier PARKED (`arm_barrier_stall`), committers whose entries
/// have fully landed in page cache (`completed_upto == head`) must still
/// be unanswered; releasing the barrier answers every one of them. The
/// durability stage's ack is gated on the barrier, never on the write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ack_never_precedes_its_barrier_on_the_strict_cadence() {
    let _faults = FaultGuard;
    let _strict = EnvVarGuard::set("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "0");
    let _cap = EnvVarGuard::set("SQUEEZEFS_META_COMMIT_BATCH_TXS", "1");
    let (routed, kv, file) = sandbox().await;
    const N: usize = 4;
    let mut arrived = squeezefs::uring_fs::arm_barrier_stall(file.path());
    let acked = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for i in 0..N {
        let routed = routed.clone();
        let acked = acked.clone();
        tasks.push(tokio::spawn(async move {
            routed
                .create(1, &format!("barrier_{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .expect("create");
            acked.fetch_add(1, Ordering::SeqCst);
        }));
    }
    // A barrier reached the stall: at least one window is written and
    // parked on durability.
    tokio::time::timeout(POLL_DEADLINE, arrived.recv())
        .await
        .expect("a barrier must reach the stall")
        .expect("stall arrival channel");
    // The apply stage is not behind the parked barrier: every committer's
    // window gets applied and submitted while the lane waits — the
    // serialized conveyor could hold only one here (its pass IS the
    // parked barrier).
    poll_until("every window applied and handed off", || {
        META_CONVEYOR_WINDOWS_INFLIGHT.load(Ordering::Relaxed) >= N as u64
    })
    .await;
    assert_eq!(
        acked.load(Ordering::SeqCst),
        0,
        "a committer was acked while its barrier was still parked"
    );
    squeezefs::uring_fs::release_barrier_stall(file.path());
    for t in tasks {
        t.await.expect("committer task");
    }
    assert_eq!(acked.load(Ordering::SeqCst), N as u64);
    assert_eq!(META_CONVEYOR_WINDOWS_INFLIGHT.load(Ordering::Relaxed), 0);
    kv.shutdown().await.expect("shutdown");
}

/// **A barrier failure fail-stops from the durability stage exactly as it
/// does today** — with windows in flight. Strict cadence, a slow device
/// (so six one-tx windows overlap), the reservation-conflict errno at the
/// barrier: every committer is answered with an error (none stranded
/// behind a fenced lane), the volume latches `failed`, the fence counter
/// trips, the in-flight gauge closes, and later mutations refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn barrier_failure_fail_stops_from_the_durability_stage_with_windows_in_flight() {
    let _faults = FaultGuard;
    let _strict = EnvVarGuard::set("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "0");
    let _cap = EnvVarGuard::set("SQUEEZEFS_META_COMMIT_BATCH_TXS", "1");
    let (routed, kv, file) = sandbox().await;
    const N: usize = 6;
    squeezefs::uring_fs::arm_device_latency(file.path(), SLOW_DEVICE, Duration::ZERO);
    squeezefs::uring_fs::arm_barrier_error(file.path(), libc::EBADE);
    assert_eq!(kv.writer_guard_fenced(), 0);

    let mut tasks = Vec::new();
    for i in 0..N {
        let routed = routed.clone();
        tasks.push(tokio::spawn(async move {
            routed
                .create(1, &format!("fenced_{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
        }));
    }
    let mut errs = 0usize;
    for t in tokio::time::timeout(POLL_DEADLINE, futures::future::join_all(tasks))
        .await
        .expect("every fenced committer must be answered (none stranded)")
    {
        if t.expect("committer task").is_err() {
            errs += 1;
        }
    }
    assert_eq!(errs, N, "every commit behind a fenced barrier fails");
    assert!(
        kv.is_failed(),
        "reservation conflict at the barrier latches failed"
    );
    assert!(kv.writer_guard_fenced() >= 1, "the fence counter trips");
    assert_eq!(
        META_CONVEYOR_WINDOWS_INFLIGHT.load(Ordering::Relaxed),
        0,
        "no window left in flight after the fail-stop"
    );
    squeezefs::uring_fs::clear_faults();
    assert!(
        routed
            .create(1, "after_fence", libc::S_IFREG | 0o644, 0, 0)
            .await
            .is_err(),
        "the failed latch holds until remount"
    );
}

// ===========================================================================
// 3. Ring pressure under a sustained storm (finding 49 — the ring-capacity
//    dimension the two-stage model has no axis for)
// ===========================================================================

/// The storm every finding-49 contract runs: `committers` files, each
/// committer setting 4 KiB xattr values on its own file (one entry = one
/// writeback-threshold crossing — every pass enqueues threshold
/// maintenance) against a device whose writes and barriers pay
/// `arm_device_latency` (every threshold append / SMO image write / barrier
/// pays it — the checkpoint task's per-item service time is the seam's).
/// Returns the committer failures (empty = every commit succeeded).
async fn xattr_storm(
    routed: &Arc<RoutedMetaBackend>,
    inos: &[u64],
    per: usize,
    value_len: usize,
) -> Vec<String> {
    let value = vec![0xA5u8; value_len];
    let mut tasks = Vec::with_capacity(inos.len());
    for (c, ino) in inos.iter().enumerate() {
        let routed = routed.clone();
        let value = value.clone();
        let ino = *ino;
        tasks.push(tokio::spawn(async move {
            for i in 0..per {
                routed
                    .setxattr(ino, &format!("user.k{}", i % 16), &value)
                    .await
                    .map_err(|e| format!("committer {c} entry {i}: {e}"))?;
            }
            Ok::<(), String>(())
        }));
    }
    let mut failures = Vec::new();
    for t in tasks {
        if let Err(e) = t.await.expect("committer task") {
            failures.push(e);
        }
    }
    failures
}

async fn storm_sandbox(
    ring: u64,
    committers: usize,
) -> (
    Arc<RoutedMetaBackend>,
    Arc<KvMetaBackend>,
    NamedTempFile,
    Vec<u64>,
) {
    const VOL: u64 = 128 * 1024 * 1024;
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL).unwrap();
    format_v3(
        file.path(),
        VOL,
        &FormatV3Options {
            // The shipped node size: leaves fill slowly, so the storm is
            // threshold APPENDS (one per pass per touched leaf), not SMOs —
            // the checkpoint-reserve exhaustion arm (which forces a cycle
            // of its own) stays out of the picture and the cadence is the
            // only reclaimer under test.
            node_size: 256 * 1024,
            journal_len_override: Some(ring),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let kv = KvMetaBackend::open(file.path()).await.expect("open");
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv.clone()]));
    // One file per committer: the xattr writes take the ino exclusively,
    // so the committers never co-queue on one key and each pass carries
    // up to `committers` independent 4 KiB entries.
    let mut inos = Vec::with_capacity(committers);
    for c in 0..committers {
        inos.push(
            routed
                .create(1, &format!("storm_{c}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .expect("create")
                .ino,
        );
    }
    (routed, kv, file, inos)
}

/// **The checkpoint cadence runs at its period under a sustained
/// threshold-maintenance storm.** §4.6: a checkpoint cycle is due every
/// ≤ 1 s (`CHECKPOINT_MAX_AGE_MS`), on ring pressure, or on the dirty
/// cap — and the cadence tick is the ONLY path to a ring-pressure cycle,
/// i.e. the only thing that ever advances `reusable_upto` for parked
/// committers. Under a storm whose every pass crosses the writeback
/// threshold, the maintenance wake is never silent; the cadence must
/// still fire. Contract: over a ≥ 4 s storm on a ring it never fills
/// (32 MiB — no pressure, no parking, so the quiesce-at-park accident
/// below cannot stand in for the cadence), `meta_kv_checkpoints` advances
/// at least once per two seconds of storm.
///
/// RED on the shipped task: its select polls the maintenance wake BEFORE
/// the deadline and its drain pops the queue until EMPTY — a storm that
/// keeps the queue full (a 5 ms device makes every pop slower than the
/// arrivals) re-arms the wake on every return, so the deadline arm is
/// never reached and the checkpoint count stays flat for the whole storm
/// (finding 49: the `kv_scale_tests` million-entry storm under the
/// all-features gate held it flat for 190 s, filled a 32 MiB ring, and the
/// parked pass escalated to fail-stop at 3 × `SQUEEZEFS_TIMEOUT`).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn checkpoint_cadence_survives_a_sustained_maintenance_storm() {
    let _faults = FaultGuard;
    const COMMITTERS: usize = 16;
    const PER: usize = 300;
    const VALUE: usize = 4096;
    let (routed, kv, file, inos) = storm_sandbox(32 * 1024 * 1024, COMMITTERS).await;
    // 15 ms: slow enough that 19.7 MB of entries take > 4 s (a 32 MiB ring
    // never fills), and every maintenance pop is slower than the arrivals.
    let latency = Duration::from_millis(15);
    squeezefs::uring_fs::arm_device_latency(file.path(), latency, latency);
    let checkpoints0 = squeezefs::meta_backend::kv::META_KV_CHECKPOINTS.load(Ordering::Relaxed);
    let t0 = Instant::now();
    let failures = xattr_storm(&routed, &inos, PER, VALUE).await;
    let wall = t0.elapsed();
    let checkpoints =
        squeezefs::meta_backend::kv::META_KV_CHECKPOINTS.load(Ordering::Relaxed) - checkpoints0;
    squeezefs::uring_fs::disarm_device_latency(file.path());
    println!(
        "cadence row: {COMMITTERS} x {PER} x {VALUE} B entries against a {latency:?} device on a \
         32 MiB ring: wall {wall:.1?}, ring stalls {}, checkpoints under the storm {checkpoints}, \
         failures {}",
        kv.journal_full_stalls(),
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "{:?}",
        &failures[..failures.len().min(3)]
    );
    assert_eq!(
        kv.journal_full_stalls(),
        0,
        "the cadence row must not reach ring pressure (parking quiesces the storm and lets the \
         cadence through by accident — the row would not measure the cadence)"
    );
    assert!(
        wall >= Duration::from_secs(4),
        "the storm must sustain ≥ 4 s for the cadence contract to read (wall {wall:?}) — widen PER"
    );
    let floor = (wall.as_secs() / 2).max(1);
    assert!(
        checkpoints >= floor,
        "the checkpoint cadence was starved by the maintenance storm: {checkpoints} cycles over a \
         {wall:.1?} storm (§4.6: due every ≤ 1 s; floor {floor})"
    );
    kv.shutdown().await.expect("shutdown");
}

/// **A committer parked for ring space is RELEASED by the checkpoint
/// reclaiming the ring — never fail-stopped.** The §4.4 pt 5 law as the
/// M1–M12 D1.b watchdog states it: ring-admission parking escalates
/// through the fail-stop lattice only when the checkpoint CANNOT make
/// progress; under a legitimate storm the checkpoint task truncates the
/// ring as windows become durable and parked committers proceed. The
/// same storm on an 8 MiB ring (the format minimum), ≈ 2.3 rings of
/// entries, `SQUEEZEFS_TIMEOUT=1` (escalation at 3 × 1 s of continuous
/// parking). Laws: every commit succeeds and the volume never latches
/// `failed`; the storm DID park (`journal_full_stalls` ≥ 1 — a row that
/// never reached ring pressure did not exercise the mechanism); the
/// checkpoint reclaimed WHILE committers were parked (`reusable_upto`
/// observed moving off its start before the storm ended).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn parked_committers_are_released_by_the_checkpoint_under_a_sustained_storm() {
    let _faults = FaultGuard;
    let _to = EnvVarGuard::set("SQUEEZEFS_TIMEOUT", "1");
    const RING: u64 = 8 * 1024 * 1024;
    const COMMITTERS: usize = 16;
    const PER: usize = 300;
    const VALUE: usize = 4096;
    let (routed, kv, file, inos) = storm_sandbox(RING, COMMITTERS).await;
    let latency = Duration::from_millis(5);
    squeezefs::uring_fs::arm_device_latency(file.path(), latency, latency);
    let checkpoints0 = squeezefs::meta_backend::kv::META_KV_CHECKPOINTS.load(Ordering::Relaxed);
    let reusable0 = kv.journal_ring().core().reusable_upto();

    // The observer: samples `reusable_upto` while the storm runs, so a
    // reclaim DURING the storm (not after it) is what the law reads.
    let reclaimed_under_storm = Arc::new(AtomicU64::new(0));
    let storm_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer = {
        let kv = kv.clone();
        let reclaimed = reclaimed_under_storm.clone();
        let done = storm_done.clone();
        tokio::spawn(async move {
            while !done.load(Ordering::SeqCst) {
                let now = kv.journal_ring().core().reusable_upto();
                if now > reusable0 {
                    reclaimed.fetch_max(now, Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };

    let t0 = Instant::now();
    let failures = xattr_storm(&routed, &inos, PER, VALUE).await;
    storm_done.store(true, Ordering::SeqCst);
    observer.await.expect("observer");
    let wall = t0.elapsed();
    let checkpoints =
        squeezefs::meta_backend::kv::META_KV_CHECKPOINTS.load(Ordering::Relaxed) - checkpoints0;
    let stalls = kv.journal_full_stalls();
    let reclaimed = reclaimed_under_storm.load(Ordering::SeqCst);
    println!(
        "storm row: {COMMITTERS} x {PER} x {VALUE} B entries ({:.1} rings) against a {latency:?} \
         device on an {} MiB ring: wall {wall:.1?}, ring stalls {stalls}, checkpoints under the \
         storm {checkpoints}, reusable_upto {reusable0} -> {reclaimed} (observed while parked), \
         failed = {}, failures = {}",
        (COMMITTERS * PER * VALUE) as f64 / RING as f64,
        RING >> 20,
        kv.is_failed(),
        failures.len()
    );
    squeezefs::uring_fs::disarm_device_latency(file.path());
    assert!(
        failures.is_empty(),
        "committers were aborted under a legitimate storm (the ring must be reclaimed, never \
         fail-stopped): {:?}",
        &failures[..failures.len().min(3)]
    );
    assert!(
        !kv.is_failed(),
        "the volume latched `failed` under a storm the checkpoint could have reclaimed"
    );
    assert!(
        stalls >= 1,
        "the storm never parked for ring space — the row did not reach ring pressure"
    );
    assert!(
        checkpoints >= 1 && reclaimed > reusable0,
        "the checkpoint did not reclaim the ring while committers were parked (checkpoints \
         {checkpoints}, reusable_upto {reusable0} -> {reclaimed})"
    );
    kv.shutdown().await.expect("shutdown");
}

// ===========================================================================
// 4. Acked ⇒ replayed under kill -9 while windows overlap (the
//    write_commit_crash_tests idiom)
// ===========================================================================

fn ledger_append(path: &std::path::Path, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open ledger");
    f.write_all(format!("{line}\n").as_bytes())
        .expect("append ledger");
    f.sync_data().expect("fsync ledger");
}

/// Child branch: 8 committers stream one-tx windows against a 3 ms journal
/// device (so several windows are always in flight) and ledger each ack
/// AFTER `create` returns. The parent kills this process mid-stream.
#[test]
fn d2_crash_child_entry() {
    if std::env::var("SQUEEZEFS_D2_CRASH_CHILD").is_err() {
        return;
    }
    let vol = std::path::PathBuf::from(std::env::var("SQUEEZEFS_D2_VOL").unwrap());
    let ledger = std::path::PathBuf::from(std::env::var("SQUEEZEFS_D2_LEDGER").unwrap());
    std::env::set_var("SQUEEZEFS_META_COMMIT_BATCH_TXS", "1");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let kv = KvMetaBackend::open(&vol).await.unwrap();
        let routed = Arc::new(RoutedMetaBackend::new(vec![kv.clone()]));
        squeezefs::uring_fs::arm_device_latency(&vol, Duration::from_millis(3), Duration::ZERO);
        let mut tasks = Vec::new();
        for c in 0..8usize {
            let routed = routed.clone();
            let ledger = ledger.clone();
            tasks.push(tokio::spawn(async move {
                // The child is killed -9 mid-storm; the bound is the lint's.
                for i in 0..=u64::MAX {
                    let name = format!("k{c}_{i}");
                    routed
                        .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
                        .await
                        .unwrap();
                    ledger_append(&ledger, &format!("ack {name}"));
                }
            }));
        }
        futures::future::join_all(tasks).await;
    });
}

/// **Acked ⇒ replayed while windows overlap.** Kill -9 lands with several
/// windows in flight (applied, submitted, some landed, some still on the
/// device's latency lane); after remount every ledger-acked name must
/// resolve (an ack is issued only after the entry's bytes landed — the D0
/// law), the mount never fails loud, and replay is idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acked_commits_survive_kill9_while_windows_overlap() {
    let rounds: u32 = std::env::var("SQUEEZEFS_D2_CRASH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let exe = std::env::current_exe().expect("test binary path");
    for round in 0..rounds {
        let dir = tempfile::tempdir().unwrap();
        let vol = dir.path().join("d2.v3.meta");
        let ledger = dir.path().join("ledger.log");
        {
            let f = std::fs::File::create(&vol).unwrap();
            f.set_len(VOL_LEN).unwrap();
            format_v3(
                &vol,
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
            .unwrap();
        }
        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "d2_crash_child_entry",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("SQUEEZEFS_D2_CRASH_CHILD", "1")
            .env("SQUEEZEFS_D2_VOL", &vol)
            .env("SQUEEZEFS_D2_LEDGER", &ledger)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn d2 crash child");

        // Anchor the kill on a MEASURED ack count (never wall-clock).
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut anchored = false;
        while Instant::now() < deadline {
            let acks = std::fs::read_to_string(&ledger)
                .map(|s| s.lines().filter(|l| l.starts_with("ack ")).count())
                .unwrap_or(0);
            if acks >= 40 {
                anchored = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            anchored,
            "round {round}: the child never reached 40 acks in 60 s"
        );
        let jitter: u64 = {
            use rand::Rng;
            rand::thread_rng().gen_range(1..=30)
        };
        tokio::time::sleep(Duration::from_millis(jitter)).await;
        child.kill().expect("SIGKILL d2 child");
        let _ = child.wait();

        let acked: Vec<String> = std::fs::read_to_string(&ledger)
            .unwrap()
            .lines()
            .filter_map(|l| l.strip_prefix("ack ").map(str::to_string))
            .collect();
        let m1 = KvMetaBackend::open(&vol)
            .await
            .unwrap_or_else(|e| panic!("round {round}: remount failed loud after kill-9: {e}"));
        let replay = m1.replay_stats();
        eprintln!(
            "[d2-kill9 round {round}] {} acked; replay {} entries, {} dropped-torn",
            acked.len(),
            replay.entries,
            replay.dropped_torn
        );
        for name in &acked {
            assert!(
                Metadata::lookup(m1.as_ref(), 1, name).await.is_ok(),
                "round {round}: ACKED create {name} lost after kill-9 — acked before its entry \
                 landed"
            );
        }
        let d1 = digest_backend(&m1).await.unwrap();
        m1.shutdown().await.unwrap();
        drop(m1);
        let m2 = KvMetaBackend::open(&vol).await.unwrap();
        assert_eq!(
            m2.replay_stats().entries,
            0,
            "clean shutdown ⇒ empty replay window"
        );
        assert_eq!(digest_backend(&m2).await.unwrap(), d1, "replay idempotence");
        m2.shutdown().await.unwrap();
    }
}
