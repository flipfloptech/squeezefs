//! MEM-2 / RES-9 (pre-rc engineering spec §2 / §7): parallel assembly
//! tasks must be OWNED, not detached.
//!
//! The defect these tests pin: the multi-block read assembly laundered
//! its destination pointer to a `usize` (bypassing the `unsafe impl
//! Send` review point `RangedDest`/`SendPtr`/`SendMutPtr` go through),
//! spawned one detached task per block, and joined with
//! `futures::future::try_join_all` over `JoinHandle`s — which
//! short-circuits on the first `JoinError` (panic/abort) and DROPS the
//! remaining handles, detaching the tasks (tokio does not abort on
//! handle drop). Outer-future cancellation did the same. Either way the
//! pooled destination returned to `BUFFER_POOL` (or the uring payload
//! was re-armed) while a detached sibling still memcpy'd into it.
//!
//! Required behavior (spec): hold the tasks in an owned set and join
//! ALL of them before releasing the destination, collecting errors
//! afterward — and give each task an owning handle so a late write hits
//! memory that is still owned. `squeezefs::assembly_tasks` is that
//! machinery; `write_striped` rides the same set with a cancel-path
//! salvage hook (RES-9: no minted block leaks when the writer future is
//! dropped).
//!
//! Determinism: oneshot gates only — no sleeps, no timing assumptions.

use std::sync::Arc;

use squeezefs::assembly_tasks::{AssemblyDest, OwnedTaskSet};
use squeezefs::cache::pool::BufferPool;

/// The RETIRED shape's defect, pinned as permanent documentation of WHY
/// assembly paths must own their tasks: `try_join_all` over
/// `JoinHandle`s returns on the first panic with siblings still
/// pending, dropping (= detaching, NOT aborting) their handles — so the
/// coordinator's error path releases the destination while a live
/// sibling later reaches its copy site. This is exactly the shape
/// `routing.rs`'s multi-block assembly used pre-fix (the sibling here
/// reports the pool state it *would have memcpy'd into* instead of
/// performing the UB write).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn try_join_all_over_join_handles_is_a_detach_footgun() {
    let pool = Arc::new(BufferPool::new(1, 4096));
    let mut buf = pool.alloc();
    buf.resize(4096, 0);
    assert_eq!(pool.len(), 0, "backing handed out");

    let (panic_go_tx, panic_go_rx) = tokio::sync::oneshot::channel::<()>();
    let (sibling_go_tx, sibling_go_rx) = tokio::sync::oneshot::channel::<()>();
    let (sibling_saw_tx, sibling_saw_rx) = tokio::sync::oneshot::channel::<usize>();

    let mut handles = Vec::new();
    handles.push(tokio::spawn(async move {
        let _ = panic_go_rx.await;
        panic!("injected sibling panic");
    }));
    let pool_probe = Arc::clone(&pool);
    handles.push(tokio::spawn(async move {
        let _ = sibling_go_rx.await;
        // The moment a detached late writer would memcpy into the (by
        // now recycled) destination: report what it would hit.
        let _ = sibling_saw_tx.send(pool_probe.len());
    }));

    panic_go_tx.send(()).expect("panicking task parked");
    let joined = futures::future::try_join_all(handles).await;
    assert!(
        joined.is_err(),
        "the panic JoinError must short-circuit try_join_all"
    );
    // Coordinator error path releases the destination: recycled.
    drop(buf);
    assert_eq!(pool.len(), 1, "destination recycled on the error path");
    // The DETACHED sibling is still alive and reaches its copy site
    // AFTER the recycle — the MEM-2 corruption window.
    sibling_go_tx.send(()).expect("detached sibling still parked");
    let seen = sibling_saw_rx
        .await
        .expect("detached sibling still runs after the coordinator returned");
    assert_eq!(
        seen, 1,
        "the late writer observes an already-recycled destination — why assembly \
         paths must own their tasks (AssemblyDest / OwnedTaskSet)"
    );
}

