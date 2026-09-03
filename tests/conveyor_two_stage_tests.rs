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

use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::{
    META_CONVEYOR_DURABILITY_PASSES, META_CONVEYOR_LEADER_PASSES, META_CONVEYOR_WINDOWS_INFLIGHT,
    META_CONVEYOR_WINDOWS_INFLIGHT_HWM, META_KV_JOURNAL_ENTRIES,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use std::sync::atomic::Ordering;
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

/// RAII: the fault shim never leaks across tests.
struct FaultGuard;
impl Drop for FaultGuard {
    fn drop(&mut self) {
        squeezefs::uring_fs::clear_faults();
    }
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
