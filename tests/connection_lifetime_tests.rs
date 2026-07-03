//! P1-10: Redis/Garnet connections must not be held across long backend I/O.
//!
//! Protocol tests model the required scoping. Hot-path correctness remains covered
//! by `routing_layout_tests` / FUSE write paths.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Stand-in for a pooled meta connection: tracks concurrent holders.
struct FakeMetaPool {
    in_use: AtomicUsize,
}

struct FakeCon<'a> {
    pool: &'a FakeMetaPool,
}

impl Drop for FakeCon<'_> {
    fn drop(&mut self) {
        self.pool.in_use.fetch_sub(1, Ordering::SeqCst);
    }
}

impl FakeMetaPool {
    fn new() -> Self {
        Self {
            in_use: AtomicUsize::new(0),
        }
    }

    async fn acquire(&self) -> FakeCon<'_> {
        self.in_use.fetch_add(1, Ordering::SeqCst);
        FakeCon { pool: self }
    }

    fn holders(&self) -> usize {
        self.in_use.load(Ordering::SeqCst)
    }
}

/// P1-10 contract: after meta-prep, the connection is dropped before simulated
/// long backend I/O so another task can acquire the pool slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_meta_con_released_before_backend_io() {
    let pool = Arc::new(FakeMetaPool::new());
    let io_started = Arc::new(tokio::sync::Notify::new());
    let peer_acquired = Arc::new(tokio::sync::Notify::new());

    let pool_a = pool.clone();
    let io_started_a = io_started.clone();
    let peer_acquired_a = peer_acquired.clone();
    let writer = tokio::spawn(async move {
        // meta-prep
        {
            let _con = pool_a.acquire().await;
            assert_eq!(pool_a.holders(), 1);
            // fencing / type / size only
        } // drop con

        io_started_a.notify_one();
        // long NVMe / stage / stripe payload — no meta con held
        assert_eq!(
            pool_a.holders(),
            0,
            "must not hold meta con during backend IO"
        );
        peer_acquired_a.notified().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        // meta commit with a fresh connection
        let _con = pool_a.acquire().await;
        assert_eq!(pool_a.holders(), 1);
    });

    io_started.notified().await;

    let pool_b = pool.clone();
    let peer = tokio::spawn(async move {
        let acquired = tokio::time::timeout(Duration::from_millis(50), pool_b.acquire()).await;
        assert!(
            acquired.is_ok(),
            "peer must acquire meta con while writer is in backend IO phase"
        );
        peer_acquired.notify_one();
        drop(acquired.unwrap());
    });

    peer.await.expect("peer join");
    writer.await.expect("writer join");
    assert_eq!(pool.holders(), 0);
}

/// Jobs-worker contract: SPOP under a short-lived connection; task body (NVMe)
/// runs with zero held meta connections; completion updates re-acquire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_job_worker_releases_con_during_task_body() {
    let pool = Arc::new(FakeMetaPool::new());
    let max_during_body = Arc::new(AtomicUsize::new(0));

    // pop phase
    let task = {
        let _con = pool.acquire().await;
        "block_move".to_string()
    }; // drop before execute

    let pool_b = pool.clone();
    let max_b = max_during_body.clone();
    // simulate execute_task (long IO) while observing holders
    let body = tokio::spawn(async move {
        for _ in 0..10 {
            max_b.fetch_max(pool_b.holders(), Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(task, "block_move");
    });

    body.await.expect("body");
    assert_eq!(
        max_during_body.load(Ordering::SeqCst),
        0,
        "no meta con during job body / NVMe move"
    );

    // completion register
    {
        let _con = pool.acquire().await;
        // SADD completed
    }
    assert_eq!(pool.holders(), 0);
}

/// Nested free_block after mapping commit must not require the same live con
/// across the free (models write_striped cleanup: redis first, free after drop).
#[tokio::test]
async fn test_block_free_after_meta_commit_not_nested_on_same_con() {
    let pool = FakeMetaPool::new();
    let freed = Arc::new(Mutex::new(false));

    let keys_to_free = {
        let mut con = pool.acquire().await;
        // pipe_update / hdel refcounts — redis only
        let _ = &mut con;
        vec!["42".to_string()]
    }; // drop con before free

    assert_eq!(pool.holders(), 0);
    for _k in keys_to_free {
        // free_block analogue (NVMe / allocator) with no meta con
        assert_eq!(pool.holders(), 0);
        *freed.lock().await = true;
    }
    assert!(*freed.lock().await);
}
