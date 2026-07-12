//! R1b — scan-resistant tier admission + O_DIRECT/stream no-publish + the
//! dehydration gate flip (docs/design-read-path.md §5.3 / PR 4).
//!
//! The tax being killed: every >256 KiB cold fill published 4 MiB into the
//! NVMe disk tier (16.5 GiB of tier writes per 16 GiB read, PR 1 ledger).
//! Admission policy (>256 KiB fills only; ≤256 KiB keeps today's behavior
//! verbatim): FIRST touch of a block key skips the disk publish and records
//! the key in the ghost table (fills still land hot-tier probation — PR 3's
//! landing zone); a SECOND miss within the two-epoch ghost window publishes
//! (protected hot put + today's awaited validated publish). Skipping a
//! publish is always correctness-safe — absence means the next reader goes
//! to the device; every retained publish keeps the full validated-fill
//! discipline untouched.
//!
//! Also pinned here: the dehydration gate flip (probation-never-touched
//! victims are DROPPED; protected victims dehydrate; the ≤256 KiB read_lru
//! population dehydrates exactly as today), the K=4 offset-lane stream
//! classifier, O_DIRECT read-flag visibility through the vendored fuse3,
//! the `SQUEEZEFS_READ_TIER_ADMISSION` escape hatch, the
//! hot-budget-0 ⇒ auto-`always` interaction, and the 256 KiB block-size
//! boundary (every fill on the small-fill path — unchanged by design).

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

async fn make_bs(block_size: &str, uuid: [u8; 16]) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", block_size);
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "admission_test")
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

async fn make() -> H {
    make_bs("524288", *b"admission-pr4-v3").await
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

/// Read through the FUSE surface with explicit open flags — the O_DIRECT
/// visibility plumb (vendored fuse3 `fuse_read_in.flags` → the handler).
async fn read_flags(h: &H, ino: u64, off: u64, size: u32, flags: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, flags)
        .await
        .unwrap()
        .data
        .to_vec()
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    read_flags(h, ino, off, size, 0).await
}

async fn block_map_of(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default()
}

async fn make_cold(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let map = block_map_of(h, ino).await;
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    map
}

fn tier_has(h: &H, key: &str) -> bool {
    h.fs.router.cache.nvme.get_cached_read_block(key).is_some()
}

fn hot_has(h: &H, key: &str) -> bool {
    h.fs.router.cache.hot_block.get(key).is_some()
}

// ---------------------------------------------------------------------------
// Admission policy table, row by row (default mode: second-touch)
// ---------------------------------------------------------------------------

/// First touch skips the disk publish (fill lands hot probation only);
/// the SECOND miss of the same key — hot entry purged to force a real
/// re-miss — publishes (ghost hit), and the entry then serves from the
/// tier. The tax kill and its warmth-recovery path in one contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_touch_admission_first_skip_then_publish() {
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    let h = make().await;
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");
    let ino = create(&h, "adm_second_touch").await;
    write_at(&h, ino, 0, &vec![0xA1u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0xA2u8; BS as usize]).await;
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();

    let skipped0 = METRICS.read_fill_publishes_skipped.load(Ordering::Relaxed);
    let d = read_at(&h, ino, 0, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xA1));
    assert!(
        !tier_has(&h, &k0),
        "FIRST touch of a >256 KiB fill must skip the disk-tier publish — \
         this is the 16.5 GiB-per-16 GiB tax being killed"
    );
    assert!(
        hot_has(&h, &k0),
        "the fill still lands in hot probation (PR 3)"
    );
    assert!(
        METRICS.read_fill_publishes_skipped.load(Ordering::Relaxed) > skipped0,
        "skip counter is the adoption signal"
    );

    // Force a genuine second MISS (hot purged; tier empty).
    h.fs.router.cache.hot_block.remove(&k0);
    let ghost0 = METRICS
        .read_tier_admission_ghost_hits
        .load(Ordering::Relaxed);
    let d = read_at(&h, ino, 0, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xA1));
    assert!(
        tier_has(&h, &k0),
        "the SECOND miss within the ghost window must publish — re-read \
         heat converges to the disk tier (second-touch admission)"
    );
    assert!(
        METRICS
            .read_tier_admission_ghost_hits
            .load(Ordering::Relaxed)
            > ghost0
    );

    // Third read: hot (protected) or tier hit — zero further device work
    // is pinned by the churn suite; here pin the tier serve path exists.
    let d = read_at(&h, ino, 128 * 1024, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xA1));
}

/// ≤256 KiB population: on a 256 KiB-block-size volume every striped fill
/// takes the small-fill path — publish-on-first-touch, exactly today's
/// behavior, by design (the §5.3 boundary pin).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn small_block_volume_keeps_first_touch_publish() {
    let h = make_bs("262144", *b"admission-sm4-v3").await;
    let bs = 262_144u64;
    let ino = create(&h, "adm_small").await;
    write_at(&h, ino, 0, &vec![0xB1u8; bs as usize]).await;
    write_at(&h, ino, bs, &vec![0xB2u8; bs as usize]).await;
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();

    let d = read_at(&h, ino, 0, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xB1));
    assert!(
        tier_has(&h, &k0),
        "≤256 KiB fills keep today's unconditional publish — the admission \
         policy governs only the population that pays 4 MiB publishes"
    );
}

