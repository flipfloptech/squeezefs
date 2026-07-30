//! R4 — hot-block RAM tier + unified block-key purge
//! (docs/design-read-path.md §5.4 / PR 3).
//!
//! Contracts pinned here:
//! - Fills larger than 256 KiB land in the hot tier (probation) as
//!   `Bytes` refcount clones; NVMe-tier hits are NEVER re-promoted into RAM
//!   (the no-RAM-repromote rationale applies to the new tier identically).
//! - Clock shard: sticky `protected` bit beside the consumable `referenced`
//!   bit — probation entries are first in eviction line; a `get` on a
//!   probation entry promotes it in place (sticky); eviction victims carry
//!   their class out (`EvictClass`), and in PR 3 the dehydration gate is
//!   NOT flipped: all classes still dehydrate (behavior-neutral plumbing —
//!   the ≤256 KiB `read_lru` population dehydrates bit-identically).
//! - `purge_block_key` is the ONLY legal block-key purge and covers all
//!   FOUR tiers: read_lru, hot_block, NVMe disk tier, GDS `.gds_cache`
//!   (filename unified through `get_gds_path` — including prefixed
//!   `be_id://offset` keys); a grep-guard makes forgetting a tier a test
//!   failure, not a code review hope.
//! - Cache-less volumes (no staging dirs) get the striped RAM tier for the
//!   first time; hot budget 0 short-circuits to today's behavior.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::lru::LruCache;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::tiering::memory::{EvictClass, MemoryCache};
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// 512 KiB blocks: above the 256 KiB RAM-LRU gate, so striped fills take
/// the hot-tier path (the production 4 MiB shape's population).
const BS: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make_with(staging: bool, uuid: [u8; 16]) -> H {
    // Default W1 patch posture per test (knob=0 pins must never leak).
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    // R3 (PR 6) fixture pin: this suite's contracts exercise the
    // WHOLE-BLOCK fill/admission/hot-tier machinery with sub-block reads;
    // on passthrough volumes those reads now legitimately take the ranged
    // path (§5.6 granularity policy) and never fill hot/tier at all.
    // Disable ranged dispatch here; the ranged path's own contracts —
    // including its ghost-convergence interplay with admission — live in
    // tests/ranged_read_tests.rs.
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "hotblock_test")
            .await
            .unwrap(),
    );
    let s = staging.then(|| tempdir().unwrap());
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
    make_with(true, *b"hotblock-tier-v3").await
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

async fn block_map_of(h: &H, ino: u64) -> std::sync::Arc<std::collections::HashMap<u32, String>> {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default()
}

/// fsync + purge every current block key through the unified helper —
/// production cold state (also exercises the helper on every fixture).
async fn make_cold(h: &H, ino: u64) -> std::sync::Arc<std::collections::HashMap<u32, String>> {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let map = block_map_of(h, ino).await;
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    for (b, key) in map.iter() {
        assert!(
            h.fs.router.cache.hot_block.get(key).is_none(),
            "fixture: block {b} must start cold in the hot tier"
        );
        assert!(
            h.fs.router.cache.nvme.get_cached_read_block(key).is_none(),
            "fixture: block {b} must start cold in the NVMe tier"
        );
    }
    map
}

// ---------------------------------------------------------------------------
// Clock-shard mechanics (unit level, deterministic single shard)
// ---------------------------------------------------------------------------

