//! PR VL4b — balance-aware write placement, red-first
//! (docs/design-volume-lifecycle.md §5.9, KD-16, gate G-VL-8):
//!
//! - **The penalty formula (§5.9)**: `balance_penalty = clamp(k ×
//!   (fill_ratio − set_mean_fill), 0, 300)` with `k = 1000` — property
//!   tests pin the clamp bounds, the 10-pp-costs-100-points slope, and
//!   the KD-16 invariant that balance NEVER outvotes health (the 300-of-
//!   1000 cap keeps a healthy-but-full volume preferable to an
//!   unhealthy-but-empty one; failover semantics untouched).
//! - **`PlacementTable`**: an ArcSwap'd immutable snapshot {id, weight
//!   (health_effective), fill_ratio, eligible} + the 90 %-of-max band,
//!   refreshed by the health worker cadence AND immediately on volume
//!   state changes / health overrides / registration / retire.
//! - **`get_active_backend` over the table**: lock-free ArcSwap load,
//!   same 90 %-band + atomic round-robin — the per-write
//!   `fs::metadata`+seek syscall pair is RETIRED (the only syscall site
//!   left is the refresh; pinned by the zero-rebuild-per-pick test and
//!   structurally by `PlacementTable::pick` taking only the snapshot).
//! - **Eligibility**: draining/retired/disabled volumes excluded (§5.4
//!   carried into the table); the `unhealthy_backends` fail-stop mark
//!   stays authoritative and INSTANT (absolute gate, before scoring).
//! - **Fill convergence (G-VL-8 at fixture scale)**: an imbalanced
//!   3-backend set (one ~70 % full, two empty) under a sustained write
//!   workload places ≥ 60 % of new blocks on the emptiest band while
//!   spread > 10 pp and drives `backend_fill_spread` monotonically-
//!   modulo-noise below 10 pp.
//! - **W1 non-interaction**: placement is NEW-allocations-only — a
//!   drain-free steady overwrite workload patches in place with zero
//!   `patch_ineligible_*` delta and an unchanged block map, even while
//!   the table prefers the OTHER backend.
//! - **Stats (§10)**: per-backend `backend_placement_picks` /
//!   `backend_placement_weight` / `backend_fill_ratio`, set-level
//!   `backend_fill_spread`, and `placement_table_refreshes` ride the
//!   stats inode.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use proptest::prelude::*;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{set_patch_max_bytes, SqueezefsFilesystem, METRICS, STATS_INODE};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{
    balance_penalty, health_effective, BackendRouter, DataRouter, StorageBackend,
    BALANCE_PENALTY_CAP,
};
use squeezefs::{DataVolumeRecord, FormatConfig, VOL_STATE_ACTIVE, VOL_STATE_DRAINING};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::TempDir;

/// Sandbox block size (the extent_patch_tests quantum: the placement and
/// patch anatomy are block-size-relative; every offset in the W1 leg
/// speaks in the 4096-byte LBA quantum).
const BS: u64 = 64 * 1024;

/// Process-global METRICS deltas: serialize tests (the cargo gate runs
/// `--test-threads=1`; this keeps the suite order-robust on its own too).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

// ---------------------------------------------------------------------------
// §5.9 penalty formula: pure-function property tests
// ---------------------------------------------------------------------------

