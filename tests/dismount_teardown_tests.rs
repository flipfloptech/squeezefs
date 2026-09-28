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
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("dismount_test").await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
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
        ..Default::default()
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

/// VL8 item 4 — external unmount cancels the session task's `destroy` future
/// mid-teardown (reply-task select / detached queue workers / daemon exit),
/// so `client:{id}` and `writer_claim` heartbeat records lingered to the
/// 45 s staleness TTL. The teardown must survive that cancellation: records
/// must deregister promptly WITHOUT the TTL wait.
///
/// The repro models the cancellation exactly: poll `destroy` once (past the
/// `dismount_once` claim, into the teardown's first await) and DROP it.
/// The deregistration must still complete. Observation is a bounded poll of
/// the backend records (the effect is external to the dropped future — there
/// is nothing in-band to synchronize on by design).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cancelled_destroy_still_deregisters_heartbeat_records() {
    use squeezefs::meta_backend::kv::backend::WRITER_CLAIM_XATTR;
    use squeezefs::meta_backend::Metadata;

    let (fs, req, _b, _m) = make().await;
    let backend = fs.meta_backend.clone().expect("fixture backend");

    // Register this "mount": client heartbeat record + the writer_claim the
    // volume open committed.
    *fs.client_id.lock().unwrap() = "vl8-item4-client".to_string();
    fs.refresh_client_registration().await;
    let client_attr = "client:vl8-item4-client".to_string();
    assert!(
        backend
            .getxattr(1, &client_attr)
            .await
            .expect("getxattr client record")
            .is_some(),
        "fixture must have a live client registration"
    );
    assert!(
        backend
            .getxattr(1, WRITER_CLAIM_XATTR)
            .await
            .expect("getxattr writer_claim")
            .is_some(),
        "volume open must have committed a writer_claim"
    );

    // External unmount: the destroy future is polled into its first await
    // and then dropped (the session task dies).
    let mut destroy_fut = Box::pin(fs.destroy(req));
    let first_poll = futures::poll!(destroy_fut.as_mut());
    assert!(
        first_poll.is_pending(),
        "destroy must reach an await point — the cancellation window is the point of this repro"
    );
    drop(destroy_fut);
    assert!(
        fs.dismount_started(),
        "the dropped destroy must have claimed the teardown"
    );

    // The records must deregister without the 45 s TTL: bounded observation
    // window well under the TTL.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let client_gone = backend
            .getxattr(1, &client_attr)
            .await
            .ok()
            .flatten()
            .is_none();
        let claim_gone = backend
            .getxattr(1, WRITER_CLAIM_XATTR)
            .await
            .ok()
            .flatten()
            .is_none();
        if client_gone && claim_gone {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "heartbeat records lingered past the observation window after a \
             cancelled destroy (client_gone={client_gone}, claim_gone={claim_gone}) — \
             they would sit until the 45 s staleness TTL"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

// ===========================================================================
// Record §4.4bx (review round 1, Issue 1): the teardown's terminal step
// CLOSES this mount's data plane — the staging root is the next mount's
// from the instant the teardown completes, while this process's writeback
// ladder, merge worker and reclaimer run until exit.
// ===========================================================================

/// After `destroy` the device gate refuses every DMA in its own class
/// (`EROFS`, `data_dma_dismount_refusals` — never the fence: a clean
/// unmount poisons nothing), the reclaim queue issues no device command
/// (`block_free_reclaim_dismount_halts`), and a staged unit the closed
/// plane refuses STAYS staged for the successor. RED on the round-1
/// build: the post-teardown DMA landed and the reclaim punched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dismount_teardown_closes_the_data_plane_to_the_workers_that_outlive_it() {
    use squeezefs::fuse_client::METRICS;
    use std::sync::atomic::Ordering;
    let (fs, req, _b, _m) = make().await;
    let dev = fs.router.backend_router.default_device.clone();
    let ba = fs.router.backend_router.default_allocator.clone();
    let block = bytes::Bytes::from(vec![0x42u8; 4096]);
    assert!(!fs.router.backend_router.data_plane_dismounted());
    dev.write_block(0, block.clone())
        .await
        .expect("a DMA lands on a serving mount");
    // A published block to free AFTER the teardown (the post-teardown
    // abandon/free shape).
    let victim = ba.allocate_block().await.expect("allocate");
    ba.publish_block(victim);

    let refusals0 = METRICS.data_dma_dismount_refusals.load(Ordering::Relaxed);
    let halts0 = METRICS
        .block_free_reclaim_dismount_halts
        .load(Ordering::Relaxed);
    let punches0 = METRICS.block_free_file_punches.load(Ordering::Relaxed);

    fs.destroy(req).await;
    assert!(fs.dismount_started() && fs.dismount_complete());
    assert!(
        fs.router.backend_router.data_plane_dismounted(),
        "the teardown's terminal step closes the data plane"
    );
    assert!(
        !squeezefs::data_custody::poisoned(),
        "a clean unmount is not a fence"
    );

    // The device gate: its own class, counted.
    let err = dev
        .write_block(0, block)
        .await
        .expect_err("a post-teardown DMA is refused");
    match &err {
        squeezefs::error::SqueezefsError::Refused { errno, msg } => {
            assert_eq!(*errno, libc::EROFS, "{msg}");
            assert!(msg.contains("dismount teardown completed"), "{msg}");
        }
        other => panic!("the dismount class is a typed refusal, got {other:?}"),
    }
    // A process-global counter beside sibling tests that also destroy and
    // DMA: exact under `--test-threads=1`, at least ours in parallel.
    assert!(METRICS.data_dma_dismount_refusals.load(Ordering::Relaxed) > refusals0);

    // The reclaim queue: a free enqueued past the teardown issues no
    // device command.
    fs.router
        .backend_router
        .free_block(&victim.to_string())
        .await
        .expect("the terminal free itself is admitted");
    fs.router.backend_router.reclaim_drain().await;
    assert!(
        METRICS
            .block_free_reclaim_dismount_halts
            .load(Ordering::Relaxed)
            > halts0,
        "the reclaim entry is halted in the dismount class"
    );
    assert_eq!(
        METRICS.block_free_file_punches.load(Ordering::Relaxed),
        punches0,
        "no discard reaches the device past the teardown"
    );

    // A unit the closed plane refuses stays STAGED for the successor.
    let key = "active_block:inode_991003:block_0";
    assert!(fs.router.cache.nvme.put_active_block(key, &[0xCC; 4096], 1));
    let summary = fs.flush_all_staged_blocks_to_backend().await;
    assert!(
        summary.failed >= 1,
        "the sweep's upload meets the closed gate: {summary:?}"
    );
    assert!(
        fs.router
            .cache
            .nvme
            .list_staged_files()
            .iter()
            .any(|k| k == key),
        "the refused unit's bytes stay staged"
    );
}