/// Probation entries are first in eviction line and cannot displace a
/// protected entry that still has its second chance; victims carry their
/// class out.
#[test]
fn probation_evicts_before_protected_and_classes_are_carried() {
    let cache = MemoryCache::new(40, 1);

    let kp = bytes::Bytes::from("prot");
    let vp = bytes::Bytes::from(vec![0xAAu8; 16]);
    let kq = bytes::Bytes::from("prob");
    let vq = bytes::Bytes::from(vec![0xBBu8; 16]);

    // Protected insert (existing put) + probationary insert.
    assert!(cache.put(kp.clone(), vp.clone()).is_empty());
    assert!(cache.put_probationary(kq.clone(), vq.clone()).is_empty());

    // Pressure with a fresh probationary entry: the probation-never-read
    // entry must be the victim, carried out with its class; the protected
    // entry survives (second chance).
    let kr = bytes::Bytes::from("pro2");
    let vr = bytes::Bytes::from(vec![0xCCu8; 16]);
    let evicted = cache.put_probationary(kr.clone(), vr.clone());
    assert_eq!(evicted.len(), 1, "exactly one victim under pressure");
    assert_eq!(evicted[0].0, kq, "probation-never-read is first in line");
    assert_eq!(evicted[0].1, vq);
    assert!(
        matches!(evicted[0].2, EvictClass::Probation),
        "victim class must be Probation (sticky bit never set)"
    );
    assert!(cache.get(&kp).is_some(), "protected entry survives");
}

/// §5.5 pipeline fills: probation CLASS (source-droppable, never sticky)
/// but WITH the clock's one-lap second chance. An unconsumed speculative
/// fill must not lose the clock race to already-consumed stream residue,
/// whose own sub-read serves re-arm `referenced` — under plain
/// probationary insert (referenced=false) the clock evicts the pipeline's
/// FUTURE to keep the stream's PAST (measured on the bench row-2 shape:
/// 595 hot-evict refetches, 1.145x device overshoot, row 2 at 0.90x
/// row 1 with every other counter clean).
#[test]
fn probationary_referenced_fill_survives_one_lap_but_stays_probation_class() {
    let cache = MemoryCache::new(40, 1);

    // Consumed stream residue: probationary insert, then a non-promoting
    // serve (sub-read consumption) — referenced re-armed, never sticky.
    let ka = bytes::Bytes::from("resi");
    let va = bytes::Bytes::from(vec![0xAAu8; 16]);
    assert!(cache.put_probationary(ka.clone(), va.clone()).is_empty());
    assert_eq!(cache.get_no_promote(&ka), Some(va));

    // Unconsumed pipeline fill: probation class WITH the second chance.
    let kb = bytes::Bytes::from("fill");
    let vb = bytes::Bytes::from(vec![0xBBu8; 16]);
    assert!(cache
        .put_probationary_referenced(kb.clone(), vb.clone())
        .is_empty());

    // Pressure with a plain (referenced=false) probationary entry: BOTH
    // graced entries survive their lap; the ungraced newcomer is the
    // victim. Under plain probationary fills, kb would be evicted here —
    // the evict-before-consume shape.
    let kc = bytes::Bytes::from("newc");
    let vc = bytes::Bytes::from(vec![0xCCu8; 16]);
    let ev1 = cache.put_probationary(kc.clone(), vc.clone());
    assert_eq!(ev1.len(), 1, "exactly one victim under pressure");
    assert_eq!(
        ev1[0].0, kc,
        "the graced fill must survive the lap; the ungraced newcomer goes"
    );

    // Second pressure wave: graces are consumed, oldest-first order rules
    // — the residue (ka) is the victim, the fill (kb) is still resident,
    // and the victim's class is Probation (grace is NOT keep-worthiness:
    // source-drop semantics stay intact).
    let kd = bytes::Bytes::from("new2");
    let vd = bytes::Bytes::from(vec![0xDDu8; 16]);
    let ev2 = cache.put_probationary(kd.clone(), vd.clone());
    assert_eq!(ev2.len(), 1);
    assert_eq!(ev2[0].0, ka, "consumed residue evicts before the fill");
    assert!(
        matches!(ev2[0].2, EvictClass::Probation),
        "the second chance must not manufacture Protected victims — \
         dehydration still drops these at the source"
    );
    assert!(
        cache.get_no_promote(&kb).is_some(),
        "the unconsumed fill outlives consumed residue"
    );
}

