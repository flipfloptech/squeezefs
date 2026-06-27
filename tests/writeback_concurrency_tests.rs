#![allow(unused_variables, clippy::clone_on_copy)]

use fuse3::raw::Filesystem;
use fuse3::raw::Request;
use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

fn make_staging_dir(test_name: &str) -> PathBuf {
    let path = std::env::current_dir()
        .unwrap()
        .join("target")
        .join("test-staging")
        .join(format!("{}-{}", test_name, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

async fn setup_fs(test_name: &str) -> Option<(SqueezefsFilesystem, RustFsClient, PathBuf)> {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).ok()?;

    let mut con = redis::Client::open(redis_url.clone())
        .ok()?
        .get_multiplexed_tokio_connection()
        .await
        .ok()?;

    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());

    let backend = RustFsClient::new().await;
    let staging_dir = make_staging_dir(test_name);
    let cache = TieredCache::new(
        vec![staging_dir.clone()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .ok()?;

    let router = DataRouter::new(dlm.clone(), backend.clone(), cache);
    router
        .block_size
        .store(1024 * 1024, std::sync::atomic::Ordering::Relaxed);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    // Call init on fs
    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    let _ = fs.init(req).await.ok()?;

    Some((fs, backend, staging_dir))
}

async fn setup_fs_mock(test_name: &str) -> Option<(SqueezefsFilesystem, RustFsClient, PathBuf)> {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).ok()?;

    let mut con = redis::Client::open(redis_url.clone())
        .ok()?
        .get_multiplexed_tokio_connection()
        .await
        .ok()?;

    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());

    let backend = RustFsClient::new_mock();
    let staging_dir = make_staging_dir(test_name);
    let cache = TieredCache::new(
        vec![staging_dir.clone()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .ok()?;

    let router = DataRouter::new(dlm.clone(), backend.clone(), cache);
    router
        .block_size
        .store(1024 * 1024, std::sync::atomic::Ordering::Relaxed);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    // Call init on fs
    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    let _ = fs.init(req).await.ok()?;

    Some((fs, backend, staging_dir))
}

#[tokio::test]
async fn test_concurrent_writeback_correctness() {
    let (fs, _backend, _staging_dir) = match setup_fs("writeback-concurrency").await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let req = Request {
        unique: 10,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    let reply_created = fs
        .create(
            req,
            1,
            OsStr::new("concurrency_writeback_test.bin"),
            0o644,
            0,
        )
        .await
        .expect("Create file should succeed");
    let ino = reply_created.attr.ino;

    // Set file type to striped
    let redis_url = get_redis_url();
    let mut con = redis::Client::open(redis_url)
        .unwrap()
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let meta_key = format!("metadata:inode_{}", ino);
    let _: () = redis::cmd("HSET")
        .arg(&meta_key)
        .arg("type")
        .arg("striped")
        .query_async(&mut con)
        .await
        .unwrap();

    let fs_arc = std::sync::Arc::new(fs);
    let num_tasks = 10;
    let write_size = 50 * 1024; // 50KB per task
    let mut handles = Vec::new();

    // Spawn concurrent tasks writing to different non-overlapping segments of the SAME block (block 0)
    for i in 0..num_tasks {
        let fs_clone = fs_arc.clone();
        let offset = i * write_size;
        let data = vec![i as u8; write_size as usize];
        let req_clone = Request {
            unique: 100 + i,
            uid: 1000,
            gid: 1000,
            pid: 1234 + i as u32,
        };
        handles.push(tokio::spawn(async move {
            fs_clone.write(req_clone, ino, 0, offset, &data, 0, 0).await
        }));
    }

    for h in handles {
        let res = h.await.unwrap();
        assert!(res.is_ok(), "Concurrent write failed");
    }

    // Force flush
    fs_arc
        .flush(req, ino, 0, 0)
        .await
        .expect("Flush should succeed");

    // Read back and verify all data is intact and correct
    let total_size = num_tasks * write_size;
    let read_reply = fs_arc
        .read(req, ino, 0, 0, total_size as u32)
        .await
        .expect("Read failed");

    let read_data = read_reply.data.as_ref();
    assert_eq!(read_data.len(), total_size as usize);

    for i in 0..num_tasks {
        let start = (i * write_size) as usize;
        let end = start + write_size as usize;
        let segment = &read_data[start..end];
        let expected = vec![i as u8; write_size as usize];
        assert_eq!(segment, expected, "Data mismatch at task segment {}", i);
    }
}

#[tokio::test]
async fn test_delayed_deletion_under_concurrent_writes() {
    let (fs, _backend, _staging_dir) = match setup_fs("delayed-deletion").await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let req = Request {
        unique: 20,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    let reply_created = fs
        .create(req, 1, OsStr::new("delayed_deletion_test.bin"), 0o644, 0)
        .await
        .expect("Create file should succeed");
    let ino = reply_created.attr.ino;

    // Set file type to striped
    let redis_url = get_redis_url();
    let mut con = redis::Client::open(redis_url)
        .unwrap()
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let meta_key = format!("metadata:inode_{}", ino);
    let _: () = redis::cmd("HSET")
        .arg(&meta_key)
        .arg("type")
        .arg("striped")
        .query_async(&mut con)
        .await
        .unwrap();

    let fs_arc = std::sync::Arc::new(fs);

    // Write initial data to block 0
    let data = vec![1; 100];
    fs_arc.write(req, ino, 0, 0, &data, 0, 0).await.unwrap();

    let cache_key = format!("active_block:inode_{}:block_0", ino);
    assert!(
        fs_arc.router.cache.nvme.read_staged(&cache_key).is_some(),
        "Staging block should exist after write"
    );

    // Spawn a flush concurrently, then perform another write while flush is in progress
    let fs_clone = fs_arc.clone();
    let req_clone = req.clone();
    let flush_handle = tokio::spawn(async move { fs_clone.flush(req_clone, ino, 0, 0).await });

    // Wait a brief moment to allow flush to read the file and start the S3 put (releasing inode lock)
    tokio::time::sleep(Duration::from_millis(5)).await;

    // Write new data to block 0 to update file metadata/mtime
    let new_data = vec![2; 200];
    fs_arc.write(req, ino, 0, 0, &new_data, 0, 0).await.unwrap();

    // Wait for flush to complete
    let flush_res = flush_handle.await.unwrap();
    assert!(flush_res.is_ok(), "Flush should succeed");

    // Because mtime changed during flush, the staging block file should NOT be deleted!
    assert!(
        fs_arc.router.cache.nvme.read_staged(&cache_key).is_some(),
        "Staging block should still exist due to concurrent write mtime change"
    );

    // Now trigger flush again to flush the new changes, which should delete the block file
    fs_arc
        .flush(req, ino, 0, 0)
        .await
        .expect("Second flush should succeed");

    fs_arc
        .release(req, ino, 0, 0, 0, false)
        .await
        .expect("Release should succeed");

    // Staging file should now be deleted because no other write changed mtime
    assert!(
        fs_arc.router.cache.nvme.read_staged(&cache_key).is_none(),
        "Staging block should be deleted after final flush"
    );
}

#[tokio::test]
async fn test_rmw_cache_ingestion() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (fs, backend, _staging_dir) = match setup_fs_mock("rmw-ingestion").await {
        Some(res) => res,
        None => {
            println!("Skipping test: setup failed");
            return;
        }
    };

    let req = Request {
        unique: 30,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    let reply_created = fs
        .create(req, 1, OsStr::new("rmw_ingestion_test.bin"), 0o644, 0)
        .await
        .expect("Create file should succeed");
    let ino = reply_created.attr.ino;

    // Set file type to striped
    let redis_url = get_redis_url();
    let mut con = redis::Client::open(redis_url)
        .unwrap()
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let meta_key = format!("metadata:inode_{}", ino);
    let _: () = redis::cmd("HSET")
        .arg(&meta_key)
        .arg("type")
        .arg("striped")
        .query_async(&mut con)
        .await
        .unwrap();

    let fs_arc = std::sync::Arc::new(fs);

    // 1. Write initial data to block 0
    let data = vec![5; 100];
    fs_arc.write(req, ino, 0, 0, &data, 0, 0).await.unwrap();

    // Print keys before flush
    let keys_before: Vec<String> = fs_arc
        .router
        .cache
        .nvme
        .staging_nvme_cache
        .list_keys()
        .into_iter()
        .map(|k| String::from_utf8(k.to_vec()).unwrap_or_default())
        .collect();
    println!("NVMe keys before flush: {:?}", keys_before);

    // 2. Flush to S3 and release (this registers it in S3 and deletes the staging block)
    fs_arc
        .flush(req, ino, 0, 0)
        .await
        .expect("Flush should succeed");
    fs_arc
        .release(req, ino, 0, 0, 0, false)
        .await
        .expect("Release should succeed");

    // Print keys after flush
    let keys_after: Vec<String> = fs_arc
        .router
        .cache
        .nvme
        .staging_nvme_cache
        .list_keys()
        .into_iter()
        .map(|k| String::from_utf8(k.to_vec()).unwrap_or_default())
        .collect();
    println!("NVMe keys after flush: {:?}", keys_after);

    // Get block key from Redis
    let block_map_id: Option<String> = con.hget(&meta_key, "block_map_id").await.unwrap();
    println!("block_map_id: {:?}", block_map_id);
    let block_map_key = format!("block_map:{}", block_map_id.as_deref().unwrap_or(""));
    let stored_block_key: Option<String> = con.hget(&block_map_key, "0").await.unwrap();
    println!("stored_block_key: {:?}", stored_block_key);
    let stored_block_key = stored_block_key.expect("stored_block_key should be set");

    // 3. Clear memory cache and NVMe cache
    fs_arc.router.cache.read_lru.clear();

    let safe_name = stored_block_key.replace(['/', ':'], "_");
    let target_dir = fs_arc.router.cache.nvme.staging_dirs()[0].join("cache");
    let block_path = target_dir.join(format!("block_{}.block", safe_name));
    if block_path.exists() {
        let _ = std::fs::remove_file(block_path);
    }
    assert!(
        fs_arc
            .router
            .cache
            .nvme
            .get_cached_read_block(&stored_block_key)
            .is_none(),
        "Read block should be removed from NVMe cache before RMW"
    );

    // 4. Perform a partial write/RMW (which requires downloading block 0 from S3)
    let partial_data = vec![9; 50];
    fs_arc
        .write(req, ino, 0, 10, &partial_data, 0, 0)
        .await
        .unwrap();

    // Give background cache_read_block task a moment to execute
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // 5. Verify the block is now cached in the NVMe read cache
    let cached_res = fs_arc
        .router
        .cache
        .nvme
        .get_cached_read_block(&stored_block_key);
    assert!(
        cached_res.is_some(),
        "Block should be cached in NVMe read cache after RMW download"
    );
}

#[tokio::test]
async fn test_full_block_overwrite_skips_backend_read() {
    let (fs, backend, _staging_dir) = match setup_fs_mock("full-block-overwrite").await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    fs.router
        .block_size
        .store(64 * 1024, std::sync::atomic::Ordering::Relaxed);
    let block_size = fs
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed);

    let req = Request {
        unique: 40,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    let reply_created = fs
        .create(
            req,
            1,
            OsStr::new("full_block_overwrite_test.bin"),
            0o644,
            0,
        )
        .await
        .expect("Create file should succeed");
    let ino = reply_created.attr.ino;

    let old_block_key = "blocks/existing/full_block_overwrite";
    let old_block_data = bytes::Bytes::from(vec![7u8; block_size as usize]);
    backend
        .put_object(old_block_key, old_block_data, 1)
        .await
        .expect("Seeding old block should succeed");
    let stored_block_key = format!("backend_0:{}", old_block_key);

    let redis_url = get_redis_url();
    let mut con = redis::Client::open(redis_url)
        .unwrap()
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();

    let attr_key = format!("squeezefs:attr:{}", ino);
    let meta_key = format!("metadata:inode_{}", ino);
    let block_map_id = format!("full-overwrite-map-{}", ino);
    let block_map_key = format!("block_map:{}", block_map_id);

    let _: () = redis::pipe()
        .hset(&attr_key, "size", block_size)
        .hset(&meta_key, "type", "striped")
        .hset(&meta_key, "size", block_size)
        .hset(&meta_key, "block_map_id", &block_map_id)
        .hset(&meta_key, "num_blocks", 1u32)
        .hset(&block_map_key, "0", &stored_block_key)
        .query_async(&mut con)
        .await
        .unwrap();

    fs.attr_cache.remove(&ino);
    fs.router
        .metadata_cache
        .invalidate(&format!("inode_{}", ino));
    fs.router.block_map_cache.invalidate(&(block_map_id, 0));

    let replacement = vec![9u8; block_size as usize];
    let gets_before = backend.mock_get_count();

    fs.write(req, ino, 0, 0, &replacement, 0, 0)
        .await
        .expect("Full block overwrite should succeed");

    assert_eq!(
        backend.mock_get_count(),
        gets_before,
        "aligned full-block overwrite should not fetch the previous block from the backend"
    );
}

#[tokio::test]
async fn test_background_writeback_flushes_multi_block_inode_without_explicit_flush() {
    let (fs, backend, _staging_dir) = match setup_fs_mock("background-writeback").await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    fs.router
        .block_size
        .store(64 * 1024, std::sync::atomic::Ordering::Relaxed);
    let block_size = fs
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed);

    let req = Request {
        unique: 50,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    let reply_created = fs
        .create(
            req,
            1,
            OsStr::new("background_writeback_test.bin"),
            0o644,
            0,
        )
        .await
        .expect("Create file should succeed");
    let ino = reply_created.attr.ino;

    let redis_url = get_redis_url();
    let mut con = redis::Client::open(redis_url)
        .unwrap()
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let meta_key = format!("metadata:inode_{}", ino);
    let _: () = redis::cmd("HSET")
        .arg(&meta_key)
        .arg("type")
        .arg("striped")
        .query_async(&mut con)
        .await
        .unwrap();

    let first_block = vec![0x11u8; block_size as usize];
    let second_block = vec![0x22u8; block_size as usize];

    fs.write(req, ino, 0, 0, &first_block, 0, 0)
        .await
        .expect("First block write should succeed");
    fs.write(req, ino, 0, block_size, &second_block, 0, 0)
        .await
        .expect("Second block write should succeed");

    let active_prefix = format!("active_block:inode_{}:", ino);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            // Check that background writeback worker has successfully uploaded the 2 parts
            let parts_key = format!("squeezefs:multipart_parts:{}", ino);
            let parts_count = con.hlen::<_, usize>(&parts_key).await.unwrap_or(0);
            if parts_count >= 2 {
                break;
            }

            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background writeback should flush both striped blocks without an explicit flush");

    // Complete the multipart upload
    fs.release(req, ino, 0, 0, 0, false)
        .await
        .expect("Release should succeed");

    let block_map_id = con
        .hget::<_, _, Option<String>>(&meta_key, "block_map_id")
        .await
        .unwrap_or(None);
    let block_count = if let Some(block_map_id) = block_map_id.as_deref() {
        let block_map_key = format!("block_map:{}", block_map_id);
        con.hlen::<_, usize>(&block_map_key).await.unwrap_or(0)
    } else {
        0
    };
    let remaining_active_blocks = fs
        .router
        .cache
        .nvme
        .list_staged_files()
        .into_iter()
        .filter(|key| key.starts_with(&active_prefix))
        .count();

    assert!(block_count >= 2);
    assert_eq!(remaining_active_blocks, 0);

    assert_eq!(
        backend.mock_get_count(),
        0,
        "full-block sequential writes should not require backend reads before background flush"
    );
}

#[tokio::test]
async fn test_fsync_deadlock_prevention() {
    let (fs, _backend, _staging_dir) = match setup_fs_mock("fsync-deadlock").await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let req = Request {
        unique: 101,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    let reply_created = fs
        .create(req, 1, OsStr::new("fsync_deadlock_test.bin"), 0o644, 0)
        .await
        .expect("Create file should succeed");
    let ino = reply_created.attr.ino;

    // Set file type to striped in Redis to route through write_file_staged/active blocks
    let redis_url = get_redis_url();
    let mut con = redis::Client::open(redis_url)
        .unwrap()
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let meta_key = format!("metadata:inode_{}", ino);
    let _: () = redis::cmd("HSET")
        .arg(&meta_key)
        .arg("type")
        .arg("striped")
        .query_async(&mut con)
        .await
        .unwrap();

    // Write a block to populate active staging blocks
    let data = vec![0xaa; 1024];
    fs.write(req, ino, 0, 0, &data, 0, 0)
        .await
        .expect("Write should succeed");

    // Get block lock entry
    let block_lock = squeezefs::fuse_client::BLOCK_FLUSH_LOCKS
        .entry((ino, 0))
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .value()
        .clone();

    // Lock block_lock to simulate background task holding it during S3 upload
    let lock_guard = block_lock.clone().lock_owned().await;

    // Spawn a simulated background task B that tries to acquire InodeLock,
    // and drops lock_guard once it gets the lock
    let fs_arc = std::sync::Arc::new(fs);
    let inode_lock = fs_arc.get_inode_lock(ino);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // B tries to acquire InodeLock (which is held by fsync)
        let _write_guard = inode_lock.write().await;
        // Reached! Release the block lock
        drop(lock_guard);
    });

    // Call fsync, which should complete successfully within a short timeout
    let fs_clone = fs_arc.clone();
    let fsync_res = tokio::time::timeout(Duration::from_secs(3), async move {
        fs_clone.fsync(req, ino, 0, false).await
    })
    .await;

    assert!(fsync_res.is_ok(), "fsync timed out due to deadlock!");
    fsync_res.unwrap().expect("fsync should succeed");
}
