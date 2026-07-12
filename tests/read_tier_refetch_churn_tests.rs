//! Regression harness for the READ-TIER REFETCH CHURN family (the elbencho
//! O_DIRECT sequential-read row's 6.3× device-read multiplier —
//! `.benchmarks/2026-07-11-elbencho-odirect-read-smallblock-attribution.md`).
//!
//! Mechanism being pinned: `get_cached_or_fetch_block` is the single-flight
//! block fetch. For blocks ≥ 64 KiB its tier publish ran as a DETACHED
//! `spawn_blocking`, so the fetched block stayed invisible to the NVMe read
//! tier for as long as the blocking pool backlog (hundreds of ms under load)
//! while blocks > 256 KiB never enter the RAM LRU at all. The in-flight
//! registry entry drops when the fetch returns, so the NEXT sub-block read
//! of the SAME block — arriving milliseconds later on a sequential stream —
//! missed every tier, found no in-flight entry, became a fresh primary, and
//! refetched the whole block from the device *and queued yet another
//! publish* (measured: `get_obj` = 25,830 for 4,096 unique blocks; a
//! self-amplifying publish backlog). Prefetch jobs raced foreground fetches
//! of the same blocks through the identical window.
//!
//! Contract: a completed, incarnation-valid, publishable block fetch is
//! TIER-VISIBLE BEFORE the single-flight completes — so
//!  1. a sub-block read of a just-fetched block never touches the device
//!     again (backend fetches per unique block == 1),
//!  2. concurrent resolvers of one cold block produce exactly ONE backend
//!     fetch between them, and
//!  3. a sequential pass over a working set that fits the tier leaves every
//!     block tier-resident (single-pass retention: publish volume == unique
//!     blocks, so a fitting working set cannot self-evict mid-pass).
//!
//! Geometry: 512 KiB blocks — above the 256 KiB RAM-LRU gate (so these
//! tests exercise the exact large-block path the 4 MiB production shape
//! uses) and ≥ 64 KiB (the detached-publish branch being fixed).

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

/// 512 KiB: > 256 KiB (no RAM-LRU shortcut) and ≥ 64 KiB (the async tier
/// publish branch). Small enough that multi-block files stay cheap.
const BS: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    // The churn contracts count DEVICE FETCHES PER REQUEST; the R2
    // pipeline (PR 5) legitimately fetches ahead of the request stream,
    // which would shift phase counts without violating any contract.
    // Pin the suite to the request-driven shape (assertions byte-
    // identical); the pipeline's own counting discipline lives in
    // tests/read_prefetch_pipeline_tests.rs.
    std::env::set_var("SQUEEZEFS_READ_PREFETCH_WINDOW", "0");
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "refetch_churn_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
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
            uuid: *b"refetch-churn-v3",
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

/// Current block map of `ino` as the write paths just published it.
async fn block_map_of(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default()
}

/// RAM-or-disk visibility (docs/design-read-path.md §5.2/§5.3): under R1b
/// a first-touch fill's landing zone is the HOT tier (the disk publish is
/// second-touch-admitted), so the churn contract's visibility obligation
/// generalizes — the get_obj deltas below stay byte-identical, they are
/// the actual churn detectors.
fn hot_or_tier_has(h: &H, key: &str) -> bool {
    h.fs.router.cache.hot_block.get(key).is_some()
        || h.fs.router.cache.nvme.get_cached_read_block(key).is_some()
}

/// Make every mapped block of `ino` COLD: fsync (parks/flushes any RAM or
/// staged write residue into durable blocks), then purge the write path's
/// own cache retentions (read-LRU slice retention, tier copies) for every
/// current block key. Leaves exactly the production cold-read shape: block
/// map + device bytes, nothing cached. Returns the (post-flush) block map.
async fn make_cold(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    // Map AFTER the flush: flush merges can displace keys.
    let map = block_map_of(h, ino).await;
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    for (b, key) in &map {
        assert!(
            !hot_or_tier_has(h, key),
            "fixture: block {b} must start cold in every read tier"
        );
        assert!(
            h.fs.router.cache.read_lru.get(key).is_none(),
            "fixture: block {b} must start cold in the RAM LRU"
        );
    }
    map
}