/// A `get` on a probation entry promotes it IN PLACE (sticky `protected`),
/// distinct from the clock's consumable `referenced` bit: the promoted
/// entry's eventual eviction reports Protected even though the clock scan
/// consumed its referenced bit on the way out.
#[test]
fn probation_get_promotes_sticky_protected() {
    let cache = MemoryCache::new(40, 1);

    let ka = bytes::Bytes::from("aa");
    let va = bytes::Bytes::from(vec![0x11u8; 16]);
    cache.put_probationary(ka.clone(), va.clone());
    assert_eq!(cache.get(&ka), Some(va.clone()), "probation entry readable");

    // Fill to budget, then two pressure puts: the promoted entry outlives
    // the never-read one, and when it finally goes, its class is
    // Protected (sticky survived the clock scan).
    let kb = bytes::Bytes::from("bb");
    let vb = bytes::Bytes::from(vec![0x22u8; 16]);
    let kc = bytes::Bytes::from("cc");
    let vc = bytes::Bytes::from(vec![0x33u8; 16]);
    let kd = bytes::Bytes::from("dd");
    let vd = bytes::Bytes::from(vec![0x44u8; 16]);

    assert!(cache.put_probationary(kb.clone(), vb.clone()).is_empty());

    let ev1 = cache.put_probationary(kc.clone(), vc.clone());
    assert_eq!(ev1.len(), 1);
    assert_eq!(
        ev1[0].0, kb,
        "never-read probation entry loses to the promoted entry \
         (clock: promoted has referenced=true second chance)"
    );
    assert!(matches!(ev1[0].2, EvictClass::Probation));

    // Round 2: ka's second chance was consumed in round 1, but the
    // never-read probation entry kc sits at the clock hand first.
    let ev2 = cache.put_probationary(kd.clone(), vd.clone());
    assert_eq!(ev2.len(), 1);
    assert_eq!(
        ev2[0].0, kc,
        "never-read probation goes before the promoted entry"
    );
    assert!(matches!(ev2[0].2, EvictClass::Probation));

    // Round 3: ka finally pops with its chance spent — and its CLASS must
    // read Protected: the sticky bit survived the clock scan that consumed
    // `referenced` (the dehydration router reads THIS bit; the consumable
    // bit is false for every victim by construction).
    let ke = bytes::Bytes::from("ee");
    let ve = bytes::Bytes::from(vec![0x55u8; 16]);
    let ev3 = cache.put_probationary(ke.clone(), ve.clone());
    assert_eq!(ev3.len(), 1);
    assert_eq!(
        ev3[0].0, ka,
        "promoted entry evicts after its second chance"
    );
    assert!(
        matches!(ev3[0].2, EvictClass::Protected { .. }),
        "sticky protected must survive the clock scan's referenced-bit \
         consumption"
    );
}