// ===========================================================================
// Record §7 item 24 — the retire wait on custody the exhausted set cannot land
// ===========================================================================

/// The dismount's writeback-retire wait is SKIPPED when every unit the
/// sweep could not land failed for SPACE: the allocators are exhausted,
/// the writeback ladder cannot land those units either, and the wait
/// would run its whole `dismount_wait` for nothing (the generic/751
/// daemon spun all 10 s of it on 3 blocks). The units stay staged for the
/// next mount at this mount point, the summary names the class, and the
/// teardown completes well inside the wait. RED on the pre-fix tree: the
/// teardown took the full `dismount_wait`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dismount_retire_wait_skips_custody_the_exhausted_set_cannot_land() {
    let (mut fs, req, _b, _m) = make().await;
    fs.dismount_wait = 10;
    let ba = fs.router.backend_router.default_allocator.clone();
    // A one-block store, minted to capacity (capacity 0 is the UNBOUNDED
    // allocator): every fresh mint from here refuses StorageFull at once,
    // BEFORE the sweep's merge could classify the unit an orphan.
    ba.set_capacity_bytes(ba.chunk_size());
    let only = ba.allocate_block().await.expect("the one block");
    ba.publish_block(only);
    let e = ba.allocate_block().await.expect_err("full exactly");
    assert!(
        matches!(&e, squeezefs::error::SqueezefsError::Io(io)
            if io.kind() == std::io::ErrorKind::StorageFull),
        "the fixture is a full store: {e:?}"
    );
    for i in 0..3u32 {
        assert!(fs.router.cache.nvme.put_active_block(
            &format!("active_block:inode_99100{i}:block_0"),
            &[0xD1; 4096],
            1
        ));
    }
    assert_eq!(fs.router.cache.nvme.active_block_custody_count(), 3);

    let t0 = std::time::Instant::now();
    fs.destroy(req).await;
    let wall = t0.elapsed();
    assert!(fs.dismount_started() && fs.dismount_complete());
    assert!(
        wall < std::time::Duration::from_secs(5),
        "the retire wait is skipped for custody the exhausted set cannot land \
         (teardown took {wall:?} against a 10-s dismount_wait)"
    );
    let staged: Vec<String> = fs
        .router
        .cache
        .nvme
        .list_staged_files()
        .into_iter()
        .filter(|k| k.starts_with("active_block:inode_99100"))
        .collect();
    assert_eq!(
        staged.len(),
        3,
        "every unit the set could not land stays staged for the next mount: {staged:?}"
    );
}