/// Required behavior leg 1 (spec acceptance): panic ONE block task and
/// assert the destination buffer is not recycled until every sibling
/// completed — and that the panic is reported only AFTER the last
/// sibling joined (its output surfaces; it was joined, not detached).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panic_sibling_destination_not_recycled_until_all_joined() {
    let pool = Arc::new(BufferPool::new(1, 4096));
    assert_eq!(pool.len(), 1);
    let mut buf = pool.alloc();
    buf.resize(4096, 0);
    assert_eq!(pool.len(), 0, "backing handed out");
    let dest = Arc::new(AssemblyDest::pooled(buf));

    let mut tasks: OwnedTaskSet<()> = OwnedTaskSet::new("test assembly");
    let (panic_go_tx, panic_go_rx) = tokio::sync::oneshot::channel::<()>();
    let (sibling_go_tx, sibling_go_rx) = tokio::sync::oneshot::channel::<()>();

    // Task A: parks, then panics — the trigger the old shape
    // short-circuited on.
    {
        let dest = Arc::clone(&dest);
        tasks.spawn(async move {
            let _own = dest;
            let _ = panic_go_rx.await;
            panic!("injected block-task panic");
        });
    }
    // Task B (the sibling): parks PAST A's panic, then writes into its
    // region and completes.
    let pool_probe = Arc::clone(&pool);
    {
        let dest = Arc::clone(&dest);
        tasks.spawn(async move {
            let _ = sibling_go_rx.await;
            // The destination must still be un-recycled: this task's own
            // Arc pins the pooled backing regardless of A's panic or of
            // anything the coordinator did meanwhile.
            assert_eq!(
                pool_probe.len(),
                0,
                "destination backing recycled while a sibling still writes"
            );
            // SAFETY: [0, 64) is this task's exclusive region (the test
            // partitions the destination), and the backing is alive —
            // this task's Arc owns it.
            unsafe { dest.write_at(0, &[0xAB; 64]) };
            Ok(())
        });
    }

    // Drop the coordinator-side Arc BEFORE joining: only the tasks may
    // keep the destination alive from here.
    drop(dest);

    panic_go_tx.send(()).expect("task A parked");
    sibling_go_tx.send(()).expect("task B parked");

    let (completed, err) = tasks.join_all().await;
    assert_eq!(
        completed.len(),
        1,
        "the sibling must be JOINED (its output surfaced), not detached"
    );
    let err = err.expect("the panic must surface as the batch error");
    let msg = format!("{err:?}");
    assert!(msg.contains("panic"), "unexpected batch error: {msg}");
    // Every owner has dropped by join_all's return: home exactly once.
    assert_eq!(
        pool.len(),
        1,
        "backing must recycle exactly once, after ALL owners dropped"
    );
}

/// Sends `released` strictly AFTER this task's `Arc<AssemblyDest>` is
/// dropped, so receiving it happens-after the ownership ended (no
/// field-drop-order race in the assertion).
struct OwnershipProbe {
    dest: Option<Arc<AssemblyDest>>,
    released_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for OwnershipProbe {
    fn drop(&mut self) {
        self.dest.take();
        if let Some(tx) = self.released_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Required behavior leg 2 (spec acceptance): cancel the OUTER future
/// mid-assembly and assert the destination is not recycled until every
/// sibling released ownership — cancellation aborts the owned set (no
/// detached writers), and the pooled backing recycles exactly once,
/// only after the last task's Arc dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outer_cancellation_destination_not_recycled_until_all_owners_drop() {
    let pool = Arc::new(BufferPool::new(1, 4096));
    let mut buf = pool.alloc();
    buf.resize(4096, 0);
    let dest = Arc::new(AssemblyDest::pooled(buf));

    let mut tasks: OwnedTaskSet<()> = OwnedTaskSet::new("test assembly");
    let mut entered_rxs = Vec::new();
    let mut released_rxs = Vec::new();
    let mut park_txs = Vec::new();
    for i in 0..2usize {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
        let (released_tx, released_rx) = tokio::sync::oneshot::channel::<()>();
        let (park_tx, park_rx) = tokio::sync::oneshot::channel::<()>();
        entered_rxs.push(entered_rx);
        released_rxs.push(released_rx);
        park_txs.push(park_tx);
        let probe = OwnershipProbe {
            dest: Some(Arc::clone(&dest)),
            released_tx: Some(released_tx),
        };
        tasks.spawn(async move {
            // SAFETY: task i owns the exclusive region [i*8, i*8+8) (the
            // test partitions the destination); the backing is alive —
            // this task's probe holds the Arc.
            unsafe { probe.dest.as_ref().unwrap().write_at(i * 8, &[1u8; 8]) };
            entered_tx.send(()).ok();
            // Cancellation point: parked here when the outer future is
            // dropped (the senders live in the test, never fired).
            let _ = park_rx.await;
            drop(probe);
            Ok(())
        });
    }

    // The coordinator as its own task, so the test can cancel it
    // mid-assembly (= drop the outer future).
    let coordinator = tokio::spawn(async move {
        let mut tasks = tasks;
        let (_completed, err) = tasks.join_all().await;
        err
    });

    for rx in entered_rxs {
        rx.await.expect("sibling entered");
    }
    // Release the test-side Arc: only the parked siblings own the
    // destination now.
    drop(dest);
    assert_eq!(
        pool.len(),
        0,
        "backing owned by cancellation-stranded siblings must not be recycled"
    );

    // Cancel the outer future mid-assembly.
    coordinator.abort();
    let joined = coordinator.await;
    assert!(
        joined.as_ref().err().is_some_and(|e| e.is_cancelled()),
        "coordinator must be cancelled, got {joined:?}"
    );

    // The destination recycles only after BOTH siblings released it.
    for rx in released_rxs {
        rx.await.expect("sibling released ownership on abort");
    }
    assert_eq!(
        pool.len(),
        1,
        "backing must recycle exactly once, after the last owner dropped"
    );
    drop(park_txs);
}

/// RES-9 face: a cancelled owner must SALVAGE every output that
/// surfaced (write_striped: free every minted-and-published block) —
/// the salvage hook receives the surfaced outputs, and parked siblings
/// are aborted before producing any.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_owner_salvages_surfaced_outputs() {
    let (salvaged_tx, salvaged_rx) = tokio::sync::oneshot::channel::<Vec<String>>();
    let mut tasks: OwnedTaskSet<String> = OwnedTaskSet::with_salvage(
        "test salvage",
        Box::new(
            move |outs: Vec<String>| -> futures::future::BoxFuture<'static, ()> {
                Box::pin(async move {
                    let mut keys = outs;
                    keys.sort();
                    let _ = salvaged_tx.send(keys);
                })
            },
        ),
    );

