//! The staged zeros-LOSS family (follow-up to C, pre-existing on e1601bc —
//! aged fsx: acked bytes read back as ZEROS, sub-second onset, GOOD→0x0000):
//!
//! `DataRouter::fetch_metadata` refreshes a hot layout-cache entry older
//! than 1 s from the BACKEND — but a staged write's layout/size live only
//! in RAM (+ the staging ring) until the fsync/flush/promotion cadence
//! persists them (`layout_dirty: true`). The refill therefore CLOBBERS the
//! only authoritative copy of the layout with a stale snapshot:
//!
//! * backend has NO layout yet (never fsynced/promoted — an aged daemon's
//!   merge queue is saturated with aged-file entries, so the deferred
//!   persist never lands): the refill installs the DEFAULT INLINE
//!   `size = 0` entry. Reads clamp to it → every acked byte reads zeros;
//!   the next RMW takes the empty-file fast path under a FRESH file_id →
//!   the whole image is durably lost.
//! * backend has a PROMOTION-ERA layout: the refill regresses `size` (and
//!   resurrects a released `block_map`) → the tail beyond the stale size
//!   reads zeros — and because the clobber also drops `layout_dirty`, a
//!   later fsync persists NOTHING, making the regression durable.
//!
//! Contract pinned here: a `layout_dirty` hot entry is the LOCAL AUTHORITY
//! (writes keep it coherent under the per-inode locks; the DLM lease means
//! no remote writer can legitimately race it) — the TTL refresh may only
//! replace CLEAN entries. Acked staged bytes must read back exactly across
//! the TTL boundary, across an RMW after it, and durably across
//! fsync + a cold cache re-read.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// Default 4 MiB blocks; a LARGE staging budget so promotion never fires —
/// isolating the deferred-persist window (the aged shape: the backend never
/// sees the staged layout).
async fn make(uuid: [u8; 16], alloc_ns: &str, budget: &str) -> H {
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some(budget),
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

async fn read_all(h: &H, ino: u64, len: usize) -> Vec<u8> {
    let r = h.fs.read(h.req, ino, 0, 0, len as u32, 0).await.unwrap();
    r.data.to_vec()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 | 1).collect()
}

fn assert_bytes(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: length mismatch");
    if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
        let zeros = got.iter().filter(|&&b| b == 0).count();
        panic!(
            "{what}: first mismatch at offset {i}: got 0x{:02x}, want 0x{:02x} \
             ({zeros}/{} bytes zero — acked bytes reading zeros is the LOSS class)",
            got[i],
            want[i],
            got.len()
        );
    }
}

/// Cross the layout-cache TTL WITHOUT any cache-inserting op — the read-only
/// stretch of the aged fsx storms. This sleep is the TRIGGER under test (the
/// 1 s freshness horizon in `fetch_metadata`), not synchronization.
async fn cross_ttl() {
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
}

/// Leg 1 — pure read after the TTL with an unpersisted staged layout: the
/// refill must not clobber the dirty RAM layout with the backend's nothing
/// (default inline size=0) — every acked byte still reads back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_acked_bytes_survive_ttl_refill_read() {
    const LEN: usize = 600 * 1024;
    let h = make(*b"szl-refill-read1", "szl_ns_a", "128MB").await;
    let ino = create(&h, "ttl_read").await;
    let pat = pattern(LEN);
    write_at(&h, ino, 0, &pat).await;

    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "staged", "fixture must exercise STAGED");
    assert!(
        m.layout_dirty,
        "fixture self-check: the staged layout must be RAM-only (deferred persist)"
    );

    cross_ttl().await;

    let got = read_all(&h, ino, LEN).await;
    assert_bytes(&got, &pat, "read across the TTL refill");

    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(
        m.size, LEN as u64,
        "the TTL refill must not regress the dirty layout size"
    );
}