/// The dismount sweep FLUSHES a writer-SCOPED staged active block (record
/// §4.4bz): every default-formatted mount since 1.2.0 mints its staging
/// keys with the `:w_<node>.m<slot>` scope suffix, and the sweep's own key
/// parser split on `:block_` and parsed the remainder as the block index —
/// `"16:w_…"` is no `u32`, so the parser returned `Ok(())`, the unit was
/// counted FLUSHED and never attempted, and every scoped mount's unmount
/// left its acked active blocks in local staging (the census said so;
/// the generic/751 daemon's "3 staged blocks" were this). RED on the
/// pre-fix tree: the entry stays staged with `flushed == 1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dismount_sweep_flushes_a_writer_scoped_staged_block() {
    // The writer scope is PROCESS-GLOBAL. Its engaged window is harmless
    // to this suite's siblings: each mints and reads its keys under
    // whatever scope is in force at that instant (a scoped key is `Mine`
    // to the sweep, an unscoped literal is `Legacy` — both flushed), and
    // none asserts an unscoped key literal against a minted one. The gate
    // runs the suite serial in any case.
    struct Disengage;
    impl Drop for Disengage {
        fn drop(&mut self) {
            squeezefs::writer_scope::engage(None);
        }
    }
    let _d = Disengage;
    squeezefs::writer_scope::engage(Some(squeezefs::writer_scope::WriterScope {
        node: 0xae12_71e5_64b9_4323,
        slot: 0x0389_7971,
    }));
    let (fs, req, _b, _m) = make().await;
    // A real striped file whose block 0 is then RE-STAGED under the
    // scoped key, exactly as the write-through fallback stages it.
    let ino = fs
        .create(
            req,
            1,
            std::ffi::OsStr::new("scoped.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("create")
        .attr
        .ino;
    let block = fs
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let want = vec![0xE7u8; block];
    let written = fs
        .write(req, ino, 0, 0, bytes::Bytes::from(want.clone()), 0, 0)
        .await
        .expect("write one block")
        .written;
    assert_eq!(written as usize, block);
    fs.fsync(req, ino, 0, false).await.expect("durable");
    let key: String = squeezefs::keys::active_block(ino, 0).to_string();
    assert!(
        key.contains(":w_"),
        "the mint carries the scope suffix: {key}"
    );
    let newer = vec![0xE8u8; block];
    let token = fs.router.dlm.get_fencing_token_ino(ino);
    assert!(fs.router.cache.nvme.put_active_block(&key, &newer, token));

    let summary = fs.flush_all_staged_blocks_to_backend().await;
    assert_eq!(summary.attempted, 1, "{summary:?}");
    assert_eq!(summary.failed, 0, "{summary:?}");
    assert_eq!(summary.flushed, 1, "{summary:?}");
    assert!(
        !fs.router.cache.nvme.has_staged_active_block(&key),
        "the scoped entry was FLUSHED, not counted and skipped"
    );
    let got = fs
        .read(req, ino, 0, 0, block as u32, 0)
        .await
        .expect("read")
        .data;
    assert_eq!(&got[..], &newer[..], "the re-staged bytes landed durably");
}