proptest! {
    /// clamp bounds: 0 ≤ penalty ≤ 300 for every census; at-or-below the
    /// set mean costs nothing.
    #[test]
    fn penalty_clamp_bounds(fill in 0.0f64..=1.0, mean in 0.0f64..=1.0) {
        let p = balance_penalty(fill, mean);
        prop_assert!(p <= BALANCE_PENALTY_CAP, "penalty {p} above the 300 cap");
        if fill <= mean {
            prop_assert_eq!(p, 0, "at/below the set mean the penalty is zero");
        }
    }

    /// Monotone: more above-mean fill never costs less.
    #[test]
    fn penalty_monotone_in_fill(
        f1 in 0.0f64..=1.0,
        f2 in 0.0f64..=1.0,
        mean in 0.0f64..=1.0,
    ) {
        let (lo, hi) = if f1 <= f2 { (f1, f2) } else { (f2, f1) };
        prop_assert!(balance_penalty(lo, mean) <= balance_penalty(hi, mean));
    }

    /// KD-16: balance never outvotes health. `health_effective` never
    /// drops more than the 300-point cap below the device health, so a
    /// healthy-but-full backend (device health 1000, any fill) can never
    /// lose to a merely-adequate empty one at 700 — and a genuinely
    /// degraded backend (≤ 699) still loses to the merely-full one.
    #[test]
    fn balance_never_outvotes_health(
        health in 0u32..=1000,
        fill in 0.0f64..=1.0,
        mean in 0.0f64..=1.0,
    ) {
        let eff = health_effective(health, fill, mean);
        prop_assert!(eff <= health, "penalty can only subtract");
        prop_assert!(
            eff >= health.saturating_sub(BALANCE_PENALTY_CAP),
            "eff {eff} fell more than the cap below health {health}"
        );
        // The concrete §5.9 invariant, ∀ mean: health-1000 fully-full
        // never scores below health-700 fully-empty.
        prop_assert!(
            health_effective(1000, 1.0, mean) >= health_effective(700, 0.0, mean),
            "a healthy-but-full volume lost to an equal-or-worse empty one"
        );
    }
}

/// The exact §5.9 constants: k = 1000 (10 pp above the set mean costs
/// 100 points), cap 300.
#[test]
fn penalty_slope_and_cap_exact() {
    assert_eq!(
        balance_penalty(0.5, 0.4),
        100,
        "10 pp above mean = 100 points"
    );
    assert_eq!(
        balance_penalty(0.6, 0.4),
        200,
        "20 pp above mean = 200 points"
    );
    assert_eq!(balance_penalty(0.9, 0.4), 300, "capped at 300");
    assert_eq!(balance_penalty(1.0, 0.0), 300, "worst case still capped");
    assert_eq!(balance_penalty(0.25, 0.25), 0, "at the mean costs nothing");
    assert_eq!(
        health_effective(1000, 1.0, 0.0),
        700,
        "the §5.9 anchor: device health 1000 fully full = 700 effective"
    );
    assert_eq!(
        health_effective(200, 1.0, 0.0),
        0,
        "health_effective saturates at zero"
    );
}

// ---------------------------------------------------------------------------
// Router fixtures (bare BackendRouter over file-backed devices)
// ---------------------------------------------------------------------------

struct RouterFx {
    router: Arc<BackendRouter>,
    _dir: TempDir,
}

fn make_dev_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

/// A bare router with `vols` named backends: per-volume allocator +
/// device, capacity bounded to `cap_blocks` ALLOCATOR CHUNKS (the fill
/// census quantum), `seed_used` chunks pre-allocated (the imbalance
/// seed). The default slot is a dummy that never joins placement (named
/// registrations exist).
async fn router_with(vols: &[(&str, u64, u64)]) -> RouterFx {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let dir = tempfile::tempdir().unwrap();
    let dlm = DlmClient::new("local").unwrap();

    let default_path = make_dev_file(dir.path(), "default.img", 16 * 1024 * 1024);
    let default_dev = Arc::new(NvmeBlockDev::new(default_path.to_str().unwrap()));
    let default_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "placement_default")
            .await
            .unwrap(),
    );
    let router = Arc::new(BackendRouter::new(
        default_alloc,
        default_dev,
        Arc::new(AtomicU64::new(BS)),
    ));

    for &(id, cap_blocks, seed_used) in vols {
        let dev_path = make_dev_file(dir.path(), &format!("{id}.img"), 16 * 1024 * 1024);
        let dev = Arc::new(NvmeBlockDev::new(dev_path.to_str().unwrap()));
        let alloc = Arc::new(
            BlockAllocator::new(dlm.meta_client().clone(), id)
                .await
                .unwrap(),
        );
        alloc.set_capacity_bytes(cap_blocks * alloc.chunk_size());
        for _ in 0..seed_used {
            alloc.allocate_block().await.unwrap();
        }
        router
            .publish_backend(
                id,
                Arc::new(StorageBackend {
                    device: dev,
                    block_allocator: alloc,
                }),
            )
            .unwrap();
    }
    router.refresh_placement_table();
    RouterFx { router, _dir: dir }
}

