use fuse3::raw::Filesystem;
use fuse3::raw::Request;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::time::Duration;
use tempfile::tempdir;
use redis::AsyncCommands;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn setup_fs() -> Option<(SqueezefsFilesystem, tempfile::TempDir)> {
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
    let temp_dir = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_dir.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .ok()?;

    let router = DataRouter::new(dlm.clone(), backend, cache);
    router.block_size.store(1024 * 1024, std::sync::atomic::Ordering::Relaxed);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    // Call init on fs
    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    let _ = fs.init(req).await.ok()?;

    Some((fs, temp_dir))
}

#[tokio::test]
async fn test_concurrent_writeback_correctness() {
    let (fs, _temp_dir) = match setup_fs().await {
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
        .create(req, 1, OsStr::new("concurrency_writeback_test.bin"), 0o644, 0)
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
            fs_clone
                .write(req_clone, ino, 0, offset, &data, 0, 0)
                .await
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
    let (fs, _temp_dir) = match setup_fs().await {
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

    let staging_dir = fs_arc.router.cache.nvme.staging_dirs().first().unwrap().clone();
    let block_path = staging_dir
        .join("active_writes")
        .join(format!("inode_{}", ino))
        .join("block_0");
    assert!(block_path.exists(), "Staging block file should exist after write");

    // Spawn a flush concurrently, then perform another write while flush is in progress
    let fs_clone = fs_arc.clone();
    let req_clone = req.clone();
    let flush_handle = tokio::spawn(async move {
        fs_clone.flush(req_clone, ino, 0, 0).await
    });

    // Wait a brief moment to allow flush to read the file and start the S3 put (releasing inode lock)
    tokio::time::sleep(Duration::from_millis(5)).await;

    // Write new data to block 0 to update file metadata/mtime
    let new_data = vec![2; 200];
    fs_arc.write(req, ino, 0, 0, &new_data, 0, 0).await.unwrap();

    // Wait for flush to complete
    let flush_res = flush_handle.await.unwrap();
    assert!(flush_res.is_ok(), "Flush should succeed");

    // Because mtime changed during flush, the staging block file should NOT be deleted!
    assert!(block_path.exists(), "Staging block file should still exist due to concurrent write mtime change");

    // Now trigger flush again to flush the new changes, which should delete the block file
    fs_arc.flush(req, ino, 0, 0).await.expect("Second flush should succeed");

    // Staging file should now be deleted because no other write changed mtime
    assert!(!block_path.exists(), "Staging block file should be deleted after final flush");
}

#[tokio::test]
async fn test_rmw_cache_ingestion() {
    let (fs, _temp_dir) = match setup_fs().await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
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

    // 2. Flush to S3 (this registers it in S3 and deletes the staging block)
    fs_arc.flush(req, ino, 0, 0).await.expect("Flush should succeed");

    // Get block key from Redis
    let block_map_id: String = con.hget(&meta_key, "block_map_id").await.unwrap();
    let block_map_key = format!("block_map:{}", block_map_id);
    let stored_block_key: String = con.hget(&block_map_key, "0").await.unwrap();

    // 3. Clear memory cache and NVMe cache
    fs_arc.router.cache.read_lru.clear();
    
    let safe_name = stored_block_key.replace(['/', ':'], "_");
    let target_dir = fs_arc.router.cache.nvme.staging_dirs()[0].join("cache");
    let block_path = target_dir.join(format!("block_{}.block", safe_name));
    if block_path.exists() {
        let _ = std::fs::remove_file(block_path);
    }
    assert!(
        fs_arc.router.cache.nvme.get_cached_read_block(&stored_block_key).is_none(),
        "Read block should be removed from NVMe cache before RMW"
    );

    // 4. Perform a partial write/RMW (which requires downloading block 0 from S3)
    let partial_data = vec![9; 50];
    fs_arc.write(req, ino, 0, 10, &partial_data, 0, 0).await.unwrap();

    // 5. Verify the block is now cached in the NVMe read cache
    let cached_res = fs_arc.router.cache.nvme.get_cached_read_block(&stored_block_key);
    assert!(
        cached_res.is_some(),
        "Block should be cached in NVMe read cache after RMW download"
    );
}
