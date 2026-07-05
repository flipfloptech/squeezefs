use criterion::{criterion_group, criterion_main, Criterion};
use squeezefs::cache::lru::LruCache;
use squeezefs::fuse_client::StripeLocks;
use squeezefs::meta_backend::dlm::DlmLockManager;
use std::sync::Arc;
use tokio::runtime::Runtime;

fn bench_high_concurrency(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("high_concurrency_contention");

    // 1. DlmLockManager lock contention
    let dlm = Arc::new(DlmLockManager::new());
    group.bench_function("dlm_lock_contention", |b| {
        b.to_async(&rt).iter(|| {
            let dlm = dlm.clone();
            async move {
                let futures = (0..10).map(|_| {
                    let dlm = dlm.clone();
                    async move {
                        let _guard = dlm.lock_shared("contended_key").await;
                        tokio::task::yield_now().await;
                    }
                });
                futures::future::join_all(futures).await;
            }
        });
    });

    // 2. StripeLocks contention
    let active_inode_locks = Arc::new(StripeLocks::<tokio::sync::RwLock<()>, 4096>::new());
    group.bench_function("stripe_locks_contention", |b| {
        b.to_async(&rt).iter(|| {
            let active_inode_locks = active_inode_locks.clone();
            async move {
                let futures = (0..10).map(|_| {
                    let active_inode_locks = active_inode_locks.clone();
                    async move {
                        let lock = active_inode_locks.get_inode_lock(42);
                        let _guard = lock.read().await;
                        tokio::task::yield_now().await;
                    }
                });
                futures::future::join_all(futures).await;
            }
        });
    });

    // 3. LRU Cache contention
    let cache = Arc::new(LruCache::with_capacity(1024 * 1024 * 10)); // 10MB
    let test_bytes = bytes::Bytes::from(vec![0u8; 4096]); // 4KB block
    group.bench_function("lru_cache_contention", |b| {
        b.to_async(&rt).iter(|| {
            let cache = cache.clone();
            let test_bytes = test_bytes.clone();
            async move {
                let futures = (0..10).map(|i| {
                    let cache = cache.clone();
                    let test_bytes = test_bytes.clone();
                    async move {
                        let key = format!("block_{}", i);
                        cache.put(&key, test_bytes);
                        let _ = cache.get(&key);
                        tokio::task::yield_now().await;
                    }
                });
                futures::future::join_all(futures).await;
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_high_concurrency);
criterion_main!(benches);