fn fill_of(router: &BackendRouter, id: &str) -> f64 {
    let be = router.backends.get(id).unwrap().value().clone();
    let alloc = &be.block_allocator;
    let cap = alloc.capacity_bytes();
    (alloc.get_used_blocks().saturating_mul(alloc.chunk_size())) as f64 / cap as f64
}

// ---------------------------------------------------------------------------
// Band + round-robin over a synthetic imbalanced table
// ---------------------------------------------------------------------------

/// A ~70 %-full backend falls out of the 90 %-of-max band (health 300 −
/// penalty 300 = weight 0); a mildly-filled one stays IN band; the
/// atomic round-robin spreads evenly across the band.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn band_and_round_robin_over_imbalanced_table() {
    let _g = serial().await;
    let fx = router_with(&[("oss1", 1000, 700), ("oss2", 1000, 50), ("oss3", 1000, 0)]).await;
    let table = fx.router.placement_snapshot();

    let mut band = table.band_ids();
    band.sort();
    assert_eq!(
        band,
        vec!["oss2".to_string(), "oss3".to_string()],
        "the near-full volume must fall out of the 90%-of-max band; the \
         mildly-filled one stays in"
    );

    // Per-backend gauges: the snapshot carries weight/fill/eligible rows.
    let row = |id: &str| {
        table
            .rows
            .iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("row {id} missing from the table"))
    };
    assert!(
        row("oss1").eligible,
        "near-full is still ELIGIBLE (state active, healthy)"
    );
    assert!(
        row("oss1").weight < row("oss3").weight,
        "the fill penalty must depress the full volume's weight"
    );
    assert!(
        (row("oss1").fill_ratio - 0.7).abs() < 0.01,
        "fill gauge exact-ish"
    );
    assert!(row("oss3").fill_ratio < 0.01);
    // Set-level spread gauge: max − min over eligible rows = ~70 pp.
    assert!(
        (table.fill_spread - 0.7).abs() < 0.02,
        "backend_fill_spread must read ~0.70, got {}",
        table.fill_spread
    );

    // 200 picks: never the out-of-band volume; even rr spread in band.
    let mut c2 = 0u32;
    let mut c3 = 0u32;
    for i in 0..200 {
        let (id, _, _) = fx
            .router
            .get_active_backend()
            .unwrap_or_else(|e| panic!("pick #{i} failed: {e:?}"));
        match id.as_str() {
            "oss1" => panic!("pick #{i} selected the out-of-band near-full volume"),
            "oss2" => c2 += 1,
            "oss3" => c3 += 1,
            other => panic!("unknown backend {other:?}"),
        }
    }
    assert!(
        c2.abs_diff(c3) <= 2,
        "atomic round-robin must alternate across the band (oss2={c2} oss3={c3})"
    );
}

