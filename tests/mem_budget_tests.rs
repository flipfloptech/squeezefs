//! R5 — the joint memory authority (docs/design-read-path.md §5.7 / PR 7).
//!
//! Contracts pinned (red-first):
//! - Budget resolution order: flag → env → cgroup×0.8 → 70 % RAM (pure fn).
//! - Floor validation: Σ floors ≤ 0.9 × budget, else proportional clamp
//!   (loud, never a mount failure).
//! - Pressure = max(Σ component gauges, windowed-max RSS over 5 samples) —
//!   a decaying window, explicitly NOT a ratchet (spike ages out in 5
//!   ticks).
//! - Level transitions with hysteresis (enter 80/95, exit 76/91) and
//!   entry-edge event counters (no re-count inside a band).
//! - Red sheds to weights: per-component targets proportional to weight
//!   over the floor, floors respected, called once per Red tick, only for
//!   components above target.
//! - `level()` is a single relaxed atomic load usable from every growth
//!   path (&self, immediate visibility, no lock).
//! - Advisory integration: Yellow pauses dehydration entirely (protected
//!   victims DROPPED, not written); Yellow freezes prefetch window growth;
//!   Red stops prefetch issue; Red halves the parked-buffer cap.
//!
//! Counter-asserting integration phases share one test fn (process-global
//! counters + the process-global level — churn-suite counter-isolation
//! discipline). Pure-arithmetic tests use private `MemBudget` instances.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::mem_budget::{self, Component, Level, MemBudget};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::{tempdir, NamedTempFile, TempDir};

