use fuse3::raw::Filesystem;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::lru::LruCache;
use squeezefs::cache::nvme::NvmeStaging;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn test_lru_eviction() {
    // Construct cache with max capacity 1000 bytes
    let cache = LruCache::with_capacity(1000);

    // Insert 40 blocks of 100 bytes (total 4000 bytes > 1000 bytes)
    for i in 0..40 {
        cache.put(
            &format!("key{}", i),
            std::sync::Arc::new(vec![i as u8; 100]),
        );
    }
    cache.run_pending_tasks();

    // Verify cache size is within limits (moka evicts asynchronously but run_pending_tasks makes it synchronous)
    assert!(
        cache.current_bytes() <= 1000,
        "Cache size {} exceeded capacity 1000",
        cache.current_bytes()
    );

    // Verify that at least some keys were evicted
    let mut present = 0;
    for i in 0..40 {
        if cache.get(&format!("key{}", i)).is_some() {
            present += 1;
        }
    }
    assert!(present < 40, "No keys were evicted");
    assert!(present > 0, "All keys were evicted");
}

#[tokio::test]
async fn test_nvme_staging_and_merge() {
    let temp_dir = tempdir().unwrap();
    let mock_backend = RustFsClient::new_mock();

    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    let mut con = redis_client
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let _: () = redis::cmd("DEL")
        .arg("squeezefs:format")
        .query_async(&mut con)
        .await
        .unwrap_or(());

    let nvme = NvmeStaging::new(
        vec![temp_dir.path().to_path_buf()],
        100 * 1024 * 1024, // 100MB write limit
        100 * 1024 * 1024, // 100MB read limit
        mock_backend.clone(),
        squeezefs::dlm::MetaClient::Single(redis_client),
    )
    .expect("Should construct NVMe staging");

    let file_id = "test-file-id-123";
    let data = vec![7; 1000];

    // Stage write (returns immediately)
    nvme.stage_write("file.txt", file_id, &data, 42)
        .await
        .expect("Should stage write successfully");

    // Immediately after write, the staged file MUST be readable locally from NVMe staging
    let staged_data = nvme
        .read_staged(file_id)
        .expect("Should find staged file locally");
    assert_eq!(staged_data, data);

    // Wait for the background worker to flush the batch (timeout is 500ms, let's wait 800ms)
    tokio::time::sleep(Duration::from_millis(800)).await;

    // After flush, the local file should be deleted (cleaned up)
    assert!(nvme.read_staged(file_id).is_none());
}

#[test]
fn test_cache_size_parser() {
    use squeezefs::cache::parse_size_string;

    // Test percentage of system/total
    let size = parse_size_string("50%", 1000).unwrap();
    assert_eq!(size, 500);

    // Test exact sizes
    assert_eq!(parse_size_string("100B", 1000).unwrap(), 100);
    assert_eq!(parse_size_string("10KB", 1000).unwrap(), 10240);
    assert_eq!(parse_size_string("5MB", 1000).unwrap(), 5 * 1024 * 1024);
    assert_eq!(
        parse_size_string("2GB", 1000).unwrap(),
        2 * 1024 * 1024 * 1024
    );

    // Test case insensitivity and spaces
    assert_eq!(parse_size_string("  1.5 gb  ", 1000).unwrap(), 1610612736);

    // Test errors
    assert!(parse_size_string("invalid", 1000).is_err());
}