// ---------------------------------------------------------------------------
// Eligibility: draining / retired / disabled excluded; refresh on change
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eligibility_and_refresh_on_state_change() {
    let _g = serial().await;
    let fx = router_with(&[
        ("oss1", 1000, 0),
        ("oss2", 1000, 0),
        ("oss3", 1000, 0),
        ("oss4", 1000, 0),
    ])
    .await;
    let recs = |states: &[(&str, &str)]| {
        states
            .iter()
            .map(|(id, st)| DataVolumeRecord {
                id: id.to_string(),
                backing_dev: format!("{id}.img"),
                state: st.to_string(),
                added_ts: 0,
            })
            .collect::<Vec<_>>()
    };
    fx.router.set_volume_records(recs(&[
        ("oss1", VOL_STATE_ACTIVE),
        ("oss2", VOL_STATE_ACTIVE),
        ("oss3", VOL_STATE_ACTIVE),
        ("oss4", VOL_STATE_ACTIVE),
    ]));

    // Draining: excluded from the band the moment the state flips — no
    // worker tick required (the state-change refresh hook).
    fx.router
        .set_volume_state("oss2", VOL_STATE_DRAINING)
        .unwrap();
    // Disabled (fail-stop health override): excluded instantly too.
    fx.router.set_health_override("oss3", true).unwrap();
    // Retired: deregistered — gone from the table entirely.
    fx.router.retire_backend("oss4").unwrap();

    let table = fx.router.placement_snapshot();
    let mut band = table.band_ids();
    band.sort();
    assert_eq!(
        band,
        vec!["oss1".to_string()],
        "only the active healthy volume places"
    );
    let row = |id: &str| table.rows.iter().find(|r| r.id == id);
    assert!(!row("oss2").expect("draining stays a row").eligible);
    assert!(!row("oss3").expect("disabled stays a row").eligible);
    assert!(row("oss4").is_none(), "a retired volume leaves the table");

    for i in 0..50 {
        let (id, _, _) = fx.router.get_active_backend().unwrap();
        assert_eq!(
            id, "oss1",
            "pick #{i} must exclude draining/disabled/retired"
        );
    }

    // Undrain / re-enable: rejoins the band immediately (refresh hooks).
    fx.router
        .set_volume_state("oss2", VOL_STATE_ACTIVE)
        .unwrap();
    fx.router.set_health_override("oss3", false).unwrap();
    let mut band = fx.router.placement_snapshot().band_ids();
    band.sort();
    assert_eq!(
        band,
        vec!["oss1".to_string(), "oss2".to_string(), "oss3".to_string()],
        "state-change hooks must refresh the table without a worker tick"
    );
}

/// The `unhealthy_backends` fail-stop mark is authoritative and INSTANT
/// even against a stale table (the absolute gate precedes any scoring —
/// §5.9: failover semantics untouched; G-VL-8 pinned test).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unhealthy_mark_is_instant_against_a_stale_table() {
    let _g = serial().await;
    let fx = router_with(&[("oss1", 1000, 0), ("oss2", 1000, 0)]).await;

    // Direct mark, NO refresh call — exactly what the health worker's
    // WentUnhealthy transition does between table refreshes.
    fx.router
        .unhealthy_backends
        .insert("oss2".to_string(), true);
    for i in 0..50 {
        let (id, _, _) = fx.router.get_active_backend().unwrap();
        assert_eq!(
            id, "oss1",
            "pick #{i}: the stale table must not serve the marked volume"
        );
    }
    fx.router.unhealthy_backends.remove("oss2");

    // Everything marked ⇒ the existing loud error path, unchanged.
    fx.router
        .unhealthy_backends
        .insert("oss1".to_string(), true);
    fx.router
        .unhealthy_backends
        .insert("oss2".to_string(), true);
    assert!(
        fx.router.get_active_backend().is_err(),
        "no healthy backends must stay a loud error"
    );
    fx.router.unhealthy_backends.remove("oss1");
    fx.router.unhealthy_backends.remove("oss2");
    // Recovery serves again (the empty-band rebuild path).
    assert!(fx.router.get_active_backend().is_ok());
}

// ---------------------------------------------------------------------------
// Zero-syscall hot path: picks never rebuild the table
// ---------------------------------------------------------------------------

