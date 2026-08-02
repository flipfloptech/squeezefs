//! DLM S0 — the `LockManager` trait surface (spec §6.9 stage S0).
//!
//! S0 ships the trait + `LocalLockManager` with byte-identical semantics
//! to the historical `DlmClient` body. These contracts run GENERICALLY
//! over `impl LockManager` — the same code path a later stage's remote
//! implementation (the S4 slot lock manager) must satisfy — so the trait
//! is a load-bearing surface from day one, not a decoration:
//!
//! 1. acquire → token snapshot ≡ generator read; release makes the key
//!    immediately reacquirable with a strictly greater token;
//! 2. conflicting acquisition fails BOUNDED (time budget, not wakeups);
//! 3. the `DlmClient` handle alias IS the local implementation (call
//!    sites and the trait see one object).

use squeezefs::dlm::{DlmClient, LocalLockManager, LockManager};
use std::time::Duration;

/// Generic body: every `LockManager` implementation must pass this.
async fn acquire_release_monotone_contract<M: LockManager>(mgr: &M, key: &str, ino: u64) {
    let l1 = mgr
        .acquire_lock(key, None, Duration::from_secs(5))
        .await
        .expect("acquire 1");
    let t1 = l1.fencing_token();
    assert!(t1 > 0, "a real grant carries token N > 0");
    assert_eq!(
        mgr.get_fencing_token(key),
        t1,
        "generator read must match the lease snapshot (trait surface)"
    );
    assert_eq!(
        mgr.get_fencing_token_ino(ino),
        t1,
        "the binary ino read is the same generator"
    );
    l1.release().await.expect("release 1");

    let l2 = mgr
        .acquire_lock(key, None, Duration::from_secs(5))
        .await
        .expect("acquire 2");
    assert!(
        l2.fencing_token() > t1,
        "fencing token must be strictly monotone across re-acquisition: {} -> {}",
        t1,
        l2.fencing_token()
    );
    l2.release().await.expect("release 2");
}

async fn bounded_conflict_contract<M: LockManager>(a: &M, b: &M, key: &str) {
    let held = a
        .acquire_lock(key, None, Duration::from_secs(5))
        .await
        .expect("holder acquire");
    let res = tokio::time::timeout(
        Duration::from_secs(3),
        b.acquire_lock(key, None, Duration::from_millis(200)),
    )
    .await;
    match res {
        Err(_) => panic!("conflicting acquire hung unbounded (200ms ttl)"),
        Ok(Ok(_)) => panic!("exclusive lock must conflict while held"),
        Ok(Err(_)) => {} // bounded, loud — the contract
    }
    held.release().await.expect("holder release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_lock_manager_satisfies_the_trait_contract() {
    let mgr = LocalLockManager::new().expect("local manager");
    acquire_release_monotone_contract(&mgr, "inode_960001", 960001).await;

    let a = LocalLockManager::new().expect("a");
    let b = LocalLockManager::new().expect("b");
    bounded_conflict_contract(&a, &b, "inode_960002").await;
}

/// The product-facing handle name resolves to the local implementation:
/// one object, both surfaces (inherent methods for the ~200 historical
/// call sites, the trait for stage consumers).
#[tokio::test]
async fn dlm_client_is_the_local_lock_manager() {
    fn is_lock_manager<M: LockManager>(_: &M) {}
    let dlm = DlmClient::new().expect("dlm");
    is_lock_manager(&dlm);
    acquire_release_monotone_contract(&dlm, "inode_960003", 960003).await;
}
