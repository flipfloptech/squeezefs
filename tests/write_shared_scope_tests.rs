//! PR 3 of `docs/design-write-inode-convoy.md` — the §4.2/KD-2 acquire
//! protocol, red-first.
//!
//! Contracts pinned here:
//! - Engagement: the diagnosis-row shape (fully-mapped within-EOF striped
//!   overwrite, warm caches, posture ON) DISPATCHES on the shared (read)
//!   guard — `write_lock_scope_shared` counts it and the bytes round-trip.
//! - KD-8 closure: Σ final-scope ≡ Σ candidate ≡ classified writes, with
//!   candidates and final scopes agreeing per class on quiet schedules.
//! - KD-2 upgrade: a truncate landing between the pre-lock probe and the
//!   held-guard revalidation forces exactly ONE upgrade to exclusive; the
//!   op completes on the exclusive path and counts its post-upgrade class.
//! - KD-7: a cold-cache write is unreachable for Shared (miss ⇒ exclusive
//!   acquisition), posture notwithstanding.
//! - Posture: `SQUEEZEFS_WRITE_SHARED=0` (the A/B lever) keeps the
//!   candidate ledger counting while every dispatch stays exclusive.
//! - A/B byte-identity: the same workload with the posture ON and OFF
//!   reads back identically.
//!
//! RED (PR 3 seam commit): the final-scope ledger exists but the handler
//! still maps Shared → MetaPrepOnly behavior (PR 1's preview posture), so
//! every engagement/closure/upgrade row fails; the posture-OFF and
//! byte-identity rows pin current behavior.

use fuse3::raw::prelude::Filesystem;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{set_write_shared_for_tests, SqueezefsFilesystem, METRICS};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

const BS: u64 = 65536;

/// Suite serializer: the posture override and the global scope counters
/// are process-wide.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// RAII posture reset (leave the shipped defaults for other binaries).
struct PostureReset;
impl Drop for PostureReset {
    fn drop(&mut self) {
        set_write_shared_for_tests(true);
        squeezefs::fuse_client::set_write_guard_narrow_for_tests(true);
    }
}

async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3")
}

struct H {
    fs: SqueezefsFilesystem,
    req: fuse3::raw::Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make(tag: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    b.as_file().set_len(256 * 1024 * 1024).unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 128 * 1024 * 1024).await,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = fuse3::raw::Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
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

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) -> u32 {
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
    .unwrap_or_else(|e| panic!("write off {off} failed: {e:?}"))
    .written
}

/// Grow striped + durable + quiet + warm caches (the diagnosis fixture).
async fn grow_striped(h: &H, ino: u64, blocks: u64) {
    let base = vec![0x11u8; (blocks * BS) as usize];
    write_at(h, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline drains"
    );
    let _ = h.fs.getattr(h.req, ino, None, 0).await.expect("getattr");
}

struct ScopeSnap {
    cand: [u64; 3],
    scope: [u64; 3],
    upgrades: u64,
}

fn snap() -> ScopeSnap {
    ScopeSnap {
        cand: [
            METRICS.write_lock_candidate_shared.load(Ordering::Relaxed),
            METRICS
                .write_lock_candidate_metaprep
                .load(Ordering::Relaxed),
            METRICS.write_lock_candidate_entire.load(Ordering::Relaxed),
        ],
        scope: [
            METRICS.write_lock_scope_shared.load(Ordering::Relaxed),
            METRICS.write_lock_scope_metaprep.load(Ordering::Relaxed),
            METRICS.write_lock_scope_entire.load(Ordering::Relaxed),
        ],
        upgrades: METRICS
            .write_lock_scope_shared_upgrades
            .load(Ordering::Relaxed),
    }
}

fn delta(a: &ScopeSnap, b: &ScopeSnap) -> ScopeSnap {
    ScopeSnap {
        cand: [
            b.cand[0] - a.cand[0],
            b.cand[1] - a.cand[1],
            b.cand[2] - a.cand[2],
        ],
        scope: [
            b.scope[0] - a.scope[0],
            b.scope[1] - a.scope[1],
            b.scope[2] - a.scope[2],
        ],
        upgrades: b.upgrades - a.upgrades,
    }
}