/// The whole churn contract lives in ONE test fn: `METRICS.get_obj` is the
/// process-global device-read counter, so counter-delta assertions cannot
/// share a binary with concurrently running sibling tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetched_blocks_are_tier_visible_and_never_refetched() {
    let h = make().await;

    // ---- Fixture file 1: three full 512 KiB blocks, block-aligned writes
    // (write-through path: uploads purge the read tier, so every block is
    // COLD for the read phases below). Blocks 0 and 2 are the probes; block
    // 1 is never read, which keeps `should_prefetch_after_striped_read`
    // from ever seeing a `prev_end + 1` continuation — no background
    // prefetch fetches can pollute the counter phases.
    let ino = create(&h, "churn_probe").await;
    write_at(&h, ino, 0, &vec![0xA1u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0xB2u8; BS as usize]).await;
    write_at(&h, ino, 2 * BS, &vec![0xC3u8; BS as usize]).await;

    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).expect("block 0 mapped").clone();
    let k2 = map.get(&2).expect("block 2 mapped").clone();

    // ---- Phase A: cold sub-block read of block 0 = exactly one device
    // fetch, and the block is tier-visible AT RETURN (the visibility
    // contract that kills the churn: the publish completes inside the
    // single-flight, not on a detached timetable).
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let d = read_at(&h, ino, 0, 128 * 1024).await;
    assert_eq!(d.len(), 128 * 1024);
    assert!(d.iter().all(|&x| x == 0xA1), "phase A content");
    let g_a = METRICS.get_obj.load(Ordering::Relaxed);
    assert_eq!(
        g_a - g0,
        1,
        "cold sub-block read must fetch its block exactly once"
    );
    assert!(
        hot_or_tier_has(&h, &k0),
        "a completed publishable fill must be RAM-or-disk visible when the \
         single-flight fetch returns — a detached (late) landing is exactly \
         the refetch-churn window"
    );

    // ---- Phase B: immediately read a DIFFERENT sub-range of block 0.
    // Pre-fix this missed the tier (publish still queued), found no
    // in-flight entry, and refetched the whole block from the device.
    let d = read_at(&h, ino, 128 * 1024, 128 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xA1), "phase B content");
    let g_b = METRICS.get_obj.load(Ordering::Relaxed);
    assert_eq!(
        g_b - g_a,
        0,
        "a sub-block read of a just-fetched block must be a tier hit, \
         never a device refetch (the 6.3x churn multiplier)"
    );

    // ---- Phase C: FOUR concurrent cold resolvers of block 2 (prefetch /
    // foreground race shape) = exactly ONE device fetch between them.
    let g_c0 = METRICS.get_obj.load(Ordering::Relaxed);
    let reads = (0..4u64).map(|i| read_at(&h, ino, 2 * BS + i * 128 * 1024, 128 * 1024));
    let results = futures::future::join_all(reads).await;
    for (i, d) in results.iter().enumerate() {
        assert_eq!(d.len(), 128 * 1024, "phase C slice {i} length");
        assert!(
            d.iter().all(|&x| x == 0xC3),
            "phase C slice {i} content — concurrent resolvers must serve \
             the block's real bytes"
        );
    }
    let g_c = METRICS.get_obj.load(Ordering::Relaxed);
    assert_eq!(
        g_c - g_c0,
        1,
        "concurrent resolvers of one cold block must dedupe to a single \
         backend fetch (single-flight + publish-before-guard-drop)"
    );
    assert!(
        hot_or_tier_has(&h, &k2),
        "phase C block RAM-or-disk visible at completion"
    );

    // ---- Phase D: sequential pass over an 8-block working set (4 MiB —
    // comfortably inside the 128 MB read tier). One fetch per unique block
    // (get_obj/unique == 1.0, prefetch fetches included: whoever fetches
    // first is the only fetcher) and single-pass retention: every block is
    // still tier-resident at the end of the pass.
    let ino2 = create(&h, "churn_seq").await;
    for b in 0..8u64 {
        write_at(&h, ino2, b * BS, &vec![b as u8 + 1; BS as usize]).await;
    }
    let map2 = make_cold(&h, ino2).await;
    assert_eq!(map2.len(), 8, "8-block fixture mapped");

    let g_d0 = METRICS.get_obj.load(Ordering::Relaxed);
    for b in 0..8u64 {
        // Two sub-block reads per block — the elbencho row-2 shape.
        for half in 0..2u64 {
            let d = read_at(&h, ino2, b * BS + half * 256 * 1024, 256 * 1024).await;
            assert_eq!(d.len(), 256 * 1024);
            assert!(
                d.iter().all(|&x| x == b as u8 + 1),
                "phase D content block {b} half {half}"
            );
        }
    }
    let g_d = METRICS.get_obj.load(Ordering::Relaxed);
    assert_eq!(
        g_d - g_d0,
        8,
        "sequential pass: backend fetches must equal unique blocks \
         (get_obj/unique == 1.0), whether the fetcher was foreground or \
         prefetch — every extra fetch is the churn being pinned out"
    );
    for (b, key) in map2.iter() {
        assert!(
            hot_or_tier_has(&h, key),
            "single-pass retention: block {b} of a hot-tier-fitting working \
             set must still be resident at end of pass (under R1b the \
             landing zone is the hot tier)"
        );
    }
}