#[tokio::test]
async fn test_multi_disk_distribution() {
    let dir1 = tempdir().unwrap();
    let dir2 = tempdir().unwrap();
    let dir3 = tempdir().unwrap();

    let staging_dirs = vec![
        dir1.path().to_path_buf(),
        dir2.path().to_path_buf(),
        dir3.path().to_path_buf(),
    ];

    let mock_backend = RustFsClient::new_mock();
    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();

    // Max capacity 100MB for both
    let nvme = NvmeStaging::new(
        staging_dirs.clone(),
        100 * 1024 * 1024,
        100 * 1024 * 1024,
        mock_backend,
        squeezefs::dlm::MetaClient::Single(redis_client),
    )
    .expect("Should construct multi-disk NVMe staging");

    // Let's write 6 files and verify they are distributed
    for i in 0..6 {
        let file_id = format!("file-id-{}", i);
        let data = vec![i as u8; 100];
        nvme.stage_write(&format!("file_{}.txt", i), &file_id, &data, 100 + i as u64)
            .await
            .unwrap();

        // Verify read_staged can read it back successfully
        let read_data = nvme.read_staged(&file_id).unwrap();
        assert_eq!(read_data, data);
    }

    // Verify files were actually placed in the respective directories
    let mut total_files = 0;
    for dir in &staging_dirs {
        let staging_dir = dir.join("staging");
        let entries: Vec<_> = std::fs::read_dir(&staging_dir)
            .unwrap()
            .map(|res| res.unwrap().path())
            .collect();
        for path in entries {
            if path.extension().is_some_and(|ext| ext == "staged")
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|name| name.starts_with("file_"))
            {
                total_files += 1;
            }
        }
    }
    assert_eq!(total_files, 6);
}

#[tokio::test]
async fn test_nvme_cache_separation_limits() {
    let temp_dir = tempdir().unwrap();
    let mock_backend = RustFsClient::new_mock();
    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    let mut con = redis_client
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let _: () = redis::cmd("FLUSHDB").query_async(&mut con).await.unwrap();
    let _: () = redis::cmd("HSET")
        .arg("squeezefs:format")
        .arg("upload_delay")
        .arg("60s")
        .query_async(&mut con)
        .await
        .unwrap();

    // Very small limits (10KB write capacity, 10KB read capacity)
    let nvme = NvmeStaging::new(
        vec![temp_dir.path().to_path_buf()],
        10 * 1024,
        10 * 1024,
        mock_backend,
        squeezefs::dlm::MetaClient::Single(redis_client),
    )
    .expect("Should construct NVMe staging");

    // 1. Stage a write of 6KB
    let data_write = vec![1u8; 6 * 1024];
    nvme.stage_write("test_write.txt", "file-id-write", &data_write, 100)
        .await
        .expect("Should stage 6KB successfully");

    // 2. Cache a read block of 8KB
    let data_read = vec![2u8; 8 * 1024];
    nvme.cache_read_block("blocks/b1", &data_read)
        .expect("Should cache read block");

    // Wait briefly for the async spawn_blocking of read caching to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify both are tracked independently in their respective fields
    assert_eq!(nvme.current_staged_write_bytes(), 8192); // staged uses aligned/padded length
    assert_eq!(nvme.current_read_cache_bytes(), 8 * 1024);

    // 3. Trying to write another 6KB should fail with StorageFull because 6KB + 6KB > 10KB
    let data_write_2 = vec![1u8; 6 * 1024];
    let err = nvme
        .stage_write("test_write_2.txt", "file-id-write-2", &data_write_2, 101)
        .await;
    assert!(err.is_err());
    let err_unwrapped = err.err().unwrap();
    match err_unwrapped {
        squeezefs::error::SqueezefsError::Io(ref e) => {
            assert_eq!(e.kind(), std::io::ErrorKind::StorageFull);
        }
        _ => panic!("Expected std::io::ErrorKind::StorageFull error"),
    }
}