/// Engagement: the diagnosis-row shape dispatches ON the shared guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mapped_overwrite_dispatches_on_the_shared_guard() {
    let _s = SERIAL.lock().await;
    let _reset = PostureReset;
    set_write_shared_for_tests(true);
    let h = make("wss_engage").await;
    let ino = create(&h, "e").await;
    grow_striped(&h, ino, 4).await;

    let s0 = snap();
    let data = vec![0x5Au8; 8192];
    assert_eq!(write_at(&h, ino, BS, &data).await as usize, data.len());
    let d = delta(&s0, &snap());
    assert_eq!(
        d.cand[0], 1,
        "the shape must classify as a Shared candidate (PR 1 ledger)"
    );
    assert_eq!(
        d.scope[0], 1,
        "the shape must DISPATCH on the shared guard (KD-8 final scope — \
         the engagement instrument)"
    );
    assert_eq!(d.upgrades, 0, "no truncate raced: no upgrade");

    // Byte-identity through the Shared dispatch.
    let back =
        h.fs.read(h.req, ino, 0, BS, 8192, 0)
            .await
            .unwrap()
            .data
            .to_vec();
    assert_eq!(back, data, "Shared-dispatched bytes serve");
}

/// KD-8 closure: Σ final-scope ≡ Σ candidate ≡ classified writes, and the
/// classes agree per shape on a quiet (unraced) schedule. Pinned on the
/// convoy campaign's v1 class (W-2's `SQUEEZEFS_WRITE_GUARD_NARROW=0` A/B
/// leg, where extends are MetaPrepOnly); the narrowed default's per-shape
/// rows live in `tests/write_stream_guard_tests.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scope_ledger_closes_against_classified_writes() {
    let _s = SERIAL.lock().await;
    let _reset = PostureReset;
    set_write_shared_for_tests(true);
    squeezefs::fuse_client::set_write_guard_narrow_for_tests(false);
    let h = make("wss_closure").await;
    let ino = create(&h, "c").await;
    grow_striped(&h, ino, 4).await;

    let s0 = snap();
    // 5 Shared-shaped (within-EOF mapped), 3 extending (MetaPrepOnly on
    // the v1 class), plus one fresh inline file (EntireOp).
    for i in 0..5u64 {
        write_at(&h, ino, i * 4096, &[0x22u8; 4096]).await;
    }
    for i in 0..3u64 {
        write_at(&h, ino, (4 + i) * BS, &[0x33u8; 4096]).await;
    }
    let ino2 = create(&h, "tiny").await;
    write_at(&h, ino2, 0, &[0x44u8; 512]).await;

    let d = delta(&s0, &snap());
    let cand_sum: u64 = d.cand.iter().sum();
    let scope_sum: u64 = d.scope.iter().sum();
    assert_eq!(cand_sum, 9, "9 writes classified");
    assert_eq!(
        scope_sum, 9,
        "every classified write must count exactly one FINAL scope (KD-8 \
         closure)"
    );
    assert_eq!(
        d.scope, d.cand,
        "on an unraced schedule the final scope agrees with the candidate \
         per class (upgrades would move counts between them)"
    );
    assert_eq!(d.scope[0], 5, "the five mapped overwrites ran Shared");
    assert_eq!(
        d.scope[1], 3,
        "the three extends ran MetaPrepOnly (v1 class)"
    );
    assert_eq!(d.upgrades, 0);
}

