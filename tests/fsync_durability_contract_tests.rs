//! DUR-1 — `fsync()` must make partially-covered striped blocks durable
//! (pre-RC engineering spec §1 DUR-1, P0).
//!
//! **The bug.** `flush_inode_to_backend` calls
//! `flush_memory_buffers_for_inode` FIRST. For a block whose written
//! coverage is partial that takes the staging leg: seed → `put_active_block`
//! → `retire_parked_overlay` (emptying `active_block_buffers`) →
//! `enqueue_writeback`. `flush_active_blocks_with_retry` then builds its
//! work list **exclusively from that now-empty map**, finds nothing, and
//! returns — so `flush_one_active_block`, the function that DMAs the staged
//! bytes to the data device and merges the block map, is never invoked.
//! `fsync` returns success with the block's bytes living only in a staging
//! segment and a queued writeback request.
//!
//! Two more links in the same chain: the data half syncs only the `file_id`
//! key (never `active_block:` keys), and `sync_data_fut` / `sync_meta_fut`
//! were joined with `tokio::try_join!` — **concurrently** — so there was no
//! ordering edge between the data sync and the metadata barrier that names
//! its blocks.
//!
//! `tests/staged_crash_recovery_tests.rs` states this contract verbatim in
//! its own header and does not catch the violation: every leg there is
//! SIGKILL, where the staging file's page cache survives.
//!
//! Acceptance (spec): write a partial block, `flush_inode_to_backend`, then
//! assert the block map names a **published device key** and
//! `staged_writes_in_flight == 0` for that ino. Plus the ordering leg: the
//! data barrier completes strictly before the metadata barrier.

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
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Downscaled block size (the `write_through_coverage_tests` convention):
/// keeps the ledger deltas exact and the images cheap. "1 MiB into a 4 MiB
/// block" is HALF into BS here — the same partial-coverage shape.
const BS: u64 = 65536;
const HALF: u64 = BS / 2;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // Partial-coverage overwrites must ride the staging/flush machinery
    // this suite pins, not the W1 in-place patch (at production block
    // sizes a 1 MiB write into a 4 MiB block is patch-oversize anyway).
    squeezefs::fuse_client::set_patch_max_bytes(0);
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
        Some("64MB"),
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
            hash_seed: 0xD0DE_BEEF_0000_0001,
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
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 ^ tag | 1).collect()
}

async fn block_map(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .block_map
        .map(|m| (*m).clone())
        .unwrap_or_default()
}

fn staged_in_flight(h: &H) -> usize {
    h.fs.router
        .cache
        .nvme
        .staged_writes_in_flight
        .load(Ordering::SeqCst)
}