/// The typed eviction channel carries victim classes faithfully at the
/// LruCache layer (PR 3 plumbing; PR 4 flips the WORKER-side gate —
/// pinned in read_tier_admission_tests): `LruCache::put` inserts are
/// protected-class by definition (the ≤256 KiB read_lru population's
/// dehydration behavior is unchanged).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_channel_carries_victim_classes() {
    let lru = LruCache::with_capacity(40);
    let mut rx = lru.take_evict_rx().expect("first take");

    lru.put("k1", bytes::Bytes::from(vec![1u8; 16])); // protected by definition
    lru.put_probationary("k2", bytes::Bytes::from(vec![2u8; 16]));
    lru.put_probationary("k3", bytes::Bytes::from(vec![3u8; 16])); // evicts k2
    lru.put("k4", bytes::Bytes::from(vec![4u8; 16])); // evicts k1 or k3

    let mut seen = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        seen.push(ev);
    }
    assert!(
        seen.len() >= 2,
        "both pressure puts must surface victims on the typed channel \
         (got {})",
        seen.len()
    );
    assert!(
        seen.iter()
            .any(|(_, _, c)| matches!(c, EvictClass::Probation)),
        "a probation victim must be visible on the channel WITH its class \
         — the worker-side gate routes on exactly this information"
    );
    for (k, v, _) in &seen {
        assert!(!k.is_empty() && !v.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Filesystem-level fill / probe / purge contracts
// ---------------------------------------------------------------------------

/// Device-validated fills land in the hot tier; a second sub-block read is
/// a hot hit (no device fetch); NVMe-tier hits never re-promote into RAM.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn validated_fills_land_hot_and_nvme_hits_never_repromote() {
    use squeezefs::fuse_client::METRICS;
    use std::sync::atomic::Ordering;

    let h = make().await;
    let ino = create(&h, "hot_fill").await;
    write_at(&h, ino, 0, &vec![0xD1u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0xD2u8; BS as usize]).await;
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();
    let k1 = map.get(&1).unwrap().clone();

    // Cold read fills BOTH the hot tier (probation, refcount clone) and —
    // in PR 3, admission untouched — the NVMe tier.
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let d = read_at(&h, ino, 0, 128 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xD1));
    assert_eq!(METRICS.get_obj.load(Ordering::Relaxed) - g0, 1);
    assert!(
        h.fs.router.cache.hot_block.get(&k0).is_some(),
        "a >256 KiB device-validated fill must land in the hot tier"
    );

    // Second sub-block read: hot hit — zero device fetches.
    let h0 = METRICS.hot_block_hits.load(Ordering::Relaxed);
    let d = read_at(&h, ino, 128 * 1024, 128 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xD1));
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        1,
        "hot-resident block must serve without a device fetch"
    );
    assert!(
        METRICS.hot_block_hits.load(Ordering::Relaxed) > h0,
        "hot_block_hits is the adoption signal"
    );

    // No-repromote: block 1 present ONLY in the NVMe tier — seed the tier
    // directly (under R1b a first-touch fill no longer publishes), then
    // read: served from the tier, hot stays empty.
    let block1 = read_at(&h, ino, BS, BS as u32).await; // fills hot
    h.fs.router.cache.hot_block.remove(&k1); // leave no RAM copy
    let _ =
        h.fs.router
            .cache
            .nvme
            .cache_read_block(&k1, bytes::Bytes::from(block1.clone()));
    assert!(h.fs.router.cache.nvme.get_cached_read_block(&k1).is_some());
    let d = read_at(&h, ino, BS + 64 * 1024, 64 * 1024).await;
    assert_eq!(&d[..], &block1[64 * 1024..128 * 1024]);
    assert!(
        h.fs.router.cache.hot_block.get(&k1).is_none(),
        "an NVMe-tier hit must NOT re-promote into the hot tier — tier \
         entry provenance cannot be proven by the incarnation word alone \
         (the no-RAM-repromote rationale, applied to the new tier)"
    );
}

/// Overwriting a striped block displaces + frees its old key: the unified
/// purge must leave NO tier serving the dead incarnation — hot included —
/// and the next read serves the new bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn displaced_key_purges_hot_tier_and_reads_serve_new_bytes() {
    let h = make().await;
    let ino = create(&h, "hot_displace").await;
    write_at(&h, ino, 0, &vec![0x0Au8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0x0Fu8; BS as usize]).await; // striped layout
    let map = make_cold(&h, ino).await;
    let k_old = map.get(&0).unwrap().clone();

    // Warm the hot tier with the old incarnation.
    let d = read_at(&h, ino, 0, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0x0A));
    assert!(h.fs.router.cache.hot_block.get(&k_old).is_some());

    // Full-block overwrite: write-through displaces the old key and frees
    // it — the free-side purge (read_tier_purge -> purge_block_key) must
    // evict the hot entry too.
    write_at(&h, ino, 0, &vec![0x0Bu8; BS as usize]).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.router.cache.hot_block.get(&k_old).is_none(),
        "a freed block key must be purged from the HOT tier — a reused \
         offset string would otherwise serve the dead incarnation's bytes \
         (the 074 stale-fill family, fourth-tier edition)"
    );
    let d = read_at(&h, ino, 0, 64 * 1024).await;
    assert!(
        d.iter().all(|&x| x == 0x0B),
        "post-displace read must serve the new incarnation"
    );
}

