//! Cluster-DLM (local backend) locking contracts.
//!
//! The local DLM backs every lease acquisition (`get_or_acquire_lease`),
//! rename/copy_range lock pairs, and all fencing-token checks. Contracts:
//!
//! 1. A waiter blocked on key A must not be starved or failed by churn on
//!    unrelated keys. (Today a single global `Notify` wakes every waiter on
//!    every release and each wakeup burns one bounded "retry" → spurious
//!    `LockFailed` while A is still legitimately held.)
//! 2. A release must promptly wake a waiter racing its way into the wait —
//!    the notify-registration must happen *before* the availability check
//!    (tokio `Notified::enable` protocol). A missed wakeup parks the waiter
//!    until some unrelated release happens to land, or forever.
//! 3. Fencing tokens are monotonic per object across re-acquisition, and a
//!    lease's token matches the generator's current value.
//! 4. Whole-file and range locks on the same path are distinct objects;
//!    identical ranges conflict.
//! 5. Release is idempotent across clones (drop-release + explicit release).
//!
//! All keys use unique inode numbers per test: the lock/fencing maps are
//! process-global statics shared across the test binary.

use squeezefs::dlm::DlmClient;
use std::time::Duration;

fn dlm() -> DlmClient {
    DlmClient::new().expect("local dlm")
}

/// Contract 1: churn on unrelated keys must not fail a legitimate waiter.
///
/// Deterministic on a current-thread runtime: the waiter parks first, then
/// 25 acquire/release cycles on an unrelated key fire the (global) release
/// notification while the waiter's key stays held. The waiter must survive
/// all of them and acquire as soon as its own key is released.
#[tokio::test]
async fn test_waiter_survives_unrelated_key_churn() {
    let a = dlm();
    let b = dlm();

    let held = a
        .acquire_lock("inode_910001", None, Duration::from_secs(10))
        .await
        .expect("initial acquire");

    let waiter = tokio::spawn({
        let b = b.clone();
        async move {
            b.acquire_lock("inode_910001", None, Duration::from_secs(10))
                .await
        }
    });
    // Let the waiter reach its wait point.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    // Unrelated-key churn: every release used to wake ALL waiters.
    for _ in 0..25 {
        let l = a
            .acquire_lock("inode_910002", None, Duration::from_secs(10))
            .await
            .expect("churn acquire");
        l.release().await.expect("churn release");
        tokio::task::yield_now().await;
    }

    held.release().await.expect("release held key");

    let got = tokio::time::timeout(Duration::from_secs(8), waiter)
        .await
        .expect("waiter timed out (starved)")
        .expect("waiter task panicked");
    assert!(
        got.is_ok(),
        "waiter failed after unrelated-key churn while its key was simply held: {:?}",
        got.err()
    );
}

/// Contract 2: a release racing the waiter into its wait must still wake it.
///
/// Stress: holder releases immediately after the waiter task is spawned on
/// another worker thread — repeatedly hitting the check-then-register
/// window. Any lost wakeup parks that round's waiter with nothing left to
/// wake it (no other releases happen on that key) and trips the timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_release_racing_waiter_never_loses_wakeup() {
    let holder = dlm();
    let contender = dlm();

    for round in 0..400u32 {
        let key = format!("inode_92{:04}", round % 7);
        let lease = holder
            .acquire_lock(&key, None, Duration::from_secs(10))
            .await
            .expect("holder acquire");

        let waiter = tokio::spawn({
            let contender = contender.clone();
            let key = key.clone();
            async move {
                contender
                    .acquire_lock(&key, None, Duration::from_secs(10))
                    .await
            }
        });

        // Release while the waiter is (maybe) between its failed check and
        // its wait registration.
        lease.release().await.expect("holder release");

        let got = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap_or_else(|_| panic!("round {round}: waiter hung — lost release wakeup"))
            .expect("waiter task panicked");
        let lease2 = got.unwrap_or_else(|e| panic!("round {round}: waiter failed: {e:?}"));
        lease2.release().await.expect("waiter release");
    }
}