/// The per-write `fs::metadata`+seek pair is RETIRED: the only syscall
/// site left is the refresh, and picks never trigger one on a healthy
/// set. Structurally, `PlacementTable::pick` takes ONLY the snapshot —
/// no router, no filesystem access to reach for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn picks_are_table_only_no_refresh_no_syscall() {
    let _g = serial().await;
    let fx = router_with(&[("oss1", 1000, 0), ("oss2", 1000, 0)]).await;

    let refreshes_before = METRICS.placement_table_refreshes.load(Ordering::Relaxed);
    for _ in 0..1000 {
        fx.router.get_active_backend().unwrap();
    }
    assert_eq!(
        METRICS.placement_table_refreshes.load(Ordering::Relaxed),
        refreshes_before,
        "healthy-set picks must never rebuild the table (the rebuild is \
         the only syscall site; a per-pick rebuild is the retired \
         per-write metadata/seek cost wearing a new hat)"
    );

    // Structural pin: the pick works on the snapshot ALONE.
    let table = fx.router.placement_snapshot();
    let sel = table.pick(|_| true);
    assert!(
        sel.is_some(),
        "a snapshot pick needs nothing but the snapshot"
    );

    // The picks land on the per-backend gauges.
    let table = fx.router.placement_snapshot();
    let total: u64 = table
        .rows
        .iter()
        .map(|r| r.picks.load(Ordering::Relaxed))
        .sum();
    assert!(
        total >= 1001,
        "backend_placement_picks must account for the picks (saw {total})"
    );
}

/// `placement_table_refreshes` counts every rebuild: explicit, and each
/// state-change hook.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_counter_and_fill_gauge_track_rebuilds() {
    let _g = serial().await;
    let fx = router_with(&[("oss1", 1000, 0), ("oss2", 1000, 0)]).await;

    let r0 = METRICS.placement_table_refreshes.load(Ordering::Relaxed);
    fx.router.refresh_placement_table();
    let r1 = METRICS.placement_table_refreshes.load(Ordering::Relaxed);
    assert_eq!(r1, r0 + 1, "explicit refresh counts");

    fx.router.set_health_override("oss2", true).unwrap();
    fx.router.set_health_override("oss2", false).unwrap();
    let r2 = METRICS.placement_table_refreshes.load(Ordering::Relaxed);
    assert!(r2 >= r1 + 2, "override hooks refresh (saw {r1} -> {r2})");

    // The fill gauge tracks the census across refreshes.
    let be = fx.router.backends.get("oss1").unwrap().value().clone();
    for _ in 0..500 {
        be.block_allocator.allocate_block().await.unwrap();
    }
    fx.router.refresh_placement_table();
    let table = fx.router.placement_snapshot();
    let row = table.rows.iter().find(|r| r.id == "oss1").unwrap();
    assert!(
        (row.fill_ratio - 0.5).abs() < 0.01,
        "backend_fill_ratio must track the allocator census (got {})",
        row.fill_ratio
    );
    assert!(
        (table.fill_spread - 0.5).abs() < 0.02,
        "backend_fill_spread must track max−min (got {})",
        table.fill_spread
    );
}

// ---------------------------------------------------------------------------
// G-VL-8 at fixture scale: fill convergence on an imbalanced set
// ---------------------------------------------------------------------------