/// A hot entry whose block-index binding no longer names its key is never
/// served: reads after a rebind return the CURRENT map's bytes even if a
/// stale hot entry is manually re-inserted under the dead key (simulating
/// a raced fill landing after the purge).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_hot_entry_under_dead_key_is_never_served() {
    let h = make().await;
    // Pin the CoW displace/free machinery this test exists for: at this
    // sandbox block size a whole-block aligned overwrite is W1
    // patch-ELIGIBLE (in place, same key — no displacement), which
    // starves the machinery under test. On real 4 MiB-block volumes the
    // 512 KiB patch cap forbids block-covering patches, so the pinned
    // machinery is the production whole-block-overwrite path;
    // extent_patch_tests owns the patched-shape twin contracts. The
    // write-wall iteration-1 in-place-overwrite default (the same law's
    // whole-block face) is pinned off for the same reason —
    // inplace_overwrite_tests owns its twin.
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::fuse_client::set_inplace_overwrite(false);
    let ino = create(&h, "hot_stale").await;
    write_at(&h, ino, 0, &vec![0x21u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0x2Fu8; BS as usize]).await; // striped layout
    let map = make_cold(&h, ino).await;
    let k_old = map.get(&0).unwrap().clone();

    write_at(&h, ino, 0, &vec![0x22u8; BS as usize]).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let map2 = block_map_of(&h, ino).await;
    let k_new = map2.get(&0).unwrap().clone();
    assert_ne!(k_old, k_new, "overwrite must displace the key");

    // Adversarial: park stale bytes in the hot tier under the DEAD key.
    h.fs.router.cache.hot_block.put("", bytes::Bytes::new()); // no-op guard: API sanity
    h.fs.router
        .cache
        .hot_block
        .put(&k_old, bytes::Bytes::from(vec![0x21u8; BS as usize]));

    let d = read_at(&h, ino, 0, 64 * 1024).await;
    assert!(
        d.iter().all(|&x| x == 0x22),
        "resolution goes through the CURRENT map + binding recheck — a \
         stale hot entry under a dead key must be unreachable"
    );
    squeezefs::fuse_client::set_inplace_overwrite(true);
}

/// Cache-less volumes (no staging dirs): striped blocks get a RAM tier for
/// the FIRST time — re-reads stop hitting the device.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_less_volume_gets_hot_tier() {
    use squeezefs::fuse_client::METRICS;
    use std::sync::atomic::Ordering;

    let h = make_with(false, *b"hotblock-noca-v3").await;
    let ino = create(&h, "hot_nocache").await;
    write_at(&h, ino, 0, &vec![0x31u8; BS as usize]).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let map = block_map_of(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();
    h.fs.router.cache.purge_block_key(&k0);

    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let d = read_at(&h, ino, 0, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0x31));
    let after_first = METRICS.get_obj.load(Ordering::Relaxed) - g0;
    assert_eq!(after_first, 1, "cold fill");
    let d = read_at(&h, ino, 128 * 1024, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0x31));
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        1,
        "cache-less volumes historically refetched every sub-read; the hot \
         tier must serve the re-read at RAM speed"
    );
}

/// Hot budget 0 short-circuits: no hot entries, behavior = today's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hot_budget_zero_short_circuits() {
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "0");
    let h = make_with(true, *b"hotblock-zero-v3").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");

    let ino = create(&h, "hot_zero").await;
    write_at(&h, ino, 0, &vec![0x41u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0x4Fu8; BS as usize]).await; // striped layout
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();

    let d = read_at(&h, ino, 0, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0x41));
    assert!(
        h.fs.router.cache.hot_block.get(&k0).is_none(),
        "budget 0: the hot tier must hold nothing"
    );
    // NVMe tier still works (publish untouched in PR 3).
    assert!(h.fs.router.cache.nvme.get_cached_read_block(&k0).is_some());
}

// ---------------------------------------------------------------------------
// The unified four-tier purge + GDS unification
// ---------------------------------------------------------------------------

