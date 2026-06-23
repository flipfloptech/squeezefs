use criterion::{criterion_group, criterion_main, Criterion};
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

    group.bench_function("read_micro_file_1kb", |b| {
        b.to_async(&rt).iter(|| async {
            let _reply = fs.read(req, micro_ino, 0, 0, 1024).await.unwrap();
        });
    });

    group.bench_function("read_small_file_128kb", |b| {
        b.to_async(&rt).iter(|| async {
            let _reply = fs.read(req, small_ino, 0, 0, 128 * 1024).await.unwrap();
        });
    });

    let write_large_counter = Arc::new(AtomicU64::new(0));
    let large_data = vec![8u8; 4 * 1024 * 1024]; // 4MB
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
                nvme_ref.stage_write(&file_path, &file_id, data_ref, 1).await.unwrap();
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
                let lease = dlm_ref.acquire_lock(&path, None, Duration::from_secs(5)).await.unwrap();
                lease.release().await.unwrap();
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
        bench_dlm_centralized_heartbeat
}
criterion_main!(benches);
