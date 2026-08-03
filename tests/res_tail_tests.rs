//! RES-12 / RES-17 / RES-19 / RES-20 lifecycle + latency contracts
//! (pre-RC spec §7).
//!
//! * **RES-12** — `Drop for UringWorker` / `Drop for DataPlaneSink` join OS
//!   threads. Dropped from an async context that blocks a tokio worker for
//!   the whole drain, so the reachable teardown sites hop through
//!   `spawn_blocking`. `Drop` keeps its synchronous join as the backstop:
//!   the dismount-teardown drain contract (`tests/dismount_teardown_tests`)
//!   is unchanged.
//! * **RES-17** — the staging shard kept `active_keys: VecDeque` beside the
//!   authoritative `map` and removed with `retain` (O(n) scan + shift per
//!   removal, under the shard WRITE lock). The queue is gone: no consumer
//!   needed its order, and the duplicate state was its own bug class.
//! * **RES-19** — `free_forensics_tape` was an insert-only global map of
//!   full backtrace strings; it is now a bounded ring of the last N.
//! * **RES-20** — `queue_reclaim_inode` spawned one task per FORGET; the
//!   enqueue is now `try_send` (zero tasks in the common case) with one
//!   shared drainer under genuine backpressure.

use bytes::Bytes;

/// RES-17: removals no longer pay an O(n) scan+shift, and every
/// `active_keys()` consumer still sees exactly the live key SET.
///
/// The three consumers (`offline_device` drain, `online_device`'s
/// `i % share_fraction` rebalance sample, `list_keys` census) are all
/// order-independent, and nothing ever popped the queue's front — the
/// eviction walk that once did was deleted with the geometry-complete
/// eviction fix ("the map is the authority; the queue is bookkeeping").
#[test]
fn staging_shard_active_keys_track_the_map_exactly() {
    // Anonymous (dir-less) cache: one shard, so `list_keys` IS the shard
    // census — no cross-shard concatenation to reason about.
    let shard = squeezefs::tiering::nvme::NvmeCache::new(&[], &[8 << 20], 1)
        .expect("anonymous shard cache");
    for i in 0..64u32 {
        let key = Bytes::from(format!("k{i:04}"));
        shard.put(key, Bytes::from(vec![0xab; 512]));
    }
    let keys = shard.list_keys();
    assert_eq!(keys.len(), 64, "every live key must be listed");
    let mut sorted: Vec<String> = keys
        .iter()
        .map(|k| String::from_utf8(k.to_vec()).unwrap())
        .collect();
    sorted.sort();
    assert_eq!(sorted[0], "k0000");
    assert_eq!(sorted[63], "k0063");

    // Remove a middle key: it leaves the census, the rest survive.
    let victim = Bytes::from("k0032");
    assert!(shard.remove(&victim).is_some(), "the key was live");
    let after = shard.list_keys();
    assert_eq!(after.len(), 63, "removal must retire exactly one key");
    assert!(
        !after.iter().any(|k| k == &victim),
        "a removed key must never linger in the census (the RES-3 \
         tombstone class)"
    );

    // Same-key replace must not duplicate the census entry (the historical
    // remove+push_back pairing is what the queue existed to maintain).
    let dup = Bytes::from("k0001");
    shard.put(dup.clone(), Bytes::from(vec![0xcd; 256]));
    let census = shard.list_keys();
    assert_eq!(
        census.iter().filter(|k| *k == &dup).count(),
        1,
        "a replaced key appears exactly once"
    );
    assert_eq!(census.len(), 63, "a replace must not grow the census");
}