#[tokio::test]
async fn test_nvme_read_cache_eviction() {
    let temp_dir = tempdir().unwrap();
    let mock_backend = RustFsClient::new_mock();
    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();

    // 20KB write capacity, 5KB read capacity
    let nvme = NvmeStaging::new(
        vec![temp_dir.path().to_path_buf()],
        20 * 1024,
        5 * 1024,
        mock_backend,
        squeezefs::dlm::MetaClient::Single(redis_client),
    )
    .expect("Should construct NVMe staging");

    // Stage write of 8KB (consumes write capacity, does not touch read capacity)
    let data_write = vec![1u8; 8 * 1024];
    nvme.stage_write("staged_write.txt", "staged-id", &data_write, 100)
        .await
        .expect("Should stage write successfully");

    // Cache read block 1 (3KB)
    let b1 = vec![2u8; 3 * 1024];
    nvme.cache_read_block("blocks/b1", &b1).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cache read block 2 (3KB) -> total read cache is now 6KB > 5KB capacity.
    // This should trigger eviction of block 1 to keep read cache under 5KB limit.
    let b2 = vec![3u8; 3 * 1024];
    nvme.cache_read_block("blocks/b2", &b2).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify block 1 was evicted, but block 2 is present
    assert!(
        nvme.read_cached_block("blocks/b1").is_none(),
        "b1 should have been evicted"
    );
    assert!(
        nvme.read_cached_block("blocks/b2").is_some(),
        "b2 should be present"
    );

    // Verify staged write was NOT evicted
    assert!(
        nvme.read_staged("staged-id").is_some(),
        "Staged write must never be evicted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_virtual_stats_file() {
    let _con = match clean_db_for_stats().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = "redis://127.0.0.1:6379/";
    let dlm = squeezefs::dlm::DlmClient::new(redis_url).unwrap();
    let backend = squeezefs::backend::RustFsClient::new().await;
    let temp_dir = tempdir().unwrap();
    let cache = squeezefs::cache::TieredCache::new(
        vec![temp_dir.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router = squeezefs::routing::DataRouter::new(dlm.clone(), backend, cache);
    let fs = squeezefs::fuse_client::SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = fuse3::raw::Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // 1. Lookup ".stats" in root directory (parent = 1)
    let reply_lookup = fs
        .lookup(req, 1, std::ffi::OsStr::new(".stats"))
        .await
        .unwrap();
    assert_eq!(reply_lookup.attr.ino, 0xffff_ffff_ffff_fffd);
    assert_eq!(
        reply_lookup.attr.kind,
        fuse3::raw::prelude::FileType::RegularFile
    );

    // 2. Getattr on stats inode
    let reply_attr = fs
        .getattr(req, 0xffff_ffff_ffff_fffd, None, 0)
        .await
        .unwrap();
    assert_eq!(reply_attr.attr.ino, 0xffff_ffff_ffff_fffd);
    assert!(reply_attr.attr.size > 0);

    // 3. Read stats data
    let reply_read = fs
        .read(req, 0xffff_ffff_ffff_fffd, 0, 0, 8192)
        .await
        .unwrap();
    let stats_str = String::from_utf8(reply_read.data.to_vec()).unwrap();

    // Parse stats JSON
    let stats_val: serde_json::Value = serde_json::from_str(&stats_str).unwrap();
    assert!(stats_val.get("read_lru_keys").is_some());
    assert!(stats_val.get("write_lru_keys").is_some());
    assert!(stats_val.get("metrics").is_some());
    assert!(stats_val.get("cache_capacities").is_some());
    assert!(stats_val.get("internal_caches").is_some());
}

async fn clean_db_for_stats() -> Option<redis::aio::MultiplexedConnection> {
    let client = redis::Client::open("redis://127.0.0.1:6379/").ok()?;
    let mut con = client.get_multiplexed_async_connection().await.ok()?;
    let _: () = redis::cmd("FLUSHDB").query_async(&mut con).await.ok()?;
    Some(con)
}

#[tokio::test]
async fn test_atomic_cache_write_concurrency() {
    let temp_dir = tempdir().unwrap();
    let mock_backend = RustFsClient::new_mock();
    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    let nvme = NvmeStaging::new(
        vec![temp_dir.path().to_path_buf()],
        10 * 1024 * 1024,
        10 * 1024 * 1024,
        mock_backend,
        squeezefs::dlm::MetaClient::Single(redis_client),
    )
    .unwrap();

    let block_key = "blocks/test_atomic_concurrency";
    let block_data = vec![0xAB; 64 * 1024]; // 64KB block

    // Spawn readers and writers to run concurrently
    let nvme_clone = nvme.clone();
    let block_data_clone = block_data.clone();
    let writer_task = tokio::spawn(async move {
        for _ in 0..100 {
            nvme_clone
                .cache_read_block(block_key, &block_data_clone)
                .unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let nvme_clone2 = nvme.clone();
    let block_data_clone2 = block_data.clone();
    let reader_task = tokio::spawn(async move {
        for _ in 0..500 {
            if let Some(cached) = nvme_clone2.read_cached_block(block_key) {
                // If it exists, it MUST be fully written (i.e. length must be exactly 64KB and content matching)
                assert_eq!(cached.len(), block_data_clone2.len());
                assert_eq!(cached, block_data_clone2);
            }
            tokio::time::sleep(Duration::from_millis(0)).await;
        }
    });

    let _ = tokio::join!(writer_task, reader_task);
}

#[test]
fn test_parse_duration() {
    use squeezefs::cache::parse_duration;

    assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
    assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
    assert_eq!(parse_duration("1000").unwrap(), Duration::from_millis(1000));
    assert_eq!(
        parse_duration("  250Ms  ").unwrap(),
        Duration::from_millis(250)
    );
    assert_eq!(parse_duration("3S").unwrap(), Duration::from_secs(3));

    assert!(parse_duration("invalid").is_err());
    assert!(parse_duration("ms").is_err());
    assert!(parse_duration("s").is_err());
    assert!(parse_duration("100x").is_err());
}

#[tokio::test]
async fn test_dynamic_upload_delay() {
    let temp_dir = tempdir().unwrap();
    let mock_backend = RustFsClient::new_mock();
    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();

    let mut con = redis_client
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let _: () = redis::cmd("HSET")
        .arg("squeezefs:format")
        .arg("upload_delay")
        .arg("1500ms")
        .query_async(&mut con)
        .await
        .unwrap();

    let nvme = NvmeStaging::new(
        vec![temp_dir.path().to_path_buf()],
        100 * 1024 * 1024,
        100 * 1024 * 1024,
        mock_backend,
        squeezefs::dlm::MetaClient::Single(redis_client),
    )
    .unwrap();

    // Sleep 6 seconds so the worker loop query-cache expires and it queries Garnet
    // and applies the 1500ms timeout
    tokio::time::sleep(Duration::from_secs(6)).await;

    let file_id = "test-delay-file-id";
    let data = vec![9; 100];
    nvme.stage_write("test_delay.txt", file_id, &data, 100)
        .await
        .unwrap();

    // Check after 800ms. Since the delay is 1500ms, it should STILL be in NVMe staging!
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        nvme.read_staged(file_id).is_some(),
        "Staged file should still be present before timeout"
    );

    // Check after another 1200ms (total 2000ms > 1500ms). It should be flushed/deleted.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        nvme.read_staged(file_id).is_none(),
        "Staged file should be flushed and deleted after timeout"
    );

    // Clean up to prevent test pollution
    let _: () = redis::cmd("DEL")
        .arg("squeezefs:format")
        .query_async(&mut con)
        .await
        .unwrap_or(());
}

#[tokio::test]
async fn test_active_writes_pruning_and_deletion() {
    use squeezefs::routing::DataRouter;
    use std::fs::{create_dir_all, read_dir, remove_dir, write};

    let temp_dir = tempfile::tempdir().unwrap();
    let staging_dirs = vec![temp_dir.path().to_path_buf()];

    // 1. Create active writes directory structure
    let active_dir = temp_dir.path().join("active_writes");
    let empty_inode_dir = active_dir.join("inode_111");
    let nonempty_inode_dir = active_dir.join("inode_222");

    create_dir_all(&empty_inode_dir).unwrap();
    create_dir_all(&nonempty_inode_dir).unwrap();

    // Write a dummy block file inside nonempty_inode_dir
    write(nonempty_inode_dir.join("block_0"), b"data").unwrap();

    // Verify both directories exist before pruning
    assert!(empty_inode_dir.exists());
    assert!(nonempty_inode_dir.exists());

    // 2. Perform the same pruning logic as in unmount/destroy
    for dir in &staging_dirs {
        let active_dir_path = dir.join("active_writes");
        if active_dir_path.exists() {
            if let Ok(entries) = read_dir(&active_dir_path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let _ = remove_dir(&path);
                    }
                }
            }
        }
    }

    // 3. Verify empty_inode_dir was deleted, but nonempty_inode_dir still exists
    assert!(
        !empty_inode_dir.exists(),
        "Empty inode directory should be pruned"
    );
    assert!(
        nonempty_inode_dir.exists(),
        "Non-empty inode directory should NOT be pruned"
    );

    // 4. Test Router's delete_file functionality deletes the active writes directory (even if non-empty)
    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    if let Ok(dlm) = squeezefs::dlm::DlmClient::new(&redis_url) {
        if let Ok(client) = redis::Client::open(redis_url.clone()) {
            if let Ok(mut con) = client.get_multiplexed_tokio_connection().await {
                let _: () = redis::cmd("FLUSHALL")
                    .query_async(&mut con)
                    .await
                    .unwrap_or(());

                let _ = squeezefs::fuse_client::format_volume(
                    &redis_url,
                    "test_active_writes_vol",
                    4 * 1024 * 1024,
                    100 * 1024 * 1024 * 1024,
                    0,
                    "none",
                    "none",
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await;

                let backend = RustFsClient::new_mock();
                let cache_dir = tempfile::tempdir().unwrap();
                let cache = squeezefs::cache::TieredCache::new(
                    vec![cache_dir.path().to_path_buf()],
                    None,
                    None,
                    None,
                    None,
                    backend.clone(),
                    dlm.meta_client().clone(),
                )
                .unwrap();

                let router = DataRouter::new(dlm, backend, cache);

                // Create an active write directory inside the router's staging dir
                let router_active_dir = cache_dir.path().join("active_writes").join("inode_333");
                create_dir_all(&router_active_dir).unwrap();
                write(router_active_dir.join("block_0"), b"data").unwrap();
                assert!(router_active_dir.exists());

                // Set up basic metadata in Redis for inode_333 so delete_file works
                let meta_key = "metadata:inode_333";
                let mut conn = router.dlm.get_connection().await.unwrap();
                let _: () = redis::cmd("HSET")
                    .arg(meta_key)
                    .arg("type")
                    .arg("striped")
                    .query_async(&mut conn)
                    .await
                    .unwrap();

                // Call delete_file
                router.delete_file("inode_333", &mut conn).await.unwrap();

                // Verify the active writes directory is completely gone!
                assert!(
                    !router_active_dir.exists(),
                    "delete_file should delete the active_writes directory of the inode"
                );
            }
        }
    }
}

#[tokio::test]
async fn test_concurrent_mounts_cache_sharing() {
    use std::path::Path;
    use std::path::PathBuf;

    let temp_root = tempfile::tempdir().unwrap();
    let root_path = temp_root.path().to_path_buf();

    let fs_name = "testvol_concurrent";

    // Two mock mountpoints
    let mnt1 = PathBuf::from("/mnt/data1");
    let mnt2 = PathBuf::from("/mnt/data2");

    // Helper closure to simulate the main.rs isolated directory mapping
    let resolve_isolated = |mountpoint: &Path| {
        let sanitized_mount = mountpoint
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect::<String>();
        let mut sanitized_mount_clean = String::new();
        let mut last_was_underscore = false;
        for c in sanitized_mount.chars() {
            if c == '_' {
                if !last_was_underscore {
                    sanitized_mount_clean.push(c);
                    last_was_underscore = true;
                }
            } else {
                sanitized_mount_clean.push(c);
                last_was_underscore = false;
            }
        }
        let sanitized_mount_clean = sanitized_mount_clean.trim_matches('_');
        root_path.join(fs_name).join(sanitized_mount_clean)
    };

    let isolated_dir1 = resolve_isolated(&mnt1);
    let isolated_dir2 = resolve_isolated(&mnt2);
    let shared_cache_dir = root_path.join(fs_name).join("cache");

    // Create directories
    std::fs::create_dir_all(&isolated_dir1).unwrap();
    std::fs::create_dir_all(&isolated_dir2).unwrap();
    std::fs::create_dir_all(&shared_cache_dir).unwrap();

    // Create symlinks
    let symlink_path1 = isolated_dir1.join("cache");
    let symlink_path2 = isolated_dir2.join("cache");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("../cache", &symlink_path1).unwrap();
        std::os::unix::fs::symlink("../cache", &symlink_path2).unwrap();
    }

    // Now initialize TieredCache for both isolated directories
    let backend = RustFsClient::new_mock();
    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();

    let _cache1 = squeezefs::cache::TieredCache::new(
        vec![isolated_dir1.clone()],
        None,
        None,
        None,
        None,
        backend.clone(),
        squeezefs::dlm::MetaClient::Single(redis_client.clone()),
    )
    .unwrap();

    let _cache2 = squeezefs::cache::TieredCache::new(
        vec![isolated_dir2.clone()],
        None,
        None,
        None,
        None,
        backend.clone(),
        squeezefs::dlm::MetaClient::Single(redis_client.clone()),
    )
    .unwrap();

    // Verify subdirectories exist
    assert!(isolated_dir1.join("active_writes").exists());
    assert!(isolated_dir1.join("staging").exists());
    assert!(symlink_path1.exists());

    assert!(isolated_dir2.join("active_writes").exists());
    assert!(isolated_dir2.join("staging").exists());
    assert!(symlink_path2.exists());

    // Verify that writing a block to cache1's cache is visible in cache2's cache (via sharing)
    let block_name = "block_abc123.block";
    let block_data = b"hello shared read cache";

    #[cfg(unix)]
    {
        // Path in cache1's cache
        let block_path1 = symlink_path1.join(block_name);
        std::fs::write(&block_path1, block_data).unwrap();

        // Should be readable from cache2's cache
        let block_path2 = symlink_path2.join(block_name);
        assert!(block_path2.exists());
        let read_data = std::fs::read(block_path2).unwrap();
        assert_eq!(read_data, block_data);

        // Verify it was actually written to the shared cache dir
        let block_path_shared = shared_cache_dir.join(block_name);
        assert!(block_path_shared.exists());
    }
}

#[tokio::test]
async fn test_router_cache_bounds() {
    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let dlm = squeezefs::dlm::DlmClient::new(&redis_url).unwrap();
    let backend = squeezefs::backend::RustFsClient::new_mock();
    let temp_dir = tempfile::tempdir().unwrap();
    let cache = squeezefs::cache::TieredCache::new(
        vec![temp_dir.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router = squeezefs::routing::DataRouter::new(dlm, backend, cache);

    let meta = squeezefs::routing::CachedMetadata {
        file_type: "inline".to_string(),
        size: 100,
        block_map_id: None,
        block_prefix: None,
        file_id: None,
        cached_at: std::time::Instant::now(),
        data_key: None,
    };

    router.metadata_cache.insert("test_key".to_string(), meta);
    assert!(router.metadata_cache.get("test_key").is_some());

    let val = (Some("block_key".to_string()), std::time::Instant::now());
    router
        .block_map_cache
        .insert(("map_id".to_string(), 0), val);
    assert!(router
        .block_map_cache
        .get(&("map_id".to_string(), 0))
        .is_some());
}

#[tokio::test]
async fn test_nvme_read_cache_lru_in_memory() {
    let temp_dir = tempdir().unwrap();
    let mock_backend = RustFsClient::new_mock();
    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();

    // 5KB read capacity limit
    let nvme = NvmeStaging::new(
        vec![temp_dir.path().to_path_buf()],
        100 * 1024 * 1024,
        5 * 1024,
        mock_backend,
        squeezefs::dlm::MetaClient::Single(redis_client),
    )
    .expect("Should construct NVMe staging");

    // Cache b1 (2KB)
    let b1 = vec![1u8; 2 * 1024];
    nvme.cache_read_block("blocks/b1", &b1).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cache b2 (2KB)
    let b2 = vec![2u8; 2 * 1024];
    nvme.cache_read_block("blocks/b2", &b2).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Read b1 again to promote it to MRU (most recently used)
    let b1_read = nvme.read_cached_block("blocks/b1");
    assert!(b1_read.is_some());

    // Cache b3 (2KB) -> total capacity is now 6KB > 5KB limit.
    // Since b1 was touched/read, the oldest block (LRU) is b2.
    // Eviction should evict b2.
    let b3 = vec![3u8; 2 * 1024];
    nvme.cache_read_block("blocks/b3", &b3).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify b2 was evicted, but b1 and b3 are still present
    assert!(
        nvme.read_cached_block("blocks/b2").is_none(),
        "b2 should have been evicted as it was the LRU block"
    );
    assert!(
        nvme.read_cached_block("blocks/b1").is_some(),
        "b1 should be present because it was promoted by being read"
    );
    assert!(
        nvme.read_cached_block("blocks/b3").is_some(),
        "b3 should be present as it is the newest block"
    );
}