/// `purge_block_key` covers all four tiers — including the GDS
/// `.gds_cache` file for PREFIXED block keys (`be_id://offset`), whose two
/// historical filename constructions diverged (`get_gds_path` vs
/// `read_direct`'s inline sanitizer). Post-unification there is exactly one
/// name, and the purge unlinks it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_block_key_unlinks_gds_cache_including_prefixed_keys() {
    let h = make().await;
    let cache = &h.fs.router.cache;

    for key in ["1048576", "nvme1://123456"] {
        let gds_path = cache
            .gds
            .get_gds_path(key)
            .expect("staging dir present => gds path resolvable");
        std::fs::write(&gds_path, b"stale gds payload").unwrap();
        assert!(gds_path.exists());

        cache.read_lru.put(key, bytes::Bytes::from_static(b"x"));
        cache.hot_block.put(key, bytes::Bytes::from_static(b"y"));

        cache.purge_block_key(key);

        assert!(
            !gds_path.exists(),
            "purge_block_key must unlink the GDS cache file for {key} — \
             the fourth block-key tier cannot be forgotten"
        );
        assert!(cache.read_lru.get(key).is_none());
        assert!(cache.hot_block.get(key).is_none());
        assert!(cache.nvme.get_cached_read_block(key).is_none());
    }

    // Idempotent on absence (ENOENT ignored).
    cache.purge_block_key("nvme1://123456");
}

/// Grep-guard (the R-6 structural mitigation): outside the unified helper
/// and the whitelisted publisher-local undo sites, production code must not
/// hand-roll block-key tier removals or build `.gds_cache` names.
#[test]
fn purge_census_grep_guard() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();

    fn visit(dir: &std::path::Path, out: &mut Vec<(String, usize, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                visit(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                let rel = p.to_string_lossy().to_string();
                let text = std::fs::read_to_string(&p).unwrap();
                for (i, line) in text.lines().enumerate() {
                    let t = line.trim();
                    if t.starts_with("//") || t.starts_with("///") {
                        continue;
                    }
                    if t.contains("remove_cached_read_block(") || t.contains(".gds_cache") {
                        out.push((rel.clone(), i + 1, t.to_string()));
                    }
                    // read_lru removals are legal only for whole-FILE keys
                    // (file_path namespace); block-key removals must route
                    // through purge_block_key.
                    if t.contains("read_lru.remove(") && !t.contains("file_path") {
                        out.push((rel.clone(), i + 1, t.to_string()));
                    }
                }
            }
        }
    }
    visit(&root, &mut offenders);

    let whitelisted = |f: &str, line: &str| -> bool {
        // The helper itself (cache/mod.rs) and the GdsCache implementation.
        if f.ends_with("cache/mod.rs") || f.ends_with("cache/gds.rs") {
            return true;
        }
        // NvmeStaging: the method definitions + the validated non-owner
        // publish's SELF-undo (it un-does only its own single-tier put —
        // nothing else was published at that point) + the mount-time
        // .gds_cache wipe sweep.
        if f.ends_with("cache/nvme.rs") {
            return true;
        }
        // routing.rs publisher-local undo inside the awaited publish
        // closure: undoes only the put it just made (no other tier has the
        // fill yet at that point).
        if f.ends_with("routing.rs") && line.contains("nvme_clone.remove_cached_read_block") {
            return true;
        }
        false
    };

    let offenders: Vec<_> = offenders
        .into_iter()
        .filter(|(f, _, l)| !whitelisted(f, l))
        .collect();
    assert!(
        offenders.is_empty(),
        "block-key tier purges must route through TieredCache::purge_block_key \
         (all four tiers — the 074 lesson). Offenders:\n{}",
        offenders
            .iter()
            .map(|(f, n, l)| format!("  {f}:{n}: {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// GDS filename unification: one construction function for every producer.
#[test]
fn gds_filename_construction_is_unified() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cache/gds.rs"),
    )
    .unwrap();
    let inline_sanitizers = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.contains("replace(['/', ':']"))
        .count();
    assert_eq!(
        inline_sanitizers, 0,
        "read_direct's divergent inline .gds_cache name construction must \
         be deleted — get_gds_path is the one source of truth (prefixed \
         be_id://offset keys otherwise produce two names and the purge \
         misses the served copy)"
    );
}
