use criterion::{criterion_group, criterion_main, Criterion, Throughput, BatchSize};
use fuse3::raw::prelude::*;
use fuse3::raw::Request;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
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
                    .unwrap_or(());
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
    let backend = rt.block_on(RustFsClient::new());
    let temp_dir = tempdir().ok()?;
    let cache = rt.block_on(async {
        TieredCache::new(
            vec![temp_dir.path().to_path_buf()],
            None,
            None,
            None,
            None,
            backend.clone(),
            dlm.meta_client().clone(),
        )
        .ok()
    })?;

    let router = DataRouter::new(dlm.clone(), backend, cache);
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

fn bench_squeezefs_routing(c: &mut Criterion) {
    let (fs, rt, _temp_dir) = match setup_fs_and_rt() {
        Some(val) => val,
        None => return,
    };

    let router = fs.router.clone();
    let mut group = c.benchmark_group("squeezefs_routing_writes");

    // Bench Micro-file routing path (< 64KB)
    let micro_data = vec![8u8; 1024]; // 1KB
    let micro_counter = Arc::new(AtomicU64::new(0));
    group.throughput(Throughput::Bytes(1024));
    group.bench_function("write_micro_file_1kb", |b| {
        let counter = micro_counter.clone();
        let data_ref = &micro_data;
        let router_ref = &router;
        b.to_async(&rt).iter(|| {
            let c = counter.fetch_add(1, Ordering::Relaxed);
            let name = format!("bench_micro_{}.bin", c);
            async move {
                router_ref.write_file(&name, 0, data_ref, 1).await.unwrap();
            }
        });
    });

    // Bench Small-file routing path (64KB - 4MB)
    let small_data = vec![8u8; 128 * 1024]; // 128KB
    let small_counter = Arc::new(AtomicU64::new(0));
    group.throughput(Throughput::Bytes(128 * 1024));
    group.bench_function("write_small_file_128kb", |b| {
        let counter = small_counter.clone();
        let data_ref = &small_data;
        let router_ref = &router;
        b.to_async(&rt).iter(|| {
            let c = counter.fetch_add(1, Ordering::Relaxed);
            let name = format!("bench_small_{}.bin", c);
            async move {
                router_ref.write_file(&name, 0, data_ref, 2).await.unwrap();
            }
        });
    });

    group.finish();
}