/// O_DIRECT visibility: the vendored fuse3 hands the file's open flags to
/// the read handler on every request; an O_DIRECT cold read is counted and
/// (first touch) skips the publish.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn o_direct_flag_reaches_the_classifier() {
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    let h = make_bs("524288", *b"admission-od4-v3").await;
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");
    let ino = create(&h, "adm_odirect").await;
    write_at(&h, ino, 0, &vec![0xC1u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0xC2u8; BS as usize]).await;
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();

    let od0 = METRICS.read_odirect_requests.load(Ordering::Relaxed);
    let d = read_flags(&h, ino, 0, 64 * 1024, libc::O_DIRECT as u32).await;
    assert!(d.iter().all(|&x| x == 0xC1));
    assert!(
        METRICS.read_odirect_requests.load(Ordering::Relaxed) > od0,
        "fuse_read_in.flags must be visible per-request (the vendored \
         crate previously discarded them)"
    );
    assert!(!tier_has(&h, &k0), "O_DIRECT first touch skips the publish");
    assert!(hot_has(&h, &k0));
}

/// Stream classifier: 4 contiguous sub-reads classify a lane as streaming;
/// two interleaved sequential readers of ONE file classify independently
/// (K = 4 offset lanes — a single cursor would see perpetual jumps).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_interleaved_readers_both_classify() {
    let h = make_bs("524288", *b"admission-ln4-v3").await;
    let ino = create(&h, "adm_lanes").await;
    for b in 0..4u64 {
        write_at(&h, ino, b * BS, &vec![b as u8 + 1; BS as usize]).await;
    }
    make_cold(&h, ino).await;

    let cls0 = METRICS.read_streams_classified.load(Ordering::Relaxed);
    // Reader A walks from 0; reader B walks from 2*BS; strictly
    // interleaved 128 KiB requests — each contiguous within its own lane.
    let step = 131_072u64;
    for i in 0..8u64 {
        let a = read_at(&h, ino, i * step, step as u32).await;
        assert_eq!(a.len(), step as usize, "reader A step {i}");
        let b = read_at(&h, ino, 2 * BS + i * step, step as u32).await;
        assert_eq!(b.len(), step as usize, "reader B step {i}");
    }
    let classified = METRICS.read_streams_classified.load(Ordering::Relaxed) - cls0;
    assert!(
        classified >= 2,
        "both interleaved sequential readers must classify (got {classified}) \
         — a single-cursor detector would see alternating jumps and never \
         classify either"
    );
}

/// Escape hatch: SQUEEZEFS_READ_TIER_ADMISSION=always restores today's
/// publish-on-first-touch verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_always_restores_first_touch_publish() {
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "always");
    let h = make_bs("524288", *b"admission-al4-v3").await;
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");

    let ino = create(&h, "adm_always").await;
    write_at(&h, ino, 0, &vec![0xD1u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0xD2u8; BS as usize]).await;
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();

    let d = read_at(&h, ino, 0, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xD1));
    assert!(
        tier_has(&h, &k0),
        "admission=always is the operator escape hatch: first-touch publish"
    );
}

/// Knob interaction: hot-tier budget 0 auto-degrades admission to `always`
/// — with no RAM landing zone, skipping the publish would make every
/// sub-read a device refetch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hot_budget_zero_auto_degrades_to_always() {
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "0");
    let h = make_bs("524288", *b"admission-hz4-v3").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");

    let ino = create(&h, "adm_hotzero").await;
    write_at(&h, ino, 0, &vec![0xE1u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0xE2u8; BS as usize]).await;
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();

    let d = read_at(&h, ino, 0, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xE1));
    assert!(
        tier_has(&h, &k0),
        "hot budget 0 must auto-degrade admission to always-publish so \
         sub-reads stay cheap (the doc's pinned knob interaction)"
    );
}

// ---------------------------------------------------------------------------
// Dehydration gate flip (the channel was typed behavior-neutral in PR 3)
// ---------------------------------------------------------------------------

