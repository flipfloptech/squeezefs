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
    h.fs.read(h.req, ino, 0, off, size)
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

fn tier_has(h: &H, key: &str) -> bool {
    h.fs.router.cache.nvme.get_cached_read_block(key).is_some()
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
        h.fs.router.cache.read_lru.remove(key);
        h.fs.router.cache.nvme.remove_cached_read_block(key);
    }
    for (b, key) in &map {
        assert!(
            !tier_has(h, key),
            "fixture: block {b} must start cold in the read tier"
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
        tier_has(&h, &k0),
        "a completed publishable fill must be tier-visible when the \
         single-flight fetch returns — a detached (late) publish is exactly \
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
    assert!(tier_has(&h, &k2), "phase C block tier-visible at completion");

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
            tier_has(&h, key),
            "single-pass retention: block {b} of a tier-fitting working \
             set must still be resident at end of pass"
        );
    }
}
