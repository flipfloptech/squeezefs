//! Dismount teardown contracts.
//!
//! FUSE-over-io_uring runs one request loop per queue; connection teardown
//! surfaces a `Destroy` to *every* loop, so `Filesystem::destroy` is invoked
//! ~nproc times per unmount. The teardown work (force-flush of staged active
//! blocks, bitmap reconciliation, client unregister) must run exactly once —
//! the repeated passes re-attempted thousands of orphan flushes, starved the
//! uring worker until the health prober declared backends offline, and spewed
//! ~3k ERROR lines per unmount.
//!
//! Contracts:
//! 1. `destroy` is idempotent: only the first invocation flushes; later ones
//!    are fast no-ops (observable: an entry staged after the first destroy is
//!    untouched by the second).
//! 2. Teardown flushing reports an aggregated summary — bounded error
//!    samples, not a log line per failed block — and failures (orphan blocks
//!    of deleted inodes, offline backends) are counted, not spammed.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

async fn make() -> (SqueezefsFilesystem, Request, NamedTempFile, NamedTempFile) {
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "dismount_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
    )
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    let ms = MetaLvStorage::open(m.path(), 64 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&ms).await.unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        Arc::new(MetaLvBackend::new(ms)),
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 0,
        uid: 0,
        gid: 0,
        pid: 0,
    };
    std::mem::forget(s); // staging dir must outlive the fs in this test
    (fs, req, b, m)
}

/// Contract 1: only the first destroy tears down; later invocations (one per
/// uring queue at unmount) are no-ops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_destroy_is_idempotent_across_queue_invocations() {
    let (fs, req, _b, _m) = make().await;

    // Orphan active block (no inode meta): first destroy attempts + drops it.
    assert!(fs.router.cache.nvme.put_active_block(
        "active_block:inode_991001:block_0",
        &[0xAA; 4096],
        1
    ));

    fs.destroy(req).await;
    assert!(fs.dismount_started(), "first destroy must mark teardown");

    // Stage a sentinel AFTER teardown: a second destroy must NOT flush or
    // remove it (it must not re-run the teardown work at all).
    assert!(fs.router.cache.nvme.put_active_block(
        "active_block:inode_991002:block_0",
        &[0xBB; 4096],
        1
    ));

    fs.destroy(req).await;

    let remaining = fs.router.cache.nvme.list_staged_files();
    assert!(
        remaining
            .iter()
            .any(|k| k == "active_block:inode_991002:block_0"),
        "second destroy re-ran teardown and consumed the sentinel: {remaining:?}"
    );
}

/// Contract 2: teardown flushing returns an aggregated summary with bounded
/// error samples — orphan blocks (deleted inodes) count as failures without
/// a log line each.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_teardown_flush_aggregates_failures() {
    let (fs, _req, _b, _m) = make().await;

    // 300 orphan active blocks: inodes never existed, so every flush fails
    // (NotFound) — exactly the deleted-files-at-unmount shape.
    for i in 0..300u32 {
        assert!(fs.router.cache.nvme.put_active_block(
            &format!("active_block:inode_87{i:04}:block_0"),
            &[0x5A; 4096],
            1
        ));
    }

    let summary = fs.flush_all_staged_blocks_to_backend().await;
    assert_eq!(summary.attempted, 300, "all orphans attempted");
    assert_eq!(summary.flushed, 0, "orphans cannot flush");
    assert_eq!(summary.failed, 300, "all orphans counted as failures");
    assert!(
        summary.error_samples.len() <= 3,
        "error samples must be bounded (got {})",
        summary.error_samples.len()
    );
}
