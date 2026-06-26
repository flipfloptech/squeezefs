use squeezefs::cache::pool::BufferPool;
use std::sync::Arc;

#[test]
fn test_pool_alloc_and_reclamation() {
    let pool = Arc::new(BufferPool::new(10, 4 * 1024 * 1024));
    let initial_len = pool.len();
    assert_eq!(
        initial_len, 10,
        "Pool should start with pre-allocated buffers"
    );

    {
        let mut buf = pool.alloc();
        assert_eq!(
            pool.len(),
            9,
            "Allocating a buffer should decrease pool count"
        );
        assert_eq!(
            buf.len(),
            4 * 1024 * 1024,
            "Buffer size should default to 4MB"
        );
        assert!(
            buf.iter().all(|&b| b == 0),
            "Pooled buffer should be zero-initialized"
        );

        // Write some data
        buf[0] = 42;
        buf[100] = 99;
    }

    assert_eq!(
        pool.len(),
        10,
        "Dropping PooledBuf should return it to the pool"
    );

    // Allocate again and verify it is zeroed out on allocation
    let buf = pool.alloc();
    assert_eq!(
        buf[0], 0,
        "Recycled buffer must be zeroed out on next allocation"
    );
    assert_eq!(
        buf[100], 0,
        "Recycled buffer must be zeroed out on next allocation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pool_concurrent_stress() {
    let pool = Arc::new(BufferPool::new(20, 1024));
    let mut handles = vec![];
    for _ in 0..50 {
        let pool_clone = pool.clone();
        let handle = tokio::spawn(async move {
            for _ in 0..100 {
                let mut buf = pool_clone.alloc();
                buf[0] = 1;
                tokio::task::yield_now().await;
                assert_eq!(buf[0], 1);
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(
        pool.len(),
        20,
        "All buffers should be returned to pool after concurrent tasks finish"
    );
}
