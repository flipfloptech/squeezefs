//! R2 — the per-stream pipelined prefetcher (docs/design-read-path.md §5.5
//! / PR 5), and the home of the multi-stream contention phase (§5.5's one
//! stated home; churn-suite counter-isolation discipline: every
//! counter-asserting phase shares ONE test fn, since `get_obj` and the
//! `prefetch_*` counters are process-global).
//!
//! Contracts pinned:
//! - A classified stream's pipeline issues through the result-carrying
//!   single-flight: foreground + prefetch dedupe to ONE device fetch per
//!   unique block (`get_obj`), fills land hot-tier probation, and on the
//!   clean shape `prefetch_wasted == 0` and
//!   `prefetch_evicted_unconsumed == 0` with every issued task accounted
//!   (`issued == completed + wasted` at settle).
//! - Adaptive window: grows on foreground-wait (hwm records it), capped by
//!   `SQUEEZEFS_READ_PREFETCH_WINDOW` (0 disables the pipeline outright).
//! - Abandonment: a non-sequential read bumps the lane generation — issue
//!   stops within one window and in-flight fills settle as wasted, never
//!   leaked.
//! - Evict-before-consume (the PR 4 R-5 spiral, now the RED shape for the
//!   control): under a deliberately tiny hot budget the consumer detects
//!   evicted-unconsumed fills (`prefetch_evicted_unconsumed`), AIMD halves
//!   the lane window, and `get_obj` overshoot stays BOUNDED (< 2x unique)
//!   — never the issue→fill→evict→issue spiral.
//! - Multi-stream contention: M streams against the same tiny budget stay
//!   bounded collectively (contention-scaled effective_window).
//! - Lane-leak self-repair: the `active_streams` two-epoch gauge ages a
//!   silently-evicted lane out within <= 2 epochs; no monotonic leak.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make_with(uuid: [u8; 16]) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "pipeline_test")
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
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

async fn make_cold_file(h: &H, name: &str, blocks: u64, fill_base: u8) -> u64 {
    let ino = create(h, name).await;
    for b in 0..blocks {
        write_at(h, ino, b * BS, &vec![fill_base.wrapping_add(b as u8); BS as usize]).await;
    }
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let map = h
        .fs
        .router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default();
    assert_eq!(map.len() as u64, blocks, "fixture map for {name}");
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    ino
}

/// Stream a file with two 256 KiB sub-reads per block, verifying content.
async fn stream_file(h: &H, ino: u64, blocks: u64, fill_base: u8) {
    for b in 0..blocks {
        for half in 0..2u64 {
            let d = read_at(h, ino, b * BS + half * 256 * 1024, 256 * 1024).await;
            assert_eq!(d.len(), 256 * 1024);
            assert!(
                d.iter().all(|&x| x == fill_base.wrapping_add(b as u8)),
                "content block {b} half {half}"
            );
        }
    }
}