fn comp(
    name: &'static str,
    gauge: Arc<AtomicU64>,
    floor: u64,
    weight: u64,
    targets: Arc<Mutex<Vec<(&'static str, u64)>>>,
) -> Component {
    let g = gauge.clone();
    Component::new(
        name,
        floor,
        weight,
        Arc::new(move || g.load(Ordering::Relaxed)),
        Arc::new(move |target| targets.lock().unwrap().push((name, target))),
    )
}

// ---------------------------------------------------------------------------
// Pure arithmetic (private instances — no process-global interference)
// ---------------------------------------------------------------------------

#[test]
fn budget_resolution_order_flag_env_cgroup_ram() {
    let gib = 1024 * 1024 * 1024u64;
    // Flag wins over everything.
    assert_eq!(
        mem_budget::resolve_budget_from(Some(2 * gib), Some(3 * gib), Some(8 * gib), 64 * gib),
        2 * gib
    );
    // Env next.
    assert_eq!(
        mem_budget::resolve_budget_from(None, Some(3 * gib), Some(8 * gib), 64 * gib),
        3 * gib
    );
    // Cgroup memory.max × 0.8.
    assert_eq!(
        mem_budget::resolve_budget_from(None, None, Some(10 * gib), 64 * gib),
        8 * gib
    );
    // RAM fraction fallback: 70 %.
    assert_eq!(
        mem_budget::resolve_budget_from(None, None, None, 10 * gib),
        7 * gib
    );
}

#[test]
fn floor_sum_clamp_is_proportional_and_loud() {
    let b = MemBudget::new_for_test();
    let t = Arc::new(Mutex::new(Vec::new()));
    b.register(comp("a", Arc::new(AtomicU64::new(0)), 600, 1, t.clone()));
    b.register(comp("b", Arc::new(AtomicU64::new(0)), 300, 1, t.clone()));
    // Σ floors = 900 ≤ 0.9 × 1000 — no clamp.
    let f = b.effective_floors(1000);
    assert_eq!(f, vec![600, 300]);
    assert!(!b.floors_clamped());
    // Σ floors = 900 > 0.9 × 800 = 720 — proportional clamp (× 0.8).
    let f = b.effective_floors(800);
    assert_eq!(f, vec![480, 240], "proportional, not truncation-ordered");
    assert!(b.floors_clamped(), "clamp must be observable (loud)");
}

#[test]
fn level_transitions_hysteresis_and_entry_events() {
    let b = MemBudget::new_for_test();
    let g = Arc::new(AtomicU64::new(0));
    let t = Arc::new(Mutex::new(Vec::new()));
    b.register(comp("g", g.clone(), 0, 1, t));

    let mut step = |pressure: u64, expect: Level| {
        g.store(pressure, Ordering::Relaxed);
        b.tick_inner(1000, 0);
        assert_eq!(b.level(), expect, "pressure {pressure}");
    };

    step(790, Level::Green);
    step(800, Level::Yellow); // enter at >= 80%
    step(940, Level::Yellow);
    step(950, Level::Red); // enter at >= 95%
    step(920, Level::Red); // hysteresis: stays Red until < 91%
    step(905, Level::Yellow); // exit Red below 91%
    step(770, Level::Yellow); // hysteresis: stays Yellow until < 76%
    step(759, Level::Green); // exit Yellow below 76%
    step(800, Level::Yellow);
    step(700, Level::Green);

    assert_eq!(
        b.yellow_events(),
        2,
        "yellow entry edges only (no re-count inside the band)"
    );
    assert_eq!(b.red_events(), 1, "red entry edges only");
}

#[test]
fn windowed_rss_max_decays_not_ratchets() {
    let b = MemBudget::new_for_test();
    // No components: pressure = windowed RSS max alone.
    b.tick_inner(1000, 960);
    assert_eq!(b.level(), Level::Red, "spike enters Red");
    for i in 0..4 {
        b.tick_inner(1000, 100);
        assert_eq!(
            b.level(),
            Level::Red,
            "tick {i}: 5-sample window still holds the spike"
        );
    }
    b.tick_inner(1000, 100);
    assert_eq!(
        b.level(),
        Level::Green,
        "spike aged out after 5 samples — a ratchet would pin Red forever"
    );
    assert!(b.pressure_bytes() <= 100);
}

#[test]
fn red_sheds_to_weights_over_floors_once_per_tick() {
    let b = MemBudget::new_for_test();
    let ga = Arc::new(AtomicU64::new(500));
    let gb = Arc::new(AtomicU64::new(400));
    let gc = Arc::new(AtomicU64::new(80));
    let targets = Arc::new(Mutex::new(Vec::new()));
    b.register(comp("a", ga, 100, 3, targets.clone()));
    b.register(comp("b", gb, 100, 1, targets.clone()));
    // c sits at its floor — must receive NO shed call.
    b.register(comp("c", gc, 80, 1, targets.clone()));

    // pressure 980 / budget 1000 = 98% ⇒ Red. Shed target-total = 85% of
    // budget ⇒ excess 130, split by weight over the sheddable set
    // {a:3, b:1, c:1 → but c is at floor}.
    b.tick_inner(1000, 0);
    assert_eq!(b.level(), Level::Red);
    let t = targets.lock().unwrap().clone();
    assert_eq!(t.len(), 2, "exactly one shed call per over-floor component");
    let a = t.iter().find(|(n, _)| *n == "a").expect("a shed").1;
    let bb = t.iter().find(|(n, _)| *n == "b").expect("b shed").1;
    assert!(
        t.iter().all(|(n, _)| *n != "c"),
        "a component at its floor gets no shed call"
    );
    // Weight-proportional over floors: a takes 3/4 of the excess, b 1/4.
    assert!(a >= 100 && bb >= 100, "targets never below floors");
    let cut_a = 500 - a;
    let cut_b = 400 - bb;
    assert!(
        (cut_a as i64 - 3 * cut_b as i64).abs() <= 3,
        "weight proportionality: cut_a {cut_a} ≈ 3 × cut_b {cut_b}"
    );
    assert_eq!(cut_a + cut_b, 130, "sheds sum to the excess over 85%");

    // Yellow must NOT shed.
    targets.lock().unwrap().clear();
    b.tick_inner(1200, 0); // 980/1200 = 82% ⇒ Yellow
    assert_eq!(b.level(), Level::Yellow);
    assert!(
        targets.lock().unwrap().is_empty(),
        "Yellow is advisory-only: growth stops, nothing sheds"
    );
}

#[test]
fn level_read_is_lock_free_and_immediately_visible() {
    let b = Arc::new(MemBudget::new_for_test());
    let g = Arc::new(AtomicU64::new(990));
    b.register(comp("g", g, 0, 1, Arc::new(Mutex::new(Vec::new()))));
    // Concurrent readers during ticks: never blocks, always a valid level.
    let mut handles = Vec::new();
    for _ in 0..4 {
        let b = b.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..10_000 {
                let l = b.level();
                assert!(matches!(l, Level::Green | Level::Yellow | Level::Red));
            }
        }));
    }
    for _ in 0..100 {
        b.tick_inner(1000, 0);
    }
    for h in handles {
        h.join().unwrap();
    }
    // Immediate visibility (&self store → &self load, no locks in the API).
    assert_eq!(b.level(), Level::Red);
}