/// The dehydration-class matrix: a probation-NEVER-read victim is dropped
/// (counted); a probation-read-once victim was promoted in place (sticky)
/// and dehydrates to the NVMe tier on eviction; the ≤256 KiB read_lru
/// population keeps its historical dehydration path bit-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dehydration_gate_drops_untouched_probation_and_dehydrates_protected() {
    // Tiny hot budget: 1 MiB = two 512 KiB entries per the whole tier.
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1");
    let h = make_bs("524288", *b"admission-dh4-v3").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");

    let ino = create(&h, "adm_dehy").await;
    for b in 0..6u64 {
        write_at(&h, ino, b * BS, &vec![b as u8 + 1; BS as usize]).await;
    }
    let map = make_cold(&h, ino).await;

    // Make block 0 PROTECTED via a block-level re-access (ghost-admitted
    // second miss ⇒ protected insert + publish); stream sub-read
    // consumption deliberately does NOT promote (the R1b liveness
    // correction — promoting consumption re-taxed streams through
    // protected-victim dehydration; the PR 4 bench OOM). Then clear the
    // tier copy so the eventual dehydration is observable, and stream
    // blocks 1..6: probation churn evicts both the protected entry
    // (dehydrates) and never-read probation victims (dropped + counted).
    let drops0 = METRICS.hot_block_probation_drops.load(Ordering::Relaxed);
    let d = read_at(&h, ino, 0, 64 * 1024).await; // touch 1: probation
    assert!(d.iter().all(|&x| x == 1));
    h.fs.router.cache.hot_block.remove(map.get(&0).unwrap());
    let d = read_at(&h, ino, 128 * 1024, 64 * 1024).await; // touch 2: ghost-admit
    assert!(d.iter().all(|&x| x == 1));
    // Tier now holds the admitted copy; drop it so dehydration is visible.
    h.fs.router
        .cache
        .nvme
        .remove_cached_read_block(map.get(&0).unwrap());
    for b in 1..6u64 {
        let d = read_at(&h, ino, b * BS, 64 * 1024).await;
        assert!(d.iter().all(|&x| x == b as u8 + 1), "block {b}");
    }
    // Give the dehydration worker a beat.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let drops = METRICS.hot_block_probation_drops.load(Ordering::Relaxed) - drops0;
    assert!(
        drops >= 1,
        "never-read probation victims must be DROPPED by the flipped gate \
         (got {drops} drops) — dehydrating one-pass streams is the tax in \
         RAM-eviction form"
    );

    // The promoted (protected) block-0 victim must have dehydrated to the
    // NVMe tier (validated non-owner publish path).
    let k0 = map.get(&0).unwrap().clone();
    let mut dehydrated = false;
    for _ in 0..20 {
        if tier_has(&h, &k0) {
            dehydrated = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        dehydrated,
        "a protected (read-promoted) victim must dehydrate to the NVMe \
         tier — warmth is preserved for entries something actually re-read"
    );
}

/// Dehydration DEDUPE: a protected victim whose bytes are ALREADY
/// tier-resident is dropped at the channel mouth — never a duplicate
/// write. Under second-touch every ghost-admitted fill publishes at fetch
/// time AND lands protected in hot, so without this probe every one of
/// its hot evictions re-wrote the same 4 MiB the tier already held
/// (measured on the bench rand-4k row: ~100% protected evictions,
/// duplicate-write churn + multi-MiB payloads parked in the channel under
/// the cage — 37 IOPS vs the 326-lineage).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dehydration_skips_tier_resident_protected_victims() {
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1");
    let h = make_bs("524288", *b"admission-dd5-v3").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");

    let ino = create(&h, "adm_dedupe").await;
    for b in 0..6u64 {
        write_at(&h, ino, b * BS, &vec![b as u8 + 1; BS as usize]).await;
    }
    let map = make_cold(&h, ino).await;

    // Block 0 → protected via ghost-admitted second miss; its publish
    // leaves the tier copy IN PLACE (unlike the matrix test above, which
    // removes it to observe the dehydrate arm).
    let d = read_at(&h, ino, 0, 64 * 1024).await; // touch 1: record
    assert!(d.iter().all(|&x| x == 1));
    h.fs.router.cache.hot_block.remove(map.get(&0).unwrap());
    let d = read_at(&h, ino, 128 * 1024, 64 * 1024).await; // touch 2: admit+publish
    assert!(d.iter().all(|&x| x == 1));
    assert!(
        tier_has(&h, map.get(&0).unwrap()),
        "fixture: the admitted fill must be tier-resident"
    );

    let skips0 = METRICS.hot_block_dehydrate_skips.load(Ordering::Relaxed);
    // Evict block 0 from hot via probation churn.
    for b in 1..6u64 {
        let d = read_at(&h, ino, b * BS, 64 * 1024).await;
        assert!(d.iter().all(|&x| x == b as u8 + 1), "block {b}");
    }
    let mut skipped = false;
    for _ in 0..30 {
        if METRICS.hot_block_dehydrate_skips.load(Ordering::Relaxed) > skips0 {
            skipped = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        skipped,
        "a tier-resident protected victim must be SKIPPED at the \
         dehydration channel mouth (counter unchanged) — re-writing bytes \
         the tier already holds is duplicate-write churn"
    );
    assert!(
        tier_has(&h, map.get(&0).unwrap()),
        "the tier copy survives untouched"
    );
}

/// The DEFAULT admission mode is `second-touch` — the §5.3 policy — as of
/// PR 5: the R-5 evict-before-consume spiral that forced PR 4's temporary
/// `always` default is closed by the pipeline's per-lane resident-
/// unconsumed accounting + AIMD (pinned in
/// tests/read_prefetch_pipeline_tests.rs phases C/D).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_admission_is_second_touch() {
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");
    let h = make_bs("524288", *b"admission-df4-v3").await;
    assert_eq!(
        h.fs.router.tier_admission,
        squeezefs::routing::TierAdmission::SecondTouch,
        "the default flipped to the design's second-touch policy with PR 5"
    );
}