fn bench_squeezefs_posix_locks(c: &mut Criterion) {
    let (fs, rt, _temp_dir) = match setup_fs_and_rt() {
        Some(val) => val,
        None => return,
    };

    let req = Request {
        unique: 2,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    // Pre-create file for lock benchmarks
    let ino = rt.block_on(async {
        let reply = fs
            .create(req, 1, OsStr::new("lock_bench_file.bin"), 0o644, 0)
            .await
            .unwrap();
        reply.attr.ino
    });

    let mut group = c.benchmark_group("squeezefs_posix_locks");

    group.bench_function("setlk_acquire_release", |b| {
        b.to_async(&rt).iter(|| async {
            // Lock
            fs.setlk(
                req,
                ino,
                101,
                1001,
                0,
                100,
                libc::F_WRLCK as u32,
                1234,
                false,
            )
            .await
            .unwrap();
            // Unlock
            fs.setlk(
                req,
                ino,
                101,
                1001,
                0,
                100,
                libc::F_UNLCK as u32,
                1234,
                false,
            )
            .await
            .unwrap();
        });
    });

    // Pre-acquire a lock for getlk to check conflict
    rt.block_on(async {
        fs.setlk(
            req,
            ino,
            101,
            1001,
            0,
            100,
            libc::F_WRLCK as u32,
            1234,
            false,
        )
        .await
        .unwrap();
    });

    group.bench_function("getlk_check", |b| {
        b.to_async(&rt).iter(|| async {
            fs.getlk(req, ino, 102, 1002, 50, 150, libc::F_WRLCK as u32, 5678)
                .await
                .unwrap();
        });
    });

    group.finish();
}

fn bench_squeezefs_metadata_ops(c: &mut Criterion) {
    let (fs, rt, _temp_dir) = match setup_fs_and_rt() {
        Some(val) => val,
        None => return,
    };

    let req = Request {
        unique: 3,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    let mut group = c.benchmark_group("squeezefs_metadata_ops");

    let create_counter = Arc::new(AtomicU64::new(0));
    group.bench_function("create_and_unlink", |b| {
        let counter = create_counter.clone();
        let fs_ref = &fs;
        b.to_async(&rt).iter(|| {
            let c = counter.fetch_add(1, Ordering::Relaxed);
            let name = format!("file_{}.bin", c);
            async move {
                let _reply = fs_ref
                    .create(req, 1, OsStr::new(&name), 0o644, 0)
                    .await
                    .unwrap();
                fs_ref.unlink(req, 1, OsStr::new(&name)).await.unwrap();
            }
        });
    });

    let mkdir_counter = Arc::new(AtomicU64::new(0));
    group.bench_function("mkdir_and_rmdir", |b| {
        let counter = mkdir_counter.clone();
        let fs_ref = &fs;
        b.to_async(&rt).iter(|| {
            let c = counter.fetch_add(1, Ordering::Relaxed);
            let name = format!("dir_{}", c);
            async move {
                let _reply = fs_ref
                    .mkdir(req, 1, OsStr::new(&name), 0o755, 0)
                    .await
                    .unwrap();
                fs_ref.rmdir(req, 1, OsStr::new(&name)).await.unwrap();
            }
        });
    });

    // Pre-create static file for lookup and setattr
    let static_ino = rt.block_on(async {
        let reply = fs
            .create(req, 1, OsStr::new("static_bench_file.bin"), 0o644, 0)
            .await
            .unwrap();
        reply.attr.ino
    });

    group.bench_function("lookup_and_getattr", |b| {
        b.to_async(&rt).iter(|| async {
            let reply = fs
                .lookup(req, 1, OsStr::new("static_bench_file.bin"))
                .await
                .unwrap();
            let _attr = fs.getattr(req, reply.attr.ino, None, 0).await.unwrap();
        });
    });

    group.bench_function("setattr_metadata", |b| {
        b.to_async(&rt).iter(|| async {
            let set_attr = SetAttr {
                mode: Some(0o755),
                uid: Some(2000),
                gid: Some(2000),
                ..Default::default()
            };
            fs.setattr(req, static_ino, None, set_attr).await.unwrap();
        });
    });

    // Setup directory with 10 files for readdir listing
    let dir_ino = rt.block_on(async {
        let reply = fs
            .mkdir(req, 1, OsStr::new("readdir_bench_dir"), 0o755, 0)
            .await
            .unwrap();
        let ino = reply.attr.ino;
        for i in 0..10 {
            let name = format!("file_{}.bin", i);
            let _ = fs
                .create(req, ino, OsStr::new(&name), 0o644, 0)
                .await
                .unwrap();
        }
        ino
    });

    group.bench_function("readdir_list", |b| {
        b.to_async(&rt).iter(|| async {
            let _reply = fs.readdir(req, dir_ino, 0, 0).await.unwrap();
        });
    });

    group.finish();
}

fn bench_squeezefs_data_io(c: &mut Criterion) {
    let (fs, rt, _temp_dir) = match setup_fs_and_rt() {
        Some(val) => val,
        None => return,
    };

    let req = Request {
        unique: 4,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    // Pre-create files of different sizes
    let (micro_ino, small_ino, large_ino) = rt.block_on(async {
        let micro_rep = fs
            .create(req, 1, OsStr::new("micro_io.bin"), 0o644, 0)
            .await
            .unwrap();
        fs.write(req, micro_rep.attr.ino, 0, 0, &vec![8u8; 1024], 0, 0)
            .await
            .unwrap();

        let small_rep = fs
            .create(req, 1, OsStr::new("small_io.bin"), 0o644, 0)
            .await
            .unwrap();
        fs.write(req, small_rep.attr.ino, 0, 0, &vec![8u8; 128 * 1024], 0, 0)
            .await
            .unwrap();

        let large_rep = fs
            .create(req, 1, OsStr::new("large_io.bin"), 0o644, 0)
            .await
            .unwrap();
        fs.write(
            req,
            large_rep.attr.ino,
            0,
            0,
            &vec![8u8; 4 * 1024 * 1024],
            0,
            0,
        )
        .await
        .unwrap();

        (micro_rep.attr.ino, small_rep.attr.ino, large_rep.attr.ino)
    });

    let mut group = c.benchmark_group("squeezefs_data_io");

    group.throughput(Throughput::Bytes(1024));
    group.bench_function("read_micro_file_1kb", |b| {
        b.to_async(&rt).iter(|| async {
            let _reply = fs.read(req, micro_ino, 0, 0, 1024).await.unwrap();
        });
    });

    group.throughput(Throughput::Bytes(128 * 1024));
    group.bench_function("read_small_file_128kb", |b| {
        b.to_async(&rt).iter(|| async {
            let _reply = fs.read(req, small_ino, 0, 0, 128 * 1024).await.unwrap();
        });
    });

    let write_large_counter = Arc::new(AtomicU64::new(0));
    let large_data = vec![8u8; 4 * 1024 * 1024]; // 4MB
    group.throughput(Throughput::Bytes(4 * 1024 * 1024));
    group.bench_function("write_large_striped_4mb", |b| {
        let counter = write_large_counter.clone();
        let data_ref = &large_data;
        let fs_ref = &fs;
        b.to_async(&rt).iter(|| {
            let c = counter.fetch_add(1, Ordering::Relaxed);
            let name = format!("large_write_{}.bin", c);
            async move {
                let reply = fs_ref
                    .create(req, 1, OsStr::new(&name), 0o644, 0)
                    .await
                    .unwrap();
                fs_ref
                    .write(req, reply.attr.ino, 0, 0, data_ref, 0, 0)
                    .await
                    .unwrap();
                fs_ref.unlink(req, 1, OsStr::new(&name)).await.unwrap();
            }
        });
    });

    group.throughput(Throughput::Bytes(4 * 1024 * 1024));
    group.bench_function("read_large_striped_4mb", |b| {
        b.to_async(&rt).iter(|| async {
            let _reply = fs
                .read(req, large_ino, 0, 0, 4 * 1024 * 1024)
                .await
                .unwrap();
        });
    });

    group.finish();
}

fn bench_nvme_combined(c: &mut Criterion) {
    let (fs, rt, _temp_dir) = match setup_fs_and_rt() {
        Some(val) => val,
        None => return,
    };

    let router = fs.router.clone();
    let nvme = router.cache.nvme.clone();
    let mut group = c.benchmark_group("bench_nvme_combined");
    group.throughput(Throughput::Bytes(64 * 1024));

    let test_data = vec![7u8; 64 * 1024]; // 64KB
    let counter = Arc::new(AtomicU64::new(0));

    group.bench_function("stage_write_64kb", |b| {
        let counter_clone = counter.clone();
        let data_ref = &test_data;
        let nvme_ref = &nvme;
        b.to_async(&rt).iter(|| {
            let c = counter_clone.fetch_add(1, Ordering::Relaxed);
            let file_path = format!("/bench_staged_{}.bin", c);
            let file_id = format!("bench_id_{}", c);
            async move {
                nvme_ref
                    .stage_write(&file_path, &file_id, data_ref, 1)
                    .await
                    .unwrap();
            }
        });
    });

    group.bench_function("read_staged_64kb", |b| {
        let counter_clone = counter.clone();
        let nvme_ref = &nvme;
        b.to_async(&rt).iter(|| {
            let c = counter_clone.load(Ordering::Relaxed);
            let file_id = format!("bench_id_{}", c.saturating_sub(1));
            async move {
                let _ = nvme_ref.read_staged(&file_id);
            }
        });
    });

    group.finish();
}

fn bench_dlm_centralized_heartbeat(c: &mut Criterion) {
    let (fs, rt, _temp_dir) = match setup_fs_and_rt() {
        Some(val) => val,
        None => return,
    };

    let dlm = fs.router.dlm.clone();
    let mut group = c.benchmark_group("bench_dlm_centralized_heartbeat");

    let counter = Arc::new(AtomicU64::new(0));

    group.bench_function("acquire_and_release_lock", |b| {
        let dlm_ref = &dlm;
        let counter_clone = counter.clone();
        b.to_async(&rt).iter(|| {
            let c = counter_clone.fetch_add(1, Ordering::Relaxed);
            let path = format!("/bench_lock_{}", c);
            async move {
                let lease = dlm_ref
                    .acquire_lock(&path, None, Duration::from_secs(5))
                    .await
                    .unwrap();
                lease.release().await.unwrap();
            }
        });
    });

    group.finish();
}

fn bench_crypto_compress(c: &mut Criterion) {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use squeezefs::crypto_compress::CryptoCompressState;

    let mut rng = rand::thread_rng();
    let priv_key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let pem = priv_key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();

    let state_none = CryptoCompressState::new("none".to_string(), "none".to_string(), None);
    let state_lz4 = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
    let state_zstd = CryptoCompressState::new("zstd".to_string(), "none".to_string(), None);
    let state_enc =
        CryptoCompressState::new("none".to_string(), "aes256gcm-rsa".to_string(), Some(&pem));
    let state_both =
        CryptoCompressState::new("lz4".to_string(), "aes256gcm-rsa".to_string(), Some(&pem));

    let payload =
        b"Hello World! This is a test of client-side encryption and compression. ".repeat(100); // ~7KB

    let mut group = c.benchmark_group("crypto_compress");

    group.bench_function("process_write_none", |b| {
        b.iter(|| state_none.process_write(&payload).unwrap());
    });
    group.bench_function("process_write_lz4", |b| {
        b.iter(|| state_lz4.process_write(&payload).unwrap());
    });
    group.bench_function("process_write_zstd", |b| {
        b.iter(|| state_zstd.process_write(&payload).unwrap());
    });
    group.bench_function("process_write_aes256gcm", |b| {
        b.iter(|| state_enc.process_write(&payload).unwrap());
    });
    group.bench_function("process_write_both", |b| {
        b.iter(|| state_both.process_write(&payload).unwrap());
    });

    let encrypted = state_both.process_write(&payload).unwrap();
    group.bench_function("process_read_both", |b| {
        b.iter(|| state_both.process_read(&encrypted).unwrap());
    });

    group.finish();
}

fn bench_squeezefs_dht_and_p2p_at_scale(c: &mut Criterion) {
    let mut group = c.benchmark_group("squeezefs_dht_and_p2p_at_scale");
    group.sample_size(10);
    group.warm_up_time(std::time::Duration::from_millis(500));
    group.measurement_time(std::time::Duration::from_secs(2));

    let rt = match Runtime::new() {
        Ok(val) => val,
        Err(_) => return,
    };
    let redis_url = get_redis_url();

    // Verify Redis/Garnet server connection
    let connection_ok = rt.block_on(async {
        if let Ok(client) = redis::Client::open(redis_url.clone()) {
            if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
                let _: () = redis::cmd("FLUSHALL")
                    .query_async(&mut con)
                    .await
                    .unwrap_or(());
                true
            } else {
                false
            }
        } else {
            false
        }
    });

    if !connection_ok {
        println!("Skipping scale benchmarks: Redis/Garnet server not available.");
        return;
    }

    let dlm = match DlmClient::new(&redis_url) {
        Ok(val) => val,
        Err(_) => return,
    };

    let mut temp_dirs = Vec::new();
    let mut nodes = Vec::new();

    // Spawn 5 nodes locally
    rt.block_on(async {
        for i in 0..5 {
            let temp_dir = tempdir().unwrap();
            let backend = RustFsClient::new_mock();
            let cache = TieredCache::new(
                vec![temp_dir.path().to_path_buf()],
                Some("50KB"),
                Some("50KB"),
                Some("200KB"),
                Some("200KB"),
                backend.clone(),
                dlm.meta_client().clone(),
            )
            .unwrap();

            let addr = format!("127.0.0.1:{}", 26300 + i);
            let server = squeezefs::p2p::P2pServer::new(addr.clone(), cache.nvme.clone());
            
            // Spawn P2P server
            tokio::spawn(async move {
                let _ = server.run().await;
            });

            nodes.push(cache);
            temp_dirs.push(temp_dir);
        }

        // Wait for DHT nodes to initialize and set in OnceLock
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Fully connect the nodes
        for i in 0..5 {
            let dht_i = nodes[i].nvme.dht_node.get().expect("DHT Node not initialized");
            for j in 0..5 {
                if i != j {
                    dht_i.add_peer(format!("127.0.0.1:{}", 26300 + j));
                }
            }
        }

        // Allow some time for TCP networks to bind/connect
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });

    let node0_cache = nodes[0].clone();
    let node4_cache = nodes[4].clone();
    let val_data = vec![8u8; 128 * 1024]; // 128KB value
    let val = bytes::Bytes::from(val_data.clone());
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // 1. Benchmark DHT Remote P2P Get
    group.throughput(Throughput::Bytes(128 * 1024));
    group.bench_function("DHT Remote P2P Get", |b| {
        let counter = counter.clone();
        let node0_cache = node0_cache.clone();
        let node4_cache = node4_cache.clone();
        let val = val.clone();

        b.iter_batched(
            || {
                let id = counter.fetch_add(1, Ordering::Relaxed);
                let block_key = format!("scale_bench_block_{}", id);
                rt.block_on(async {
                    // Cache the block on Node 4
                    node4_cache.nvme.cache_read_block(&block_key, &val).unwrap();

                    // Poll Node 0 DHT until provider registration is found (propagated from Node 4)
                    let dht0 = node0_cache.nvme.dht_node.get().unwrap();
                    let key_hash = xxhash_rust::xxh3::xxh3_64(block_key.as_bytes());
                    let mut found = false;
                    for _ in 0..500 {
                        if let Ok(Some(addr)) = dht0.find_provider(key_hash).await {
                            if addr == "127.0.0.1:26304" {
                                found = true;
                                break;
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    assert!(found, "DHT provider registration failed to propagate to node0 in time!");
                });
                block_key
            },
            |block_key| {
                rt.block_on(async {
                    // GET from Node 0. This misses locally and queries DHT to find Node 4, then downloads the block from Node 4.
                    let dht0 = node0_cache.nvme.dht_node.get().unwrap();
                    let client = squeezefs::p2p::P2pClient::new();
                    let res = client.download_block_from_peer(dht0, &block_key).await.unwrap();
                    assert_eq!(res.len(), 128 * 1024);
                });
            },
            BatchSize::PerIteration,
        );
    });

    // 2. Benchmark DHT Lookup Rate
    group.throughput(Throughput::Elements(1));
    let dht0 = node0_cache.nvme.dht_node.get().unwrap().clone();
    let static_key_hash = xxhash_rust::xxh3::xxh3_64(b"scale_bench_block_0");
    group.bench_function("DHT Lookup Provider", |b| {
        let dht = dht0.clone();
        b.to_async(&rt).iter(|| {
            let dht_clone = dht.clone();
            async move {
                let res = dht_clone.find_provider(static_key_hash).await.unwrap();
                criterion::black_box(res);
            }
        });
    });

    // 3. Benchmark P2P Direct Fetch Value
    group.throughput(Throughput::Bytes(128 * 1024));
    let node4_addr = "127.0.0.1:26304".to_string();
    let direct_key = bytes::Bytes::from("scale_bench_block_0");
    group.bench_function("P2P Direct Fetch Remote Value", |b| {
        let dht = dht0.clone();
        let addr = node4_addr.clone();
        let key = direct_key.clone();
        b.to_async(&rt).iter(|| {
            let dht_clone = dht.clone();
            let addr_clone = addr.clone();
            let key_clone = key.clone();
            async move {
                let res = dht_clone.fetch_remote_value(&addr_clone, key_clone).await.unwrap();
                criterion::black_box(res);
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
    targets =
        bench_squeezefs_routing,
        bench_squeezefs_posix_locks,
        bench_squeezefs_metadata_ops,
        bench_squeezefs_data_io,
        bench_nvme_combined,
        bench_dlm_centralized_heartbeat,
        bench_crypto_compress,
        bench_squeezefs_dht_and_p2p_at_scale
}
criterion_main!(benches);