/// One near-full (70 %) + two empty backends under a sustained write
/// workload: while spread > 10 pp, ≥ 60 % of new blocks land on the
/// emptiest band; `backend_fill_spread` decreases monotonically-modulo-
/// noise and converges below 10 pp — no rebalance job anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fill_spread_converges_on_imbalanced_set() {
    let _g = serial().await;
    let fx = router_with(&[
        ("full1", 1024, 717),
        ("empty1", 1024, 0),
        ("empty2", 1024, 0),
    ])
    .await;

    let mut prev_spread = f64::MAX;
    let mut converged = false;
    for epoch in 0..40 {
        fx.router.refresh_placement_table();
        let spread = fx.router.placement_snapshot().fill_spread;
        assert!(
            spread <= prev_spread + 0.02,
            "epoch {epoch}: spread must decrease monotonically-modulo-noise \
             ({prev_spread:.3} -> {spread:.3})"
        );
        prev_spread = spread;
        if spread < 0.10 {
            converged = true;
            break;
        }

        // One epoch of the workload: 64 new-block placements, each
        // charged to the backend the table picked (the write path's
        // allocate-on-picked-backend shape).
        let mut to_empty = 0u32;
        for _ in 0..64 {
            let (id, alloc, _) = fx.router.get_active_backend().unwrap();
            alloc
                .allocate_block()
                .await
                .unwrap_or_else(|e| panic!("epoch {epoch}: allocation failed: {e:?}"));
            if id != "full1" {
                to_empty += 1;
            }
        }
        // G-VL-8 (a): ≥ 60 % of new blocks to the emptiest band while
        // spread > 10 pp.
        assert!(
            to_empty >= 39,
            "epoch {epoch}: only {to_empty}/64 placements reached the \
             emptiest band while spread was {spread:.3} (> 10 pp)"
        );
    }
    assert!(
        converged,
        "spread did not converge below 10 pp within the write budget \
         (final {prev_spread:.3}); fills: full1={:.3} empty1={:.3} empty2={:.3}",
        fill_of(&fx.router, "full1"),
        fill_of(&fx.router, "empty1"),
        fill_of(&fx.router, "empty2"),
    );
    // The near-full volume never got fuller: placement drifted the set,
    // it never moved existing blocks.
    assert!(
        (fill_of(&fx.router, "full1") - 0.7).abs() < 0.01,
        "existing blocks must never move because of placement"
    );
}

// ---------------------------------------------------------------------------
// W1 non-interaction (fs-level): patches stay in place, ledger clean
// ---------------------------------------------------------------------------

const BLOCK_LBA: usize = 4096;

fn base_format_config(data_lvs: &[&Path]) -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BS,
        capacity: 1 << 34,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(
            data_lvs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        ),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
    }
}

async fn format_meta(meta: &Path, data_lvs: &[&Path]) {
    let cfg = base_format_config(data_lvs);
    squeezefs::meta_backend::kv::builder::format_v3(
        meta,
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
        },
    )
    .await
    .expect("format v3 meta volume");
}

struct FsFx {
    fs: SqueezefsFilesystem,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    _staging: TempDir,
}

/// Mount-shaped two-volume fixture (the volume_drain_tests shape without
/// the job fabric): records registered, first record's Arcs ARE the
/// default slot, refcounts recovered like a real mount.
async fn open_fs_fixture(meta: &Path, records: &[DataVolumeRecord]) -> FsFx {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    set_patch_max_bytes(512 * 1024);
    let dlm = DlmClient::new("local").unwrap();

    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), &first.id)
            .await
            .unwrap(),
    );
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }

    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    for rec in records {
        router
            .backend_router
            .register_backend(rec, dlm.meta_client().clone())
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.to_vec());
    router
        .backend_router
        .active_write_backend
        .store(Arc::new(first.id.clone()));

    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    for kv in &routed.volumes {
        for entry in fs.router.backend_router.backends.iter() {
            entry
                .value()
                .block_allocator
                .recover_active_blocks_v3(kv, &fs.router.backend_router)
                .await
                .expect("allocator recovery");
        }
    }
    FsFx {
        fs,
        meta: routed,
        _staging: staging,
    }
}

impl FsFx {
    async fn close(self) {
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
    }
}

async fn write_at(fx: &FsFx, ino: u64, off: u64, data: &[u8]) {
    let written = fx
        .fs
        .write(
            req(),
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"))
        .written;
    assert_eq!(written as usize, data.len(), "short write at {off}");
}

async fn read_at(fx: &FsFx, ino: u64, off: u64, len: usize) -> Vec<u8> {
    fx.fs
        .read(req(), ino, 0, off, len as u32, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off} failed: {e:?}"))
        .data
        .to_vec()
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 249) as u8) ^ tag | 1).collect()
}

/// The W1 decision-ledger snapshot: every `patch_ineligible_*` bucket +
/// the must-stay-0 tripwires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PatchLedger {
    ineligible_unmapped: u64,
    ineligible_decorated: u64,
    ineligible_unaligned: u64,
    ineligible_overlay: u64,
    ineligible_shared: u64,
    ineligible_transform: u64,
    ineligible_adjacent: u64,
    ineligible_oversize: u64,
    patch_writes: u64,
    edge_rmw_reads: u64,
    seed_read_bytes: u64,
}