/// Contract 3: fencing tokens are monotonic per object across re-acquisition
/// and the map read matches the lease's snapshot.
#[tokio::test]
async fn test_fencing_token_monotonic_and_readable() {
    let d = dlm();

    let l1 = d
        .acquire_lock("inode_930001", None, Duration::from_secs(5))
        .await
        .expect("acquire 1");
    let t1 = l1.fencing_token();
    assert_eq!(
        d.get_fencing_token("inode_930001"),
        t1,
        "generator read must match the lease snapshot"
    );
    l1.release().await.expect("release 1");

    let l2 = d
        .acquire_lock("inode_930001", None, Duration::from_secs(5))
        .await
        .expect("acquire 2");
    assert!(
        l2.fencing_token() > t1,
        "fencing token must be monotonic: {} -> {}",
        t1,
        l2.fencing_token()
    );
    assert_eq!(d.get_fencing_token("inode_930001"), l2.fencing_token());
}

/// Contract 4 (S11 — byte-range custody): whole-file custody conflicts
/// with every byte range, and an identical range conflicts with itself.
///
/// **This contract inverted at S11.** Pre-S11 a range rode the lock KEY,
/// so `Some((0,4096))` was a different lock *object* than `None` and was
/// granted straight through a held whole-file lease — two writers could
/// believe they owned the same bytes. The lock table is now keyed by FILE
/// and carries the file's live spans, so whole-file custody (the strongest
/// custody, and what the write path's `get_or_acquire_lease` takes) is
/// mutually exclusive with every span. Disjoint spans still proceed in
/// parallel — `tests/dlm_range_custody_tests.rs` owns that half.
#[tokio::test]
async fn test_range_and_whole_file_locks_are_distinct() {
    let d = dlm();
    let e = dlm();

    let whole = d
        .acquire_lock("inode_940001", None, Duration::from_secs(5))
        .await
        .expect("whole-file acquire");

    // A range on the same file is inside whole-file custody: it must
    // arbitrate, with a *bounded* failure while the whole-file lease is
    // held. With no churn anywhere, a wakeup-counting waiter has nothing to
    // count and hangs forever — the wait budget must be time (the ttl
    // parameter), not wakeups.
    match tokio::time::timeout(
        Duration::from_secs(3),
        e.acquire_lock("inode_940001", Some((0, 4096)), Duration::from_millis(300)),
    )
    .await
    {
        Err(_) => panic!(
            "conflicting acquire hung unbounded (300ms ttl, no churn): wait budget must be time-based"
        ),
        Ok(Ok(_)) => panic!("a byte range must not be granted under a held whole-file lease"),
        Ok(Err(_)) => {} // bounded, loud — correct
    }
    whole.release().await.expect("release whole");

    // With whole-file custody gone the span is grantable, and the exact
    // same span from another client then conflicts with itself.
    let range = tokio::time::timeout(
        Duration::from_secs(2),
        e.acquire_lock("inode_940001", Some((0, 4096)), Duration::from_secs(5)),
    )
    .await
    .expect("range acquire hung after the whole-file release")
    .expect("range acquire failed");
    match tokio::time::timeout(
        Duration::from_secs(3),
        d.acquire_lock("inode_940001", Some((0, 4096)), Duration::from_millis(300)),
    )
    .await
    {
        Err(_) => panic!("identical-range acquire hung unbounded (300ms ttl, no churn)"),
        Ok(Ok(_)) => panic!("identical range must conflict while held"),
        Ok(Err(_)) => {}
    }
    range.release().await.expect("release range");
}

/// Contract 5: release is idempotent across clones; the key is reusable
/// afterwards and drop-release also frees it.
#[tokio::test]
async fn test_release_idempotent_across_clones() {
    let d = dlm();

    let l1 = d
        .acquire_lock("inode_950001", None, Duration::from_secs(5))
        .await
        .expect("acquire");
    let l1_clone = l1.clone();
    assert!(l1.is_held().await);

    l1.release().await.expect("first release");
    drop(l1_clone); // second logical release via Drop — must be a no-op

    // Key must be immediately reacquirable.
    let l2 = tokio::time::timeout(
        Duration::from_secs(2),
        d.acquire_lock("inode_950001", None, Duration::from_secs(5)),
    )
    .await
    .expect("reacquire hung after release")
    .expect("reacquire failed");

    // Drop-release path: no explicit release.
    drop(l2);
    let l3 = tokio::time::timeout(
        Duration::from_secs(2),
        d.acquire_lock("inode_950001", None, Duration::from_secs(5)),
    )
    .await
    .expect("reacquire hung after drop-release")
    .expect("reacquire failed after drop-release");
    l3.release().await.expect("final release");
}
