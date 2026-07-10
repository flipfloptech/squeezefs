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