/// RES-19: the forensics tape is a BOUNDED ring. Insert-only growth of
/// full backtrace strings under a global mutex, on the write path, was the
/// defect; capping the retention is the fix.
#[test]
fn free_forensics_tape_is_a_bounded_ring() {
    let cap = squeezefs::block_allocator::FREE_FORENSICS_TAPE_CAP;
    assert!(cap > 0, "the ring must retain something");
    for off in 0..(cap as u64 * 3) {
        squeezefs::block_allocator::record_free_forensics_for_test(off * 4096, "bt");
    }
    let len = squeezefs::block_allocator::free_forensics_tape_len_for_test();
    assert!(
        len <= cap,
        "the tape must never exceed its cap (got {len}, cap {cap}) — it \
         held FULL backtrace strings forever"
    );
    // The most recent entries are the ones a double-free investigation
    // needs; the oldest are evicted.
    let newest = (cap as u64 * 3 - 1) * 4096;
    assert!(
        squeezefs::block_allocator::lookup_free_forensics_for_test(newest).is_some(),
        "the newest recorded free must still be readable"
    );
    assert!(
        squeezefs::block_allocator::lookup_free_forensics_for_test(0).is_none(),
        "the oldest free must have aged out of the ring"
    );
}

/// RES-20: a FORGET storm must not pile one task per inode onto the
/// current lane's `LocalSet`. The enqueue is synchronous (`try_send`) in
/// the common case: no runtime is even required.
#[test]
fn reclaim_enqueue_is_task_free_when_the_queue_has_room() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(1024);
    let q = squeezefs::fuse_client::ReclaimEnqueue::new(tx);
    // No tokio runtime in scope: a spawning implementation panics here.
    for ino in 2..1000u64 {
        q.enqueue(ino);
    }
    let mut seen = 0usize;
    while rx.try_recv().is_ok() {
        seen += 1;
    }
    assert_eq!(
        seen, 998,
        "every FORGET must reach the reclaim queue without a task per ino"
    );
    assert_eq!(
        q.spawned_drainers(),
        0,
        "a queue with room must never spawn a drainer (RES-20)"
    );
}

/// RES-20: genuine backpressure (a full queue) still never loses an ino —
/// it parks on ONE shared drainer, not one task per FORGET.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reclaim_enqueue_overflows_onto_one_shared_drainer() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(4);
    let q = squeezefs::fuse_client::ReclaimEnqueue::new(tx);
    for ino in 2..64u64 {
        q.enqueue(ino);
    }
    assert!(
        q.spawned_drainers() <= 1,
        "at most ONE drainer task may exist for the whole storm (got {})",
        q.spawned_drainers()
    );
    let mut seen = std::collections::HashSet::new();
    // Drain until every ino arrives (the drainer awaits capacity).
    while seen.len() < 62 {
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await {
            Ok(Some(ino)) => {
                seen.insert(ino);
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert_eq!(
        seen.len(),
        62,
        "backpressure must defer, never DROP a FORGET (an orphan ino that \
         never reclaims is a leaked slot)"
    );
}

/// RES-12: the teardown-site hop is observable — `drop_off_runtime` moves
/// a blocking-drop value onto the blocking pool when a runtime is present
/// and drops it inline when there is none (offline CLI verbs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_drop_hops_to_the_blocking_pool_inside_a_runtime() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct JoinsOnDrop {
        done: Arc<AtomicBool>,
    }
    impl Drop for JoinsOnDrop {
        fn drop(&mut self) {
            // Stand-in for the UringWorker/DataPlaneSink thread join.
            std::thread::sleep(std::time::Duration::from_millis(20));
            self.done.store(true, Ordering::SeqCst);
        }
    }

    let done = Arc::new(AtomicBool::new(false));
    let handle = squeezefs::detached::drop_off_runtime(JoinsOnDrop { done: done.clone() });
    let handle = handle.expect("inside a runtime the drop must be handed off");
    handle.await.expect("the blocking drop completes");
    assert!(done.load(Ordering::SeqCst), "the value was actually dropped");
}

/// RES-12: outside a runtime the same helper drops inline — offline verbs
/// and `Drop` backstops must not require a reactor.
#[test]
fn blocking_drop_is_inline_without_a_runtime() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct Marks(Arc<AtomicBool>);
    impl Drop for Marks {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let flag = Arc::new(AtomicBool::new(false));
    let none = squeezefs::detached::drop_off_runtime(Marks(flag.clone()));
    assert!(
        none.is_none(),
        "no runtime ⇒ no handoff (the value dropped inline)"
    );
    assert!(flag.load(Ordering::SeqCst), "the value dropped synchronously");
}
