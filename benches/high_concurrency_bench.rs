#![allow(clippy::needless_range_loop)]

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fuse3::raw::prelude::*;
use fuse3::raw::Request;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::routing::DataRouter;
use squeezefs::cache::pool::BufferPool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::runtime::Runtime;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

fn setup_fs_and_rt() -> Option<(SqueezefsFilesystem, Runtime, tempfile::TempDir)> {
    let rt = Runtime::new().ok()?;
    let redis_url = get_redis_url();

    // Check connection to Garnet/Redis and flush database
    let connection_ok = rt.block_on(async {
        if let Ok(client) = redis::Client::open(redis_url.clone()) {
            if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
                let _: () = redis::cmd("FLUSHALL")
                    .query_async(&mut con)
                    .await
                    .unwrap_or_default();
                true
            } else {
                false
            }
        } else {
            false
        }
    });

    if !connection_ok {
        println!("Skipping benchmarks: Redis/Garnet server not available.");
        return None;
    }

    let dlm = DlmClient::new(&redis_url).ok()?;
    let temp_dir = tempdir().ok()?;
    let block_alloc = rt.block_on(async {
        let alloc = squeezefs::block_allocator::BlockAllocator::new(
            std::sync::Arc::new(dlm.meta_client().clone()),
            "bench_vol_hc",
        )
        .await
        .ok()?;
        Some(std::sync::Arc::new(alloc))
    })?;
    let nvme_path = format!("{}/.squeezefs_nvme", temp_dir.path().display());
    if let Ok(file) = std::fs::File::create(&nvme_path) {
        let _ = file.set_len(512 * 1024 * 1024);
    }
    let nvme_dev = std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(&nvme_path));
    let cache = rt.block_on(async {
        TieredCache::new(
            vec![temp_dir.path().to_path_buf()],
            None,
            None,
            None,
            None,
            dlm.meta_client().clone(),
            block_alloc.clone(),
            nvme_dev.clone(),
        )
        .ok()
    })?;

    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    rt.block_on(async {
        let _ = fs.init(req).await.ok()?;
        Some(())
    })?;

    Some((fs, rt, temp_dir))
}

fn bench_squeezefs_concurrency(c: &mut Criterion) {
    let (fs, rt, _temp_dir) = match setup_fs_and_rt() {
        Some(val) => val,
        None => return,
    };

    let router = fs.router.clone();
    let mut group = c.benchmark_group("squeezefs_concurrency");

    // 1. Concurrent writes: 16 tasks concurrently writing to different files
    let write_data = vec![7u8; 128 * 1024]; // 128KB
    let write_counter = Arc::new(AtomicU64::new(0));
    group.throughput(Throughput::Bytes(128 * 1024 * 16));
    group.bench_function("concurrent_writes_16_tasks", |b| {
        let counter = write_counter.clone();
        let data_ref = &write_data;
        let router_ref = &router;
        b.to_async(&rt).iter(|| {
            let base = counter.fetch_add(16, Ordering::Relaxed);
            let futures: Vec<_> = (0..16).map(|i| {
                let name = format!("bench_concurrent_w_{}.bin", base + i);
                let router = router_ref.clone();
                let data = data_ref.clone();
                tokio::spawn(async move {
                    router.write_file(&name, 0, &data, (base + i) as u64).await.unwrap();
                    let mut con = router.dlm.get_connection().await.unwrap();
                    let _ = router.delete_file(&name, &mut con).await;
                })
            }).collect();
            async move {
                for f in futures {
                    f.await.unwrap();
                }
            }
        });
    });

    // 2. Concurrent reads: 16 tasks concurrently reading from the same file (contended reads)
    let read_file_name = "routing_concurrent_read.bin".to_string();
    rt.block_on(async {
        router.write_file(&read_file_name, 0, &vec![9u8; 128 * 1024], 9999).await.unwrap();
    });
    group.throughput(Throughput::Bytes(128 * 1024 * 16));
    group.bench_function("concurrent_reads_16_tasks_same_file", |b| {
        let router_ref = &router;
        let filename = read_file_name.clone();
        b.to_async(&rt).iter(|| {
            let futures: Vec<_> = (0..16).map(|_| {
                let router = router_ref.clone();
                let fname = filename.clone();
                tokio::spawn(async move {
                    let data = router.read_file(&fname).await.unwrap();
                    assert_eq!(data.len(), 128 * 1024);
                })
            }).collect();
            async move {
                for f in futures {
                    f.await.unwrap();
                }
            }
        });
    });

    // 3. Concurrent locks: 16 tasks concurrently locking/unlocking distinct ranges
    let dlm_ref = &router.dlm;
    group.bench_function("concurrent_locks_16_tasks", |b| {
        let dlm = dlm_ref.clone();
        b.to_async(&rt).iter(|| {
            let futures: Vec<_> = (0..16).map(|i| {
                let dlm = dlm.clone();
                tokio::spawn(async move {
                    let lock_key = format!("lock_{}", i);
                    let lease = dlm.acquire_lock(&lock_key, None, Duration::from_secs(3)).await.unwrap();
                    assert!(lease.fencing_token() > 0);
                    lease.release().await.unwrap();
                })
            }).collect();
            async move {
                for f in futures {
                    f.await.unwrap();
                }
            }
        });
    });

    // 4. Concurrent buffer pool allocation: 16 tasks concurrently allocating/deallocating buffer pools
    let pool = Arc::new(BufferPool::new(128, 1024 * 1024)); // 128 capacity, 1MB blocks
    group.bench_function("concurrent_pool_alloc_16_tasks", |b| {
        let pool = pool.clone();
        b.to_async(&rt).iter(|| {
            let futures: Vec<_> = (0..16).map(|_| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    let mut buf = pool.alloc();
                    assert_eq!(buf.len(), 1024 * 1024);
                    buf[0] = 42;
                })
            }).collect();
            async move {
                for f in futures {
                    f.await.unwrap();
                }
            }
        });
    });

    group.finish();
}

fn custom_criterion() -> Criterion {
    Criterion::default()
        .measurement_time(Duration::from_secs(3))
        .warm_up_time(Duration::from_secs(1))
        .sample_size(30)
}

criterion_group! {
    name = benches;
    config = custom_criterion();
    targets = bench_squeezefs_concurrency
}
criterion_main!(benches);
