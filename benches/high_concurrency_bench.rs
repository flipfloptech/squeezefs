use criterion::{black_box, criterion_group, criterion_main, Criterion};
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

/// Quantifies the inline small-write zero-copy win: `CachedMetadata` is cloned
/// ~3x per small write (moka `get`, `meta.clone()`). With `data_key: Bytes` the
/// inline-payload clone is an O(1) refcount bump; the old `Vec<u8>` layout paid a
/// full deep copy of the payload on every clone. Both are benched side by side.
fn bench_metadata_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("metadata_clone");

    let inline_4k = squeezefs::routing::CachedMetadata {
        file_type: "inline".to_string(),
        size: 4096,
        data_key: Some(bytes::Bytes::from(vec![0xABu8; 4096])),
        ..Default::default()
    };
    group.bench_function("cached_metadata_clone_inline_4k_bytes", |b| {
        b.iter(|| {
            let m = black_box(&inline_4k).clone();
            black_box(m);
        });
    });

    // Reference point: the per-clone cost the old `data_key: Vec<u8>` layout paid.
    let payload_vec = vec![0xABu8; 4096];
    group.bench_function("vec_u8_deep_copy_4k", |b| {
        b.iter(|| {
            let v = black_box(&payload_vec).clone();
            black_box(v);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_high_concurrency, bench_metadata_clone);
criterion_main!(benches);