/// Leg 2 — RMW after the TTL: the seed must still see the acked image (the
/// clobbered empty-file fast path re-staged ONLY the patch under a fresh
/// file_id — total loss).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_rmw_after_ttl_refill_preserves_image() {
    const LEN: usize = 600 * 1024;
    const WOFF: u64 = 4096;
    const WLEN: usize = 4096;
    let h = make(*b"szl-refill-rmw01", "szl_ns_b", "128MB").await;
    let ino = create(&h, "ttl_rmw").await;
    let pat = pattern(LEN);
    write_at(&h, ino, 0, &pat).await;

    cross_ttl().await;

    write_at(&h, ino, WOFF, &[0xEE; WLEN]).await;

    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(
        m.size, LEN as u64,
        "an in-bounds RMW after the TTL must not shrink the file to the patch"
    );

    let mut want = pat.clone();
    want[WOFF as usize..WOFF as usize + WLEN].fill(0xEE);
    let got = read_all(&h, ino, LEN).await;
    assert_bytes(&got, &want, "RMW across the TTL refill");
}

/// Leg 3 — durability cleave: after the TTL crossing, fsync must still
/// persist the TRUE layout (the clobber dropped `layout_dirty`, so fsync
/// persisted nothing and a cold-cache re-read served the stale snapshot).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_layout_persists_after_ttl_refill_fsync_cold_reread() {
    const LEN: usize = 600 * 1024;
    let h = make(*b"szl-refill-cold1", "szl_ns_c", "128MB").await;
    let ino = create(&h, "ttl_cold").await;
    let pat = pattern(LEN);
    write_at(&h, ino, 0, &pat).await;

    cross_ttl().await;
    // Touch the meta through the refill horizon (a read is enough), then
    // fsync — the dirty layout must still reach the backend.
    let _ = read_all(&h, ino, 16).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();

    // Cold cache: drop the hot entry so the next read resolves the layout
    // from the persisted backend (the remount-equivalent for the layout).
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.invalidate(&path);

    let got = read_all(&h, ino, LEN).await;
    assert_bytes(&got, &pat, "cold re-read after TTL + fsync");
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.size, LEN as u64, "persisted size must be the true size");
}

/// Leg 4 — the promotion-era regression: once the file HAS a persisted
/// (promotion-era) layout, later unpersisted RMWs must not be regressed by
/// the TTL refill (size rolls back → tail beyond it reads zeros).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_growth_survives_ttl_refill_after_promotion() {
    const L1: usize = 800 * 1024; // > high water of a 1 MiB ring => promoted
    const L2: usize = 1200 * 1024; // grown afterwards, RAM-only
    let h = make(*b"szl-refill-promo", "szl_ns_d", "1MB").await;
    let ino = create(&h, "ttl_promo").await;
    let path = squeezefs::keys::inode_path(ino);

    let pat1 = pattern(L1);
    write_at(&h, ino, 0, &pat1).await;
    let file_id = h
        .fs
        .router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .file_id
        .expect("staged file_id");

    // Wait until the merge worker promotes (publishes block_map[0], persists
    // the promotion-era layout, releases the ring entry).
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let promoted = h
                .fs
                .router
                .metadata_cache
                .get(&path)
                .and_then(|m| m.block_map.as_ref().and_then(|bm| bm.get(&0).cloned()))
                .is_some();
            if promoted && h.fs.router.cache.nvme.read_staged(&file_id).is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("staged blob promoted to a durable block");

    // Grow the file (re-stages the ring; layout/size RAM-only again).
    let mut pat2 = pat1.clone();
    pat2.resize(L2, 0);
    for (i, b) in pat2.iter_mut().enumerate().skip(L1) {
        *b = ((i % 251) as u8) | 1;
    }
    write_at(&h, ino, L1 as u64, &pat2[L1..]).await;

    cross_ttl().await;

    let got = read_all(&h, ino, L2).await;
    assert_bytes(&got, &pat2, "grown staged image across the TTL refill");
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(
        m.size, L2 as u64,
        "the TTL refill must not roll the size back to the promotion era"
    );
}