/// A durable striped file: 6 blocks written and fsynced (map published).
async fn durable_striped(h: &H, name: &str, tag: u8) -> (u64, Vec<u8>) {
    let ino = create(h, name).await;
    let base = pattern(6 * BS as usize, tag);
    write_at(h, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    (ino, base)
}

/// **The DUR-1 acceptance leg.** A partially-covered overwrite of a
/// durable striped block must be on the data device — named by the block
/// map — when `flush_inode_to_backend` returns, with nothing left staged.
///
/// RED before the fix: the staging leg empties `active_block_buffers`, so
/// `flush_active_blocks_with_retry` finds an empty work list and returns
/// at its early exit; the bytes sit in staging behind a queued writeback.
#[tokio::test]
async fn test_fsync_publishes_a_partially_covered_striped_block() {
    let h = make([0x11; 16], "dur1_partial").await;
    let (ino, _base) = durable_striped(&h, "partial.bin", 0x5A).await;

    let before = block_map(&h, ino).await;
    let old_key = before.get(&2).cloned().expect("block 2 published by setup");

    // Partial coverage: HALF of block 2 (the "1 MiB into a 4 MiB block"
    // shape at this suite's downscaled geometry).
    let patch = pattern(HALF as usize, 0xC3);
    write_at(&h, ino, 2 * BS, &patch).await;

    let token = h.fs.dlm().get_fencing_token_ino(ino);
    h.fs.flush_inode_to_backend(ino, token)
        .await
        .expect("flush_inode_to_backend");

    assert_eq!(
        staged_in_flight(&h),
        0,
        "fsync returned with staged writes still in flight — the block's \
         acked bytes are only in a staging segment behind a queued writeback"
    );

    let after = block_map(&h, ino).await;
    let new_key = after.get(&2).cloned().expect("block 2 must stay mapped");
    assert_ne!(
        new_key, old_key,
        "the block map must name a NEWLY published device key for the \
         rewritten block (CoW publish); it still names the pre-write key, \
         so the acked bytes are not durable"
    );

    // The published key must actually hold the merged image.
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.cache.write_lru.remove(&path);
    h.fs.router.cache.read_lru.remove(&path);
    h.fs.router.cache.purge_block_key(&new_key);
    let got =
        h.fs.read(h.req, ino, 0, 2 * BS, HALF as u32, 0)
            .await
            .unwrap()
            .data
            .to_vec();
    assert_eq!(got, patch, "the published block must hold the acked bytes");
}

/// The same contract for a block that has no predecessor: a partial write
/// into a fresh block index must be published by `fsync`, not left staged.
#[tokio::test]
async fn test_fsync_publishes_a_fresh_partially_written_block() {
    let h = make([0x12; 16], "dur1_fresh").await;
    let (ino, _base) = durable_striped(&h, "fresh.bin", 0x27).await;

    let patch = pattern(HALF as usize, 0x9E);
    write_at(&h, ino, 6 * BS, &patch).await; // extends into a fresh block 6

    let token = h.fs.dlm().get_fencing_token_ino(ino);
    h.fs.flush_inode_to_backend(ino, token)
        .await
        .expect("flush_inode_to_backend");

    assert_eq!(
        staged_in_flight(&h),
        0,
        "a fresh partially-written block must not be left staged by fsync"
    );
    assert!(
        block_map(&h, ino).await.contains_key(&6),
        "fsync must publish a device key for the fresh partial block"
    );
}

/// Every `fsync` that has data to make durable issues a DATA-device
/// barrier (DUR-2's primitive on DUR-1's path). RED before the fix: the
/// only barrier `fsync` ever issued was on the metadata volume.
#[tokio::test]
async fn test_fsync_issues_a_data_device_barrier() {
    let h = make([0x13; 16], "dur1_barrier").await;
    let (ino, _) = durable_striped(&h, "barrier.bin", 0x41).await;

    let patch = pattern(HALF as usize, 0x1D);
    write_at(&h, ino, 3 * BS, &patch).await;

    let d0 = METRICS.data_device_syncs.load(Ordering::Relaxed);
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    h.fs.flush_inode_to_backend(ino, token).await.unwrap();
    assert!(
        METRICS.data_device_syncs.load(Ordering::Relaxed) > d0,
        "fsync must barrier the data device before the metadata that names \
         its blocks"
    );
}

/// **The ordering leg.** The data barrier completes STRICTLY BEFORE the
/// metadata barrier that names it: with the data-device barrier faulted,
/// `fsync` must fail and the metadata barrier must never run. Under the
/// retired `tokio::try_join!` both ran concurrently, so a failing data
/// barrier could not prevent the metadata commit from being barriered.
#[tokio::test]
async fn test_data_barrier_precedes_the_metadata_barrier() {
    let h = make([0x14; 16], "dur1_order").await;
    let (ino, _) = durable_striped(&h, "order.bin", 0x66).await;

    let patch = pattern(HALF as usize, 0x77);
    write_at(&h, ino, 4 * BS, &patch).await;

    let dev_path = h.fs.router.backend_router.default_device.device_path.clone();
    squeezefs::dev_power_cut::arm_barrier_error(&dev_path, libc::EIO);
    let m0 = METRICS.meta_device_syncs.load(Ordering::Relaxed);

    let token = h.fs.dlm().get_fencing_token_ino(ino);
    let res = h.fs.flush_inode_to_backend(ino, token).await;
    squeezefs::dev_power_cut::clear_faults();

    assert!(
        res.is_err(),
        "a failed data-device barrier must fail the fsync — never report \
         success for bytes that are still volatile"
    );
    assert_eq!(
        METRICS.meta_device_syncs.load(Ordering::Relaxed),
        m0,
        "the metadata barrier ran despite a failed data barrier — the data \
         barrier does not strictly precede it"
    );
}