/// R1a (docs/design-read-path.md §5.2) — the result-carrying single-flight.
/// Waiter correctness must come from the CARRIED FILL RESULT, not from the
/// tier publish: PR 4 will skip publishes for streaming fills, so a waiter
/// that needs the tier to hit would refetch — the exact churn this suite
/// exists to pin out. Phases (one test fn — get_obj and the new
/// singleflight_waiter_result_serves counter are process-global, same
/// counter-isolation discipline as the phase A–D fn):
///
/// - E (waiter-serves-from-result): with the tier put artificially delayed
///   via `routing::TEST_TIER_PUBLISH_DELAY_MS` (the §5.2 named seam,
///   FAIL_NEXT_WRITES precedent), N concurrent cold resolvers of one block
///   dedupe to ONE device fetch and every non-primary is served from the
///   broadcast FillResult (`singleflight_waiter_result_serves == N-1`) —
///   bytes correct, no tier probe needed for correctness.
/// - F (late subscriber): a reader arriving after the cohort completed is
///   served by the cache re-check (tier hit while resident) — zero device
///   fetches, zero result-serves (the §5.2 close-only guard-drop case).
/// - G (primary failure): every member of a cohort whose primary's device
///   fetch fails gets an error promptly (the un-completed guard's Drop
///   sends `None` before closing — waiters fail fast into the re-check
///   loop, never park out the 60 s deadline); no device-read counter
///   movement (failed reads never count).
/// - H (primary cancelled mid-publish): aborting the primary's future while
///   it awaits the delayed publish drops the un-completed guard ⇒ `None` ⇒
///   live waiters re-check, ONE becomes the new primary and refetches
///   (device fetches == 2 total), the rest are served from the second
///   cohort's result (`singleflight_waiter_result_serves == cohort-1`) —
///   no fill leak, no hang, correct bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn result_carrying_single_flight_decouples_waiters_from_publish() {
    use squeezefs::routing::TEST_TIER_PUBLISH_DELAY_MS;

    let h = make().await;

    // Fixture: two fresh striped blocks (block 1 is the phase-H probe,
    // block 0 the phase-E/F probe), made cold exactly like phases A–D.
    let ino = create(&h, "r1a_probe").await;
    write_at(&h, ino, 0, &vec![0xE1u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0xF2u8; BS as usize]).await;
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).expect("block 0 mapped").clone();
    let k1 = map.get(&1).expect("block 1 mapped").clone();

    // ---- Phase E: delayed publish, 4 concurrent resolvers, 1 fetch,
    // waiters served from the carried result.
    TEST_TIER_PUBLISH_DELAY_MS.store(400, Ordering::Relaxed);
    let g_e0 = METRICS.get_obj.load(Ordering::Relaxed);
    let w_e0 = METRICS
        .singleflight_waiter_result_serves
        .load(Ordering::Relaxed);
    let reads = (0..4u64).map(|i| read_at(&h, ino, i * 128 * 1024, 128 * 1024));
    let results = futures::future::join_all(reads).await;
    TEST_TIER_PUBLISH_DELAY_MS.store(0, Ordering::Relaxed);
    for (i, d) in results.iter().enumerate() {
        assert_eq!(d.len(), 128 * 1024, "phase E slice {i} length");
        assert!(
            d.iter().all(|&x| x == 0xE1),
            "phase E slice {i} content — waiters must serve the fill's real bytes"
        );
    }
    let g_e = METRICS.get_obj.load(Ordering::Relaxed);
    let w_e = METRICS
        .singleflight_waiter_result_serves
        .load(Ordering::Relaxed);
    assert_eq!(g_e - g_e0, 1, "phase E: one device fetch for the cohort");
    assert_eq!(
        w_e - w_e0,
        3,
        "phase E: every non-primary cohort member must be served from the \
         carried FillResult (not a tier probe) — publish-independent \
         waiter correctness is R1a's whole point"
    );
    assert!(
        hot_or_tier_has(&h, &k0),
        "phase E: the fill still lands RAM-or-disk (hot probation under R1b)"
    );

    // ---- Phase F: post-cohort reader = cache re-check serve, no fetch,
    // no result-serve.
    let g_f0 = METRICS.get_obj.load(Ordering::Relaxed);
    let w_f0 = METRICS
        .singleflight_waiter_result_serves
        .load(Ordering::Relaxed);
    let d = read_at(&h, ino, 3 * 128 * 1024, 128 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xE1), "phase F content");
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g_f0,
        0,
        "phase F: late reader must be a cache hit"
    );
    assert_eq!(
        METRICS
            .singleflight_waiter_result_serves
            .load(Ordering::Relaxed)
            - w_f0,
        0,
        "phase F: late reader is served by the re-check loop, not the \
         (closed) broadcast"
    );

    // ---- Phase G: failing primary — whole cohort errors promptly, no
    // 60 s deadline park, no device-read counter movement.
    let g_g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let t0 = std::time::Instant::now();
    let fetches = (0..4).map(|_| h.fs.router.get_cached_or_fetch_block("unknownbe://42"));
    let results = futures::future::join_all(fetches).await;
    for (i, r) in results.iter().enumerate() {
        assert!(
            r.is_err(),
            "phase G resolver {i} must surface the fetch error"
        );
    }
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(10),
        "phase G: cohort failure must resolve promptly (None-on-drop), \
         never park toward the 60 s deadline"
    );
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g_g0,
        0,
        "phase G: failed fetches never count device reads"
    );

    // ---- Phase H: primary aborted mid-publish — waiters recover through
    // a second cohort; exactly one refetch; no leak, no hang.
    //
    // R1b prerequisite: abort-MID-PUBLISH requires a publish to exist, and
    // under second-touch admission a first-touch fill skips it. Prime the
    // ghost with one fetch of block 1, then purge every tier so the phase
    // fill is a genuine ghost-admitted (publishing) miss.
    let _ = read_at(&h, ino, BS, 64 * 1024).await;
    h.fs.router.cache.purge_block_key(&k1);
    TEST_TIER_PUBLISH_DELAY_MS.store(800, Ordering::Relaxed);
    let g_h0 = METRICS.get_obj.load(Ordering::Relaxed);
    let w_h0 = METRICS
        .singleflight_waiter_result_serves
        .load(Ordering::Relaxed);
    let router = h.fs.router.clone();
    let k1c = k1.clone();
    let primary = tokio::spawn(async move { router.get_cached_or_fetch_block(&k1c).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let waiters: Vec<_> = (0..3)
        .map(|_| {
            let router = h.fs.router.clone();
            let k = k1.clone();
            tokio::spawn(async move { router.get_cached_or_fetch_block(&k).await })
        })
        .collect();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    primary.abort();
    let _ = primary.await; // JoinError(cancelled) expected
    for (i, w) in futures::future::join_all(waiters)
        .await
        .into_iter()
        .enumerate()
    {
        let val = w
            .expect("phase H waiter task must not panic")
            .unwrap_or_else(|e| panic!("phase H waiter {i} must recover after the abort: {e:?}"));
        assert_eq!(val.len(), BS as usize, "phase H waiter {i} length");
        assert!(
            val.iter().all(|&x| x == 0xF2),
            "phase H waiter {i} content — recovery must serve block 1's real bytes"
        );
    }
    TEST_TIER_PUBLISH_DELAY_MS.store(0, Ordering::Relaxed);
    let g_h = METRICS.get_obj.load(Ordering::Relaxed);
    let w_h = METRICS
        .singleflight_waiter_result_serves
        .load(Ordering::Relaxed);
    assert_eq!(
        g_h - g_h0,
        2,
        "phase H: the aborted primary's fetch plus exactly one recovery \
         refetch — waiters must neither all refetch (leak) nor serve a \
         cancelled cohort's missing result (hang)"
    );
    // Serve SOURCE for the second cohort's non-primaries is legitimately
    // nondeterministic: the aborted primary's publish closure keeps running
    // on the blocking pool (spawn_blocking is not cancelled by task abort —
    // §5.2's future-drop case cancels the AWAIT, not the closure) and its
    // incarnation-checked put may land first, so waiters can be served by
    // the cache re-check (tier hit) instead of the second cohort's carried
    // result. Both routes are refetch-free — the get_obj == 2 assert above
    // is the leak detector; phases E–G already pin the result-serve path
    // when the tier cannot serve. Never MORE result-serves than
    // non-primaries exist:
    assert!(
        w_h - w_h0 <= 2,
        "phase H: at most the two non-primaries can be result-served \
         (got {})",
        w_h - w_h0
    );
}