#[test]
fn parked_cap_halves_under_red() {
    assert_eq!(mem_budget::effective_parked_cap(256, Level::Green), 256);
    assert_eq!(mem_budget::effective_parked_cap(256, Level::Yellow), 256);
    assert_eq!(
        mem_budget::effective_parked_cap(256, Level::Red),
        128,
        "Red halves the parked-buffer spill threshold (early flush)"
    );
}

// ---------------------------------------------------------------------------
// Integration phases (process-global level + counters: ONE test fn)
// ---------------------------------------------------------------------------

const BS: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    // Whole-block machinery under test — the ranged/churn pin precedent.
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let s = Some(tempdir().unwrap());
    let staging_dirs = s
        .as_ref()
        .map(|d| vec![d.path().to_path_buf()])
        .unwrap_or_default();
    let cache = TieredCache::new(
        staging_dirs,
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
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
    assert_eq!(w.written as usize, data.len());
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

/// Force the PROCESS level for advisory-integration phases.
fn force_level(l: Level) {
    mem_budget::MEM_BUDGET.force_level_for_test(l);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advisory_integration_phases() {
    // ---- Phase A: Yellow pauses dehydration ENTIRELY — protected victims
    // dropped at the worker, never written to the tier (frees the eviction
    // channel's Bytes refs; disk-tier warmth is the cheapest sacrifice).
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1");
    let h = make(*b"membudget-a-pr70", "mb_ns_a").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");

    let ino =
        h.fs.create(h.req, 1, OsStr::new("mb_a"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    for b in 0..6u64 {
        write_at(&h, ino, b * BS, &vec![b as u8 + 1; BS as usize]).await;
    }
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let map =
        h.fs.router
            .fetch_metadata(&path)
            .await
            .unwrap()
            .block_map
            .unwrap_or_default();
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    // Block 0 → PROTECTED via ghost-admitted second miss, then remove its
    // tier copy so a dehydration WOULD have something to do.
    let d = read_at(&h, ino, 0, 65536).await;
    assert!(d.iter().all(|&x| x == 1));
    h.fs.router.cache.hot_block.remove(map.get(&0).unwrap());
    let d = read_at(&h, ino, 128 * 1024, 65536).await;
    assert!(d.iter().all(|&x| x == 1));
    h.fs.router
        .cache
        .nvme
        .remove_cached_read_block(map.get(&0).unwrap());

    force_level(Level::Yellow);
    let paused0 = METRICS.mem_budget_dehydrate_paused.load(Ordering::Relaxed);
    // Evict block 0 (protected) via probation churn.
    for b in 1..6u64 {
        let d = read_at(&h, ino, b * BS, 65536).await;
        assert!(d.iter().all(|&x| x == b as u8 + 1));
    }
    let mut paused_seen = false;
    for _ in 0..30 {
        if METRICS.mem_budget_dehydrate_paused.load(Ordering::Relaxed) > paused0 {
            paused_seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        paused_seen,
        "Yellow must DROP protected victims at the dehydration worker \
         (counter mem_budget_dehydrate_paused)"
    );
    assert!(
        h.fs.router
            .cache
            .nvme
            .get_cached_read_block(map.get(&0).unwrap())
            .is_none(),
        "the paused victim must NOT reach the tier"
    );
    force_level(Level::Green);
    drop(h);

    // ---- Phase B: Red stops prefetch issue outright; Green resumes.
    std::env::remove_var("SQUEEZEFS_READ_RANGED_THRESHOLD");
    let h2 = make(*b"membudget-b-pr70", "mb_ns_b").await;
    let ino_b = h2
        .fs
        .create(h2.req, 1, OsStr::new("mb_b"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino;
    for b in 0..16u64 {
        write_at(&h2, ino_b, b * BS, &vec![b as u8 + 10; BS as usize]).await;
    }
    h2.fs.fsync(h2.req, ino_b, 0, false).await.unwrap();
    let path_b = squeezefs::keys::inode_path(ino_b);
    let map_b = h2
        .fs
        .router
        .fetch_metadata(&path_b)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default();
    for key in map_b.values() {
        h2.fs.router.cache.purge_block_key(key);
    }

    force_level(Level::Red);
    let issued0 = METRICS.prefetch_issued.load(Ordering::Relaxed);
    for b in 0..8u64 {
        for half in 0..2u64 {
            let d = read_at(&h2, ino_b, b * BS + half * 262_144, 262_144).await;
            assert!(d.iter().all(|&x| x == b as u8 + 10));
        }
    }
    assert_eq!(
        METRICS.prefetch_issued.load(Ordering::Relaxed) - issued0,
        0,
        "Red must stop prefetch issue (the reads still serve, foreground-\
         driven)"
    );

    force_level(Level::Green);
    let issued0 = METRICS.prefetch_issued.load(Ordering::Relaxed);
    for b in 8..16u64 {
        for half in 0..2u64 {
            let d = read_at(&h2, ino_b, b * BS + half * 262_144, 262_144).await;
            assert!(d.iter().all(|&x| x == b as u8 + 10));
        }
    }
    assert!(
        METRICS.prefetch_issued.load(Ordering::Relaxed) > issued0,
        "Green resumes the pipeline"
    );
    // Settle all pipeline tasks before the next phase.
    for _ in 0..100 {
        let issued = METRICS.prefetch_issued.load(Ordering::Relaxed);
        let done = METRICS.prefetch_completed.load(Ordering::Relaxed)
            + METRICS.prefetch_wasted.load(Ordering::Relaxed);
        if issued == done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // ---- Phase C: Yellow freezes window GROWTH (no ×2 on foreground
    // wait) but keeps the pipeline alive at its current window.
    force_level(Level::Yellow);
    let hwm0 = METRICS.prefetch_window_hwm.load(Ordering::Relaxed);
    let g0 = METRICS.prefetch_foreground_waits.load(Ordering::Relaxed);
    let ino_c = h2
        .fs
        .create(h2.req, 1, OsStr::new("mb_c"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino;
    for b in 0..12u64 {
        write_at(&h2, ino_c, b * BS, &vec![b as u8 + 30; BS as usize]).await;
    }
    h2.fs.fsync(h2.req, ino_c, 0, false).await.unwrap();
    let path_c = squeezefs::keys::inode_path(ino_c);
    let map_c = h2
        .fs
        .router
        .fetch_metadata(&path_c)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default();
    for key in map_c.values() {
        h2.fs.router.cache.purge_block_key(key);
    }
    for b in 0..12u64 {
        for half in 0..2u64 {
            let d = read_at(&h2, ino_c, b * BS + half * 262_144, 262_144).await;
            assert!(d.iter().all(|&x| x == b as u8 + 30));
        }
    }
    let _ = g0;
    assert_eq!(
        METRICS.prefetch_window_hwm.load(Ordering::Relaxed),
        hwm0,
        "Yellow freezes window growth: the high-water mark must not move"
    );
    force_level(Level::Green);
}