    let (t1_done_tx, t1_done_rx) = tokio::sync::oneshot::channel::<()>();
    tasks.spawn(async move {
        t1_done_tx.send(()).ok();
        Ok("k1".to_string())
    });
    let (park_tx, park_rx) = tokio::sync::oneshot::channel::<()>();
    tasks.spawn(async move {
        let _ = park_rx.await;
        Ok("k2".to_string())
    });

    t1_done_rx.await.expect("t1 ran");
    // Cancel the owner mid-flight (the outer-future-drop shape).
    drop(tasks);

    let keys = salvaged_rx
        .await
        .expect("the salvage hook must run on cancellation");
    assert_eq!(
        keys,
        vec!["k1".to_string()],
        "surfaced outputs are salvaged; the parked sibling was aborted before producing"
    );
    drop(park_tx);
}

/// The salvage hook must NOT run when the owner joined normally — the
/// happy/error paths keep their explicit custody (no double-free).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joined_owner_never_salvages() {
    let (salvaged_tx, salvaged_rx) = tokio::sync::oneshot::channel::<Vec<String>>();
    let mut tasks: OwnedTaskSet<String> = OwnedTaskSet::with_salvage(
        "test salvage",
        Box::new(
            move |outs: Vec<String>| -> futures::future::BoxFuture<'static, ()> {
                Box::pin(async move {
                    let _ = salvaged_tx.send(outs);
                })
            },
        ),
    );
    tasks.spawn(async move { Ok("k".to_string()) });
    let (completed, err) = tasks.join_all().await;
    assert_eq!(completed, vec!["k".to_string()]);
    assert!(err.is_none());
    drop(tasks);
    assert!(
        salvaged_rx.await.is_err(),
        "salvage must be dropped unused after a normal join"
    );
}

/// Payload arm: writes land in the caller's region, `into_bytes` hands
/// out a view over the same memory, and dropping the view releases
/// nothing (the transport owns the region) — the `UringBufOwner`
/// semantics the read path had before, now behind the reviewed wrapper.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payload_arm_writes_land_and_bytes_view_matches() {
    // A test's private allocation stands in for the registered uring
    // payload region (the RangedDest convention).
    let mut backing = vec![0xFFu8; 4096];
    let ptr = backing.as_mut_ptr();
    // SAFETY: `backing` outlives every use below; the spawned tasks are
    // the region's only writers while they run.
    let dest = Arc::new(unsafe { AssemblyDest::payload(ptr, 4096) });

    let mut tasks: OwnedTaskSet<()> = OwnedTaskSet::new("test payload");
    for i in 0..4usize {
        let dest = Arc::clone(&dest);
        tasks.spawn(async move {
            // SAFETY: task i owns the exclusive 1 KiB region
            // [i*1024, (i+1)*1024); the backing is alive for the test.
            unsafe {
                dest.write_at(i * 1024, &[(i as u8) + 1; 512]);
                // Short serve: the tail is zeroed (the reused-payload
                // replay rule the read path enforces).
                dest.zero_range(i * 1024 + 512, 512);
            }
            Ok(())
        });
    }
    let (_completed, err) = tasks.join_all().await;
    assert!(err.is_none(), "payload assembly failed: {err:?}");

    let bytes = dest.into_bytes();
    assert_eq!(bytes.len(), 4096);
    for i in 0..4usize {
        assert!(
            bytes[i * 1024..i * 1024 + 512]
                .iter()
                .all(|&b| b == (i as u8) + 1),
            "task {i} write did not land"
        );
        assert!(
            bytes[i * 1024 + 512..(i + 1) * 1024].iter().all(|&b| b == 0),
            "task {i} tail not zeroed"
        );
    }
    drop(bytes);
    // The transport-owned region is untouched by the view drop.
    assert_eq!(backing[0], 1);
}

/// Bounds are LOUD: a write past the destination extent must panic (the
/// alternative is a heap overflow through the raw pointer).
#[test]
#[should_panic(expected = "assembly write past the destination")]
fn write_at_past_len_is_loud() {
    let pool = Arc::new(BufferPool::new(1, 4096));
    let mut buf = pool.alloc();
    buf.resize(4096, 0);
    let dest = AssemblyDest::pooled(buf);
    // SAFETY: sole owner, no concurrent access — the bounds assert is
    // what this test exercises.
    unsafe { dest.write_at(4090, &[0u8; 32]) };
}