/// KD-2: a truncate landing between the pre-lock probe and the held-guard
/// revalidation forces exactly ONE upgrade; the op completes on the
/// exclusive path and counts its post-upgrade class.
///
/// Deterministic schedule: the test holds the ino's EXCLUSIVE guard
/// directly (parking the writer's read acquisition behind it — tokio
/// RwLock is write-preferring), mutates both caches to the post-truncate
/// state a real truncate publishes before dropping its guard, then
/// releases. The writer's revalidation sees end > floor ⇒ upgrade.
///
/// Pinned on the v1 class (W-2's `SQUEEZEFS_WRITE_GUARD_NARROW=0` leg):
/// under the narrowed default an extend is itself Shared, so a truncate
/// no longer moves the verdict — the narrowed class's revalidation
/// failure is the snapshot EVICTION, pinned in
/// `tests/write_stream_guard_tests.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncate_between_probe_and_guard_upgrades_exactly_once() {
    let _s = SERIAL.lock().await;
    let _reset = PostureReset;
    set_write_shared_for_tests(true);
    squeezefs::fuse_client::set_write_guard_narrow_for_tests(false);
    let h = make("wss_upgrade").await;
    let ino = create(&h, "u").await;
    grow_striped(&h, ino, 4).await;

    // Hold the exclusive guard: the writer's probe (pre-lock) will still
    // see the 4·BS world, but its read acquisition parks behind us.
    let lock = h.fs.active_inode_locks.get_inode_lock(ino);
    let held = lock.write().await;

    let s0 = snap();
    let fs_w = h.fs.clone();
    let req = h.req;
    // Within-EOF-of-the-OLD-world write at block 2.
    let w = tokio::spawn(async move {
        fs_w.write(
            req,
            ino,
            0,
            2 * BS,
            bytes::Bytes::from_static(&[0x77u8; 4096]),
            0,
            0,
        )
        .await
    });
    // Let the writer run its pre-lock probe and park on the guard.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Publish the post-truncate world into BOTH caches — exactly the
    // state a real truncate leaves before dropping its exclusive guard
    // (§4.1.2: truncate publishes to both caches before the drop).
    if let Some(mut m) = h.fs.router.metadata_cache.get(&ino) {
        m.size = BS;
        m.cached_at = std::time::Instant::now();
        h.fs.router.metadata_cache.insert(ino, m);
    }
    if let Some((mut attr, _)) = h.fs.attr_cache.get(&ino) {
        attr.size = BS;
        attr.blocks = BS.div_ceil(512);
        h.fs.attr_cache
            .insert(ino, (attr, std::time::Instant::now()));
    }
    drop(held);

    let written = w.await.unwrap().expect("upgraded write completes").written;
    assert_eq!(written, 4096);
    let d = delta(&s0, &snap());
    assert_eq!(
        d.upgrades, 1,
        "revalidation must fail once and take the ONE upgrade (KD-2)"
    );
    assert_eq!(
        d.scope[0], 0,
        "an upgraded op never counts a Shared final scope"
    );
    assert_eq!(
        d.scope[1] + d.scope[2],
        1,
        "the upgraded op counts its post-upgrade exclusive class"
    );
}

/// KD-7: a cold-cache write is unreachable for Shared — the probe's miss
/// routes to the exclusive acquisition, where the lifecycle probe and the
/// fetch-capable classification run exactly as today.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_cache_write_routes_exclusive() {
    let _s = SERIAL.lock().await;
    let _reset = PostureReset;
    set_write_shared_for_tests(true);
    let h = make("wss_cold").await;
    let ino = create(&h, "k").await;
    grow_striped(&h, ino, 4).await;

    // Kill both RAM caches: the Shared class must become unreachable.
    h.fs.router.metadata_cache.invalidate(&ino);
    h.fs.attr_cache.invalidate(&ino);

    let s0 = snap();
    assert_eq!(write_at(&h, ino, BS, &[0x66u8; 4096]).await, 4096);
    let d = delta(&s0, &snap());
    assert_eq!(
        d.scope[0], 0,
        "a cache-miss write must not dispatch Shared (KD-2 miss ⇒ \
         exclusive; KD-7 lifecycle order preserved)"
    );
    assert_eq!(
        d.upgrades, 0,
        "miss routes exclusive at ADMISSION, not via upgrade"
    );
    assert_eq!(
        d.scope.iter().sum::<u64>(),
        1,
        "the write still counts exactly one exclusive-class final scope"
    );
}

/// The A/B lever: posture OFF keeps candidates counting while every
/// dispatch stays exclusive — and the bytes are identical either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn posture_off_maps_shared_candidates_to_exclusive_dispatch() {
    let _s = SERIAL.lock().await;
    let _reset = PostureReset;
    set_write_shared_for_tests(false);
    let h = make("wss_off").await;
    let ino = create(&h, "o").await;
    grow_striped(&h, ino, 4).await;

    let s0 = snap();
    let data = vec![0x5Au8; 8192];
    assert_eq!(write_at(&h, ino, BS, &data).await as usize, data.len());
    let d = delta(&s0, &snap());
    assert_eq!(
        d.cand[0], 1,
        "candidates count regardless of the posture (KD-8 preview ledger)"
    );
    assert_eq!(d.scope[0], 0, "posture OFF: no Shared dispatch");
    assert_eq!(
        d.scope[1], 1,
        "the candidate executes on the exclusive drop-before-I/O class"
    );
    let back =
        h.fs.read(h.req, ino, 0, BS, 8192, 0)
            .await
            .unwrap()
            .data
            .to_vec();
    assert_eq!(back, data, "A/B byte-identity (posture OFF)");
}
