//! Hang-1 regression pins — the FUSE_COPY_FILE_RANGE wedge on long-churned
//! persistent mounts (gdb forensics: `.benchmarks/2026-07-10-cfr-wedge-*`).
//!
//! Root-cause shape: the staging shard is a parking_lot `RwLock`, which is
//! WRITER-PREFERRING — once a writer is queued, plain `read()` PARKS. The
//! §5.5 zero-copy flush holds a shard READ guard across the DMA await, so a
//! parked-behind-writer read on an async executor thread closes a cycle:
//!
//!   C (executor thread, sync `read()` probe) parks behind
//!   W (queued shard writer, `remove_active_block` on the blocking pool),
//!   which waits for
//!   A (§5.5 read guard held across an await), whose wake needs
//!   C's parked executor thread.               → total daemon wedge.
//!
//! The fixed invariant, pinned here: **shard reads acquired on async
//! executor threads never park behind a QUEUED writer** (recursive-read
//! acquisition). Executor threads may wait only for an ACTIVE writer's
//! bounded critical section, which runs on a blocking thread with no
//! dependency on any executor — cycle impossible by construction.

use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Deterministic reconstruction of the wedge cycle on one current-thread
/// LocalSet executor (the fuse3 TPC handler-thread shape). On the broken
/// code the scenario thread parks forever inside the probe `get_static`
/// and the 20 s deadline fails the test; on fixed code it completes in
/// milliseconds regardless of interleaving.
#[test]
fn test_shard_read_does_not_park_executor_behind_queued_writer() {
    let dir = tempfile::tempdir().expect("tempdir");
    // One shard: every key collides — guard, writer, and probe share the lock.
    let cache = Arc::new(
        squeezefs::tiering::nvme::NvmeCache::new(&[dir.path()], &[1 << 20], 1).expect("nvme cache"),
    );

    let meta = [0u8; 8];
    assert!(cache.reserve_and_write(
        Bytes::from_static(b"guard-key"),
        meta.len() as u64,
        &meta,
        b"guard-bytes",
        None
    ));
    assert!(cache.reserve_and_write(
        Bytes::from_static(b"probe-key"),
        meta.len() as u64,
        &meta,
        b"probe-bytes",
        None
    ));

    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let cache_scenario = cache.clone();

    std::thread::spawn(move || {
        // One fuse3 TPC handler thread: current-thread runtime + LocalSet.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            // A — the §5.5 holder: takes the shard read guard, then suspends
            // on an await (stand-in for the DMA CQE). Its wake can only be
            // polled by THIS thread.
            let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
            let cache_a = cache_scenario.clone();
            let holder = tokio::task::spawn_local(async move {
                let g = cache_a
                    .get_static(&Bytes::from_static(b"guard-key"))
                    .expect("guard entry resident");
                let _ = gate_rx.await;
                drop(g);
            });
            // Let A take its guard before the writer queues.
            tokio::task::yield_now().await;

            // W — the shard writer on its own OS thread (the spawn_blocking
            // shape used by remove_active_block). It must wait for A's guard.
            let w_started = Arc::new(AtomicBool::new(false));
            let ws = w_started.clone();
            let cache_w = cache_scenario.clone();
            let writer = std::thread::spawn(move || {
                ws.store(true, Ordering::SeqCst);
                cache_w.remove(&Bytes::from_static(b"probe-key"));
            });
            while !w_started.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            // Red-side determinism only: give W time to enqueue as a waiting
            // writer. On fixed code the probe below succeeds regardless of
            // whether W is queued yet, so this cannot flake green runs.
            std::thread::sleep(Duration::from_millis(200));

            // C — the probe: a sync shard read on the executor thread (the
            // read_staged_zero_copy existence probe in flush_one_active_block).
            // BROKEN: parks this thread behind W; nobody can ever fire A's
            // gate; the daemon shape of this is the fsx CFR wedge.
            let probe = cache_scenario.get_static(&Bytes::from_static(b"probe-key"));
            drop(probe);

            let _ = gate_tx.send(());
            let _ = holder.await;
            let _ = writer.join();
        });
        let _ = done_tx.send(());
    });

    done_rx.recv_timeout(Duration::from_secs(20)).expect(
        "staging-shard deadlock: executor thread parked in a shard read behind a queued \
         writer while the §5.5 guard holder needed this executor (Hang-1 CFR wedge shape)",
    );
}

