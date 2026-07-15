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
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

/// Format + mount one v3 metadata volume for this harness.
async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> std::sync::Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
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
    .expect("format v3 meta volume");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

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
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();

    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 256 * 1024 * 1024).await,
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
/// error samples, and orphan blocks (deleted inodes) are VERIFIED-and-
/// DISCARDED as clean resolutions — never a log line each, never a leaked
/// entry. (FIND-M11-A superseded the old fail-and-leave shape: 300 leaked
/// orphans kept `staged_writes_in_flight` pinned, so destroy's drain-wait
/// spun its full budget on entries nothing could ever remove — the
/// recovery contract "missing inode meta discards orphan active blocks"
/// now applies live at teardown.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_teardown_flush_aggregates_failures() {
    let (fs, _req, _b, _m) = make().await;
    let discards_before = squeezefs::fuse_client::METRICS
        .writeback_orphan_discards
        .load(std::sync::atomic::Ordering::Relaxed);

    // 300 orphan active blocks: inodes never existed, so every flush's
    // merge hits NotFound — exactly the deleted-files-at-unmount shape.
    for i in 0..300u32 {
        assert!(fs.router.cache.nvme.put_active_block(
            &format!("active_block:inode_87{i:04}:block_0"),
            &[0x5A; 4096],
            1
        ));
    }

    let summary = fs.flush_all_staged_blocks_to_backend().await;
    assert_eq!(summary.attempted, 300, "all orphans attempted");
    assert_eq!(
        summary.flushed, 300,
        "orphans resolve as verified discards, not failures: {:?}",
        summary.error_samples
    );
    assert_eq!(summary.failed, 0, "no orphan may surface as a failure");
    assert!(
        summary.error_samples.len() <= 3,
        "error samples must be bounded (got {})",
        summary.error_samples.len()
    );
    let leaked: Vec<String> = fs
        .router
        .cache
        .nvme
        .list_staged_files()
        .into_iter()
        .filter(|k| k.starts_with("active_block:"))
        .collect();
    assert!(
        leaked.is_empty(),
        "orphan entries must DRAIN at teardown (the FIND-M11-A drain-wait \
         wedge): {leaked:?}"
    );
    assert_eq!(
        fs.router
            .cache
            .nvme
            .staged_writes_in_flight
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "staged_writes_in_flight must reach 0 so destroy's drain-wait is \
         bounded"
    );
    let discards_after = squeezefs::fuse_client::METRICS
        .writeback_orphan_discards
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        discards_after >= discards_before + 300,
        "each orphan discard must be counted \
         (before {discards_before}, after {discards_after})"
    );
}

// ===========================================================================
// PR K6b — checkpoint-task lifecycle (design §4.6): the per-volume
// checkpoint/writeback task drains cleanly on unmount and never leaks when
// a backend is dropped without one.
// ===========================================================================

use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::Metadata;

async fn v3_volume() -> (std::sync::Arc<KvMetaBackend>, NamedTempFile) {
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(64 * 1024 * 1024).unwrap();
    format_v3(
        f.path(),
        64 * 1024 * 1024,
        &FormatV3Options {
            node_size: 64 * 1024,
            journal_len_override: Some(1024 * 1024),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .unwrap();
    let be = KvMetaBackend::open(f.path()).await.unwrap();
    (be, f)
}

/// Contract 3 (K6b): `shutdown` runs a final checkpoint and JOINS the
/// checkpoint task — after it returns, the task is gone (the liveness
/// probe fails to upgrade) and a remount replays an EMPTY window. A
/// second `shutdown` is a no-op, and mutations after shutdown are
/// refused rather than silently un-checkpointed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_v3_shutdown_drains_checkpoint_task() {
    let (be, f) = v3_volume().await;
    for i in 0..10 {
        Metadata::create(
            be.as_ref(),
            1,
            &format!("t{i}"),
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await
        .unwrap();
    }
    let probe = be.checkpoint_alive_probe();
    assert!(
        probe.upgrade().is_some(),
        "the checkpoint task must be alive while the backend serves"
    );

    be.shutdown().await.expect("clean shutdown");
    assert!(
        probe.upgrade().is_none(),
        "shutdown must JOIN the checkpoint task — an alive probe means a leaked task"
    );
    be.shutdown().await.expect("shutdown is idempotent");
    assert!(
        Metadata::create(be.as_ref(), 1, "late", libc::S_IFREG | 0o644, 0, 0)
            .await
            .is_err(),
        "mutations after shutdown must be refused (they could never be checkpointed)"
    );
    drop(be);

    let re = KvMetaBackend::open(f.path()).await.unwrap();
    assert_eq!(
        re.replay_stats().entries,
        0,
        "the final checkpoint must drain the whole window (tail == head)"
    );
    for i in 0..10 {
        assert!(re.lookup(1, &format!("t{i}")).await.is_ok());
    }
    re.shutdown().await.unwrap();
}

/// Contract 4 (K6b): dropping a backend WITHOUT shutdown must not leak
/// the checkpoint task — it observes the dead backend on its next tick
/// and exits (the v2 flusher's Arc-sentinel discipline).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_v3_dropped_backend_reaps_checkpoint_task() {
    let (be, _f) = v3_volume().await;
    Metadata::create(be.as_ref(), 1, "orphan", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let probe = be.checkpoint_alive_probe();
    drop(be);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while probe.upgrade().is_some() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        probe.upgrade().is_none(),
        "the checkpoint task must exit once its backend is dropped (no leaked tasks)"
    );
}