/// Poll until every issued prefetch task has settled (completed or wasted).
async fn settle_pipeline() {
    for _ in 0..100 {
        let issued = METRICS.prefetch_issued.load(Ordering::Relaxed);
        let done = METRICS.prefetch_completed.load(Ordering::Relaxed)
            + METRICS.prefetch_wasted.load(Ordering::Relaxed);
        if issued == done {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!(
        "pipeline never settled: issued={} completed={} wasted={} — leaked tasks",
        METRICS.prefetch_issued.load(Ordering::Relaxed),
        METRICS.prefetch_completed.load(Ordering::Relaxed),
        METRICS.prefetch_wasted.load(Ordering::Relaxed)
    );
}

/// ALL counter-asserting phases in one fn (get_obj + prefetch_* are
/// process-global — the churn suite's counter-isolation discipline; §5.5
/// names this file as the contention phase's one home).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipeline_phases() {
    // ---- Phase A: clean single stream — dedupe, zero waste, zero
    // evicted-unconsumed, full task accounting.
    let h = make_with(*b"pipeline-a-pr5v3").await;
    let ino = make_cold_file(&h, "pipe_a", 16, 1).await;

    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let issued0 = METRICS.prefetch_issued.load(Ordering::Relaxed);
    let wasted0 = METRICS.prefetch_wasted.load(Ordering::Relaxed);
    let evicted0 = METRICS.prefetch_evicted_unconsumed.load(Ordering::Relaxed);

    stream_file(&h, ino, 16, 1).await;
    settle_pipeline().await;

    let g = METRICS.get_obj.load(Ordering::Relaxed) - g0;
    assert_eq!(
        g, 16,
        "clean stream: foreground + pipeline must dedupe through the \
         single-flight to exactly one device fetch per unique block"
    );
    assert!(
        METRICS.prefetch_issued.load(Ordering::Relaxed) > issued0,
        "the pipeline must actually issue on a classified stream"
    );
    assert_eq!(
        METRICS.prefetch_wasted.load(Ordering::Relaxed) - wasted0,
        0,
        "clean shape: zero wasted fills"
    );
    assert_eq!(
        METRICS.prefetch_evicted_unconsumed.load(Ordering::Relaxed) - evicted0,
        0,
        "clean shape: zero evicted-unconsumed (budget fits the stream)"
    );
    assert!(
        METRICS.prefetch_window_hwm.load(Ordering::Relaxed) >= 2,
        "window high-water mark recorded"
    );
    assert_eq!(
        METRICS.prefetch_inflight_bytes.load(Ordering::Relaxed),
        0,
        "in-flight gauge returns to zero at settle"
    );

    // ---- Phase B: abandonment — a non-sequential read stops issue within
    // one window; nothing leaks.
    let ino_b = make_cold_file(&h, "pipe_b", 24, 40).await;
    for b in 0..6u64 {
        let d = read_at(&h, ino_b, b * BS, 256 * 1024).await;
        assert!(d.iter().all(|&x| x == 40u8.wrapping_add(b as u8)));
    }
    // Jump far away: generation bump, plan cleared.
    let d = read_at(&h, ino_b, 20 * BS, 256 * 1024).await;
    assert!(d.iter().all(|&x| x == 60u8));
    settle_pipeline().await;
    let issued_snapshot = METRICS.prefetch_issued.load(Ordering::Relaxed);
    // Idle-ish random touches must not issue a stream's worth of fetches.
    for b in [3u64, 11, 7, 15] {
        let _ = read_at(&h, ino_b, b * BS + 128 * 1024, 4096).await;
    }
    settle_pipeline().await;
    let issued_after = METRICS.prefetch_issued.load(Ordering::Relaxed);
    assert!(
        issued_after - issued_snapshot <= 16,
        "abandoned/random access must not keep a stream pipeline alive \
         (issued {} more after abandonment)",
        issued_after - issued_snapshot
    );

    // ---- Phase C: evict-before-consume control (the PR 4 R-5 spiral,
    // pinned bounded). Tiny hot budget: fills evict before consumption;
    // the consumer detects it, AIMD collapses the window, and total device
    // fetches stay < 2x unique — never the runaway spiral.
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1");
    let h2 = make_with(*b"pipeline-c-pr5v3").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    let ino_c = make_cold_file(&h2, "pipe_c", 24, 100).await;

    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    stream_file(&h2, ino_c, 24, 100).await;
    settle_pipeline().await;
    let g = METRICS.get_obj.load(Ordering::Relaxed) - g0;
    assert!(
        g < 48,
        "evict-before-consume must stay BOUNDED (AIMD): {g} device fetches \
         for 24 unique blocks — >= 2x is the spiral this control exists \
         to prevent"
    );
    assert!(
        METRICS.prefetch_evicted_unconsumed.load(Ordering::Relaxed) > 0,
        "the consumer must DETECT evicted-unconsumed fills — this counter \
         is the spiral detector the design's observability demands"
    );

    // ---- Phase D: multi-stream contention against the same tiny budget —
    // collectively bounded (contention-scaled effective_window).
    let mut inos = Vec::new();
    for i in 0..4u8 {
        inos.push((
            make_cold_file(&h2, &format!("pipe_d{i}"), 12, 150 + i * 20).await,
            150 + i * 20,
        ));
    }
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let mut tasks = Vec::new();
    for (ino, base) in inos {
        let fs = h2.fs.clone();
        let req = h2.req;
        tasks.push(tokio::spawn(async move {
            for b in 0..12u64 {
                let d = fs
                    .read(req, ino, 0, b * BS + 128 * 1024, 262_144, 0)
                    .await
                    .unwrap()
                    .data
                    .to_vec();
                assert!(
                    d.iter().all(|&x| x == base.wrapping_add(b as u8)),
                    "stream base {base} block {b}"
                );
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    settle_pipeline().await;
    let g = METRICS.get_obj.load(Ordering::Relaxed) - g0;
    assert!(
        g < 96,
        "4-stream contention must stay collectively bounded: {g} fetches \
         for 48 unique blocks"
    );

    // ---- Phase E: window cap + kill switch.
    std::env::set_var("SQUEEZEFS_READ_PREFETCH_WINDOW", "0");
    let h3 = make_with(*b"pipeline-e-pr5v3").await;
    std::env::remove_var("SQUEEZEFS_READ_PREFETCH_WINDOW");
    let ino_e = make_cold_file(&h3, "pipe_e", 8, 200).await;
    let issued0 = METRICS.prefetch_issued.load(Ordering::Relaxed);
    stream_file(&h3, ino_e, 8, 200).await;
    settle_pipeline().await;
    assert_eq!(
        METRICS.prefetch_issued.load(Ordering::Relaxed) - issued0,
        0,
        "SQUEEZEFS_READ_PREFETCH_WINDOW=0 must disable the pipeline outright"
    );

    // ---- Phase F: lane-leak self-repair — the two-epoch active_streams
    // gauge ages out a silently-evicted lane within <= 2 epochs.
    let h4 = make_with(*b"pipeline-f-pr5v3").await;
    let ino_f1 = make_cold_file(&h4, "pipe_f1", 16, 10).await;
    let ino_f2 = make_cold_file(&h4, "pipe_f2", 16, 30).await;
    // Classify both (interleaved starts).
    for b in 0..5u64 {
        let _ = read_at(&h4, ino_f1, b * BS, 262_144).await;
        let _ = read_at(&h4, ino_f2, b * BS, 262_144).await;
    }
    assert!(
        METRICS.prefetch_active_streams.load(Ordering::Relaxed) >= 2,
        "both classified streams must be counted"
    );
    // Silently evict stream 1's lane state (the leak class the gauge
    // design precludes), keep stream 2 alive across >= 2 epochs (2 s each).
    h4.fs
        .router
        .stream_lanes
        .invalidate(&squeezefs::keys::inode_path(ino_f1));
    let mut b = 5u64;
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_millis(5200) {
        let _ = read_at(&h4, ino_f2, (b % 16) * BS, 262_144).await;
        b += 1;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let gauge = METRICS.prefetch_active_streams.load(Ordering::Relaxed);
    assert!(
        gauge <= 1,
        "a dead lane must age out of active_streams within two epochs \
         (gauge = {gauge}) — an inc/dec counter would leak here"
    );
    settle_pipeline().await;
}