/// VL8 item 2, capture 2 — the generic/464 writes-only wedge (the LEDGER
/// face of Hang-1). The staged-budget ledger (`NvmeStaging::staged_ledger`,
/// an scc map) is a SECOND lock population with the shard-lock hazard: a
/// re-stage's blocking-pool closure holds a file_id's ledger ENTRY lock
/// across `reserve_and_write`, which legitimately waits on the staging
/// shard WRITE lock — and that wait is unbounded while a §5.5 read guard
/// is parked on an await. Any `*_sync` scc op on the ledger from an async
/// executor thread then BLOCKS THE THREAD on the bucket, and when the §5.5
/// guard holder lives on that same (fuse3 TPC current-thread) executor,
/// the cycle closes:
///
///   H (executor thread, `stage_write`'s prior-cost `read_sync`) blocks on
///   B (the ledger bucket, held by a re-stage closure on the blocking
///     pool), which waits for
///   S (the staging shard WRITE lock), which waits for
///   A (a §5.5 read guard held across an await), whose wake needs
///   H's blocked executor thread.            → writes wedge forever
///     (capture: /tmp/vl8_fstests/wedge464, T39/T29/T26 at nvme.rs:1143,
///      T2 holding the bucket inside reserve_and_write@685).
///
/// The fixed invariant, pinned here: **async executor threads never issue
/// blocking `*_sync` scc ops against the staged ledger** — its entry locks
/// are held across shard-write waits by design, so executor-side ledger
/// access must park the TASK (scc `*_async`), never the thread.
#[test]
fn test_ledger_read_never_blocks_executor_while_bucket_holder_waits_shard_write() {
    let staging_dir = tempfile::tempdir().expect("staging dir");
    let backing = tempfile::NamedTempFile::new().expect("backing file");
    std::fs::File::create(backing.path())
        .expect("create backing")
        .set_len(64 * 1024 * 1024)
        .expect("set_len");

    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let staging_path = staging_dir.path().to_path_buf();
    let backing_path = backing.path().to_path_buf();

    std::thread::spawn(move || {
        // One fuse3 TPC handler thread: current-thread runtime + LocalSet —
        // the §5.5 guard holder and the blocked writer share this thread,
        // exactly as handlers share a TPC LocalSet in the daemon.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let dlm = squeezefs::dlm::DlmClient::new("local").expect("dlm");
            let ba = Arc::new(
                squeezefs::block_allocator::BlockAllocator::new(
                    dlm.meta_client().clone(),
                    "ledger_wedge_test",
                )
                .await
                .expect("allocator"),
            );
            let nvme_dev = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
                backing_path.to_str().expect("utf8 path"),
            ));
            let staging = squeezefs::cache::nvme::NvmeStaging::new(
                vec![staging_path],
                8 * 1024 * 1024,
                8 * 1024 * 1024,
                ba,
                nvme_dev,
                dlm.meta_client().clone(),
                None,
            )
            .await
            .expect("staging");
            let staging = Arc::new(staging);

            // Seed the entry the whole scenario revolves around.
            staging
                .stage_write("inode_9001", "fid-wedge", Bytes::from(vec![1u8; 4096]), 1)
                .await
                .expect("seed stage");

            // A — the §5.5 holder: shard READ guard held across an await
            // (stand-in for the flush unit's DMA CQE). Its wake can only be
            // polled by THIS thread.
            let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
            let staging_a = staging.clone();
            let holder = tokio::task::spawn_local(async move {
                let g = staging_a
                    .staged_dma_source("fid-wedge")
                    .expect("guard entry resident");
                let _ = gate_rx.await;
                drop(g);
            });
            tokio::task::yield_now().await;

            // W — a re-stage of the SAME file_id on its own OS thread (its
            // internal spawn_blocking closure takes the ledger ENTRY lock,
            // then waits for the shard WRITE lock inside reserve_and_write —
            // blocked by A's guard for as long as A cannot be polled).
            let w_started = Arc::new(AtomicBool::new(false));
            let ws = w_started.clone();
            let staging_w = staging.clone();
            let writer = std::thread::spawn(move || {
                let rt2 = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("rt2");
                ws.store(true, Ordering::SeqCst);
                rt2.block_on(staging_w.stage_write(
                    "inode_9001",
                    "fid-wedge",
                    Bytes::from(vec![2u8; 4096]),
                    1,
                ))
            });
            while !w_started.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            // Red-side determinism only: give W time to take the ledger
            // entry lock and park on the shard write lock. On fixed code
            // the stage below succeeds regardless of W's progress, so this
            // cannot flake green runs.
            std::thread::sleep(Duration::from_millis(300));

            // The gate fires from an independent thread (the daemon shape:
            // A's wake is a DMA completion, executor-independent). Pre-fix
            // the executor thread is about to block, so A is never polled
            // even though its wake arrived.
            let gate_thread = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(750));
                let _ = gate_tx.send(());
            });

            // H — a third stage of the same file_id from THIS executor
            // thread. BROKEN: the prior-cost ledger read (`read_sync`)
            // blocks the thread on the bucket W holds; nobody can ever
            // poll A; the daemon shape of this is the generic/464
            // writes-only wedge.
            staging
                .stage_write("inode_9001", "fid-wedge", Bytes::from(vec![3u8; 4096]), 1)
                .await
                .expect("executor-side re-stage must complete");

            let _ = holder.await;
            writer
                .join()
                .expect("writer thread")
                .expect("blocking-pool re-stage must complete");
            let _ = gate_thread.join();
        });
        let _ = done_tx.send(());
    });

    done_rx.recv_timeout(Duration::from_secs(30)).expect(
        "staged-ledger deadlock: executor thread blocked in a ledger read_sync while the \
         bucket holder waited on the shard write lock behind a parked §5.5 guard \
         (VL8 item 2 capture 2 — the generic/464 writes-only wedge shape)",
    );
}