fn ledger() -> PatchLedger {
    let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
    PatchLedger {
        ineligible_unmapped: l(&METRICS.patch_ineligible_unmapped),
        ineligible_decorated: l(&METRICS.patch_ineligible_decorated),
        ineligible_unaligned: l(&METRICS.patch_ineligible_unaligned),
        ineligible_overlay: l(&METRICS.patch_ineligible_overlay),
        ineligible_shared: l(&METRICS.patch_ineligible_shared),
        ineligible_transform: l(&METRICS.patch_ineligible_transform),
        ineligible_adjacent: l(&METRICS.patch_ineligible_adjacent),
        ineligible_oversize: l(&METRICS.patch_ineligible_oversize),
        patch_writes: l(&METRICS.patch_writes),
        edge_rmw_reads: l(&METRICS.patch_edge_rmw_reads),
        seed_read_bytes: l(&METRICS.write_path_seed_read_bytes),
    }
}

/// Placement is NEW-allocations-only: with the table actively preferring
/// the OTHER backend, a drain-free steady overwrite workload on a
/// striped file (a) patches in place (`patch_writes == ops`, zero
/// `patch_ineligible_*` delta, zero seed reads) and (b) leaves the block
/// map — and therefore block placement — bit-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w1_patch_unaffected_while_placement_prefers_other_backend() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_dev_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_dev_file(dir.path(), "oss1", 256 * 1024 * 1024);
    let oss2 = make_dev_file(dir.path(), "oss2", 256 * 1024 * 1024);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fs_fixture(&meta, &recs).await;
    let br = &fx.fs.router.backend_router;

    // Create the striped file with oss2 disabled: every block lands on
    // oss1 (the default slot — bare whole-block keys).
    br.set_health_override("oss2", true).unwrap();
    let ino = fx
        .fs
        .create(req(), 1, OsStr::new("patch.dat"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino;
    let blocks = 4u64;
    let mut want = pattern((blocks * BS) as usize, 0x3D);
    write_at(&fx, ino, 0, &want).await;
    fx.fs.fsync(req(), ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let m = fx.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    let map_before = m.block_map.clone().expect("striped block map");

    // Make oss1 the census loser (small capacity bound + extra fill) and
    // re-enable oss2: the table now prefers oss2 for NEW allocations.
    let oss1_id = recs[0].id.clone();
    let be1 = br.backends.get(&oss1_id).unwrap().value().clone();
    let chunk = be1.block_allocator.chunk_size();
    be1.block_allocator.set_capacity_bytes(16 * chunk);
    for _ in 0..4 {
        be1.block_allocator.allocate_block().await.unwrap();
    }
    br.backends
        .get("oss2")
        .unwrap()
        .value()
        .block_allocator
        .set_capacity_bytes(16 * chunk);
    br.set_health_override("oss2", false).unwrap();
    for i in 0..8 {
        let (id, _, _) = br.get_active_backend().unwrap();
        assert_eq!(
            id, "oss2",
            "premise (pick #{i}): the table must prefer the emptier oss2 \
             for new allocations"
        );
    }

    // The steady overwrite workload: LBA-aligned in-block small writes,
    // three rounds over four blocks.
    let before = ledger();
    let mut ops = 0u64;
    for round in 0..3u64 {
        let shapes: &[(u64, usize)] = &[
            (0, BLOCK_LBA),
            (BS + 8192, BLOCK_LBA),
            (2 * BS + 16384, 2 * BLOCK_LBA),
            (3 * BS + (BS - BLOCK_LBA as u64), BLOCK_LBA),
        ];
        for &(off, len) in shapes {
            let p = pattern(len, 0xA0 ^ (round as u8));
            write_at(&fx, ino, off, &p).await;
            want[off as usize..off as usize + len].copy_from_slice(&p);
            ops += 1;
        }
    }
    let after = ledger();

    assert_eq!(
        after.patch_writes - before.patch_writes,
        ops,
        "every aligned overwrite must ride the in-place patch"
    );
    for (name, b, a) in [
        (
            "unmapped",
            before.ineligible_unmapped,
            after.ineligible_unmapped,
        ),
        (
            "decorated",
            before.ineligible_decorated,
            after.ineligible_decorated,
        ),
        (
            "unaligned",
            before.ineligible_unaligned,
            after.ineligible_unaligned,
        ),
        (
            "overlay",
            before.ineligible_overlay,
            after.ineligible_overlay,
        ),
        ("shared", before.ineligible_shared, after.ineligible_shared),
        (
            "transform",
            before.ineligible_transform,
            after.ineligible_transform,
        ),
        (
            "adjacent",
            before.ineligible_adjacent,
            after.ineligible_adjacent,
        ),
        (
            "oversize",
            before.ineligible_oversize,
            after.ineligible_oversize,
        ),
    ] {
        assert_eq!(
            a - b,
            0,
            "patch_ineligible_{name} moved on a drain-free steady \
             overwrite workload — placement rotted the W1 predicate"
        );
    }
    assert_eq!(
        after.edge_rmw_reads, before.edge_rmw_reads,
        "v1: no edge RMW ever"
    );
    assert_eq!(
        after.seed_read_bytes, before.seed_read_bytes,
        "write_path_seed_read_bytes is a must-stay-0 tripwire"
    );

    // Existing blocks never move because of placement.
    let map_after = fx
        .fs
        .router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .block_map
        .clone()
        .expect("striped block map");
    assert_eq!(
        map_before, map_after,
        "the block map must be bit-identical — placement is \
         NEW-allocations-only"
    );

    // Byte-exactness end to end.
    let got = read_at(&fx, ino, 0, want.len()).await;
    assert_eq!(got, want, "patched file must stay byte-exact");
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Stats surface (§10): the placement family rides the stats inode
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_inode_carries_the_placement_family() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_dev_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_dev_file(dir.path(), "oss1", 256 * 1024 * 1024);
    let oss2 = make_dev_file(dir.path(), "oss2", 256 * 1024 * 1024);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fs_fixture(&meta, &recs).await;

    // A few placements so the gauges are non-trivial.
    for _ in 0..8 {
        fx.fs.router.backend_router.get_active_backend().unwrap();
    }
    fx.fs.router.backend_router.refresh_placement_table();

    let reply = fx
        .fs
        .read(req(), STATS_INODE, 0, 0, 1 << 20, 0)
        .await
        .expect("read .stats");
    let stats: serde_json::Value =
        serde_json::from_slice(&reply.data).expect(".stats must be valid JSON");

    let placement = stats
        .get("placement")
        .expect(".stats must carry the placement object");
    assert!(
        placement
            .get("backend_fill_spread")
            .is_some_and(|v| v.is_number()),
        "backend_fill_spread gauge missing: {placement}"
    );
    let backends = placement
        .get("backends")
        .and_then(|v| v.as_array())
        .expect("placement.backends rows");
    assert_eq!(
        backends.len(),
        2,
        "one row per registered backend: {backends:?}"
    );
    let mut picks_total = 0u64;
    for row in backends {
        for key in [
            "id",
            "backend_placement_picks",
            "backend_placement_weight",
            "backend_fill_ratio",
            "eligible",
        ] {
            assert!(row.get(key).is_some(), "placement row missing {key}: {row}");
        }
        picks_total += row["backend_placement_picks"].as_u64().unwrap();
    }
    assert!(
        picks_total >= 8,
        "picks gauge must account for the placements"
    );
    assert!(
        stats["metrics"]
            .get("placement_table_refreshes")
            .is_some_and(|v| v.as_u64().unwrap_or(0) >= 1),
        "placement_table_refreshes missing from the metrics object"
    );
    fx.close().await;
}