/// The writer path itself must stay correct under the recursive-read rule:
/// a writer queued behind an outstanding read guard still completes once
/// the guard drops, and its removal is observed by subsequent reads.
#[test]
fn test_queued_writer_completes_once_guard_drops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = Arc::new(
        squeezefs::tiering::nvme::NvmeCache::new(&[dir.path()], &[1 << 20], 1).expect("nvme cache"),
    );
    let meta = [0u8; 8];
    assert!(cache.reserve_and_write(
        Bytes::from_static(b"victim"),
        meta.len() as u64,
        &meta,
        b"victim-bytes",
        None
    ));

    let guard = cache
        .get_static(&Bytes::from_static(b"victim"))
        .expect("resident");

    let w_started = Arc::new(AtomicBool::new(false));
    let ws = w_started.clone();
    let cache_w = cache.clone();
    let writer = std::thread::spawn(move || {
        ws.store(true, Ordering::SeqCst);
        cache_w.remove(&Bytes::from_static(b"victim"))
    });
    while !w_started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }

    // Reads (recursive) still serve the entry while the writer waits.
    assert!(
        cache.get_static(&Bytes::from_static(b"victim")).is_some(),
        "entry must stay readable while the writer is queued behind a live guard"
    );

    drop(guard);
    let removed = writer.join().expect("writer thread");
    assert!(
        removed.is_some(),
        "queued writer must complete after guard drop"
    );
    assert!(
        cache.get_static(&Bytes::from_static(b"victim")).is_none(),
        "removal must be visible once the writer completes"
    );
}
