use fuse3::raw::{prelude::*, Request};
use redis::AsyncCommands;
use squeezefs::backend::{MultiBackendClient, RustFsClient};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{format_volume, get_volume_status, start_mount, SqueezefsFilesystem};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn clean_db() -> Option<redis::aio::MultiplexedConnection> {
    let redis_url = get_redis_url();
    let mut con = redis::Client::open(redis_url)
        .ok()?
        .get_multiplexed_tokio_connection()
        .await
        .ok()?;
    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());
    Some(con)
}

#[tokio::test]
async fn test_cli_format_and_status() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    format_volume(
        &redis_url,
        "testvolume",
        8 * 1024 * 1024,
        // 8MB block size
        500 * 1024 * 1024 * 1024 * 1024,
        // 500TB capacity
        0,
        // inodes limit
        "none",
        // compression
        "none",
        // encrypt_algo
        None,
        // encrypt_key
        Some("64GB"),
        Some("100GB"),
        Some(&[std::path::PathBuf::from("/tmp/test_staging_format")]),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("Format volume should succeed");

    let status = get_volume_status(&redis_url)
        .await
        .expect("Status query should succeed");

    assert_eq!(status["Setting"]["Name"], "testvolume");
    assert_eq!(status["Setting"]["BlockSize"], 8388608);
    assert_eq!(status["Setting"]["Capacity"], 549755813888000i64);
    assert_eq!(status["Setting"]["MemCacheSize"], "64GB");
    assert_eq!(status["Setting"]["DiskCacheSize"], "100GB");
}

#[tokio::test]
async fn test_mount_auto_format_and_abi_check() {
    let mut con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
    let backend = RustFsClient::new().await;
    let temp_dir = tempdir().unwrap();

    // Auto-Format on Mount Check: verify that if we don't format, it automatically formats on initialization
    let cache = TieredCache::new(
        vec![temp_dir.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    // Triggering fs init should perform auto-format
    let _ = fs
        .init(req)
        .await
        .expect("Auto-format should set up volume on init");

    // Check that format config key was written
    let format_exists: bool = con.exists("squeezefs:format").await.unwrap_or(false);
    assert!(
        format_exists,
        "Auto-format should have created squeezefs:format key"
    );

    // ABI check: if we write a higher version in the format key, subsequent fs creation should fail
    let _: () = con.hset("squeezefs:format", "version", 99).await.unwrap();

    drop(fs);

    // Re-create fs
    let dlm2 = DlmClient::new(&redis_url).unwrap();
    let backend2 = RustFsClient::new().await;
    let temp_dir2 = tempdir().unwrap();
    let cache2 = TieredCache::new(
        vec![temp_dir2.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend2.clone(),
        dlm2.meta_client().clone(),
    )
    .unwrap();
    let router2 = DataRouter::new(dlm2.clone(), backend2, cache2);
    let fs2 = SqueezefsFilesystem::new(router2, dlm2, 1000, 1000);

    let res = fs2.init(req).await;
    assert!(
        res.is_err(),
        "Init should fail because database ABI version (99) is higher than supported"
    );

    // Clean up format version so other tests are not broken
    let _: () = con.hset("squeezefs:format", "version", 1).await.unwrap();
}

#[tokio::test]
async fn test_space_accounting_and_statfs() {
    let mut con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();
    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // Create a file
    let file_name = OsStr::new("space_test.txt");
    let reply_create = fs.create(req, 1, file_name, 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    // Verify initial used bytes is 0
    let initial_used: u64 = con
        .get("squeezefs:used_bytes")
        .await
        .unwrap_or(None)
        .unwrap_or(0);
    assert_eq!(initial_used, 0);

    // Perform a write
    let data = vec![7u8; 1000];
    let _reply_write = fs.write(req, ino, 0, 0, &data, 0, 0).await.unwrap();

    // Wait a brief moment and verify used_bytes is updated
    let post_write_used: u64 = con
        .get("squeezefs:used_bytes")
        .await
        .unwrap_or(None)
        .unwrap_or(0);
    assert_eq!(post_write_used, 1000);

    // Perform statfs check
    let reply_statfs = fs.statfs(req, 1).await.unwrap();
    assert_eq!(reply_statfs.bsize, 4096);
    assert!(reply_statfs.blocks > 0);
    assert_eq!(reply_statfs.blocks - reply_statfs.bfree, 1); // 1000 bytes takes 1 block (4096 bsize)

    // Truncate file (setattr size to 500)
    let set_attr = SetAttr {
        size: Some(500),
        ..Default::default()
    };
    let _ = fs.setattr(req, ino, None, set_attr).await.unwrap();
    let post_trunc_used: u64 = con
        .get("squeezefs:used_bytes")
        .await
        .unwrap_or(None)
        .unwrap_or(0);
    assert_eq!(post_trunc_used, 500);

    // Delete file (unlink)
    fs.unlink(req, 1, file_name).await.unwrap();
    let post_delete_used: u64 = con
        .get("squeezefs:used_bytes")
        .await
        .unwrap_or(None)
        .unwrap_or(0);
    assert_eq!(post_delete_used, 0);
}

#[tokio::test]
async fn test_stale_mount_warning_only() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();
    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    // If we pass a nonexistent path or trigger an ENOTCONN, it should fail
    // We can simulate an ENOTCONN by checking if mounting on a stale directory returns standard error rather than unmounting it.
    // We check if start_mount returns standard IO error for invalid setups.
    let res = start_mount(
        "/nonexistent/mountpoint/path/here",
        fs,
        1000,
        1000,
        false,
        true,
        None,
    )
    .await;
    assert!(res.is_err(), "Mount should fail on invalid path");
}

#[tokio::test]
async fn test_three_tiered_writeback_and_lease_cache() {
    let mut con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();

    let router = DataRouter::new(dlm.clone(), backend.clone(), cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // Create a large file
    let file_name = OsStr::new("large_writeback.bin");
    let reply_create = fs.create(req, 1, file_name, 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    // Transition file to striped first
    let initial_data = vec![1u8; 5 * 1024 * 1024]; // 5MB (> 4MB block size)
    fs.write(req, ino, 0, 0, &initial_data, 0, 0).await.unwrap();

    // Perform sequential writes that modify the file
    let write_data_1 = vec![2u8; 1024 * 1024]; // 1MB
    let write_data_2 = vec![3u8; 1024 * 1024]; // 1MB

    // Sequential writes: these should buffer in RAM/NVMe staging, avoiding immediate CoW S3 puts
    fs.write(req, ino, 0, 1024 * 1024, &write_data_1, 0, 0)
        .await
        .unwrap();
    fs.write(req, ino, 0, 2 * 1024 * 1024, &write_data_2, 0, 0)
        .await
        .unwrap();

    // Verify local active write block 0 exists in NVMe cache
    let cache_key = format!("active_block:inode_{}:block_0", ino);
    assert!(
        fs.router.cache.nvme.read_staged(&cache_key).is_some(),
        "Local dirty block 0 must be present in NVMe cache"
    );

    // Call FUSE flush (mimicking close)
    fs.flush(req, ino, 0, 0)
        .await
        .expect("Flush should succeed");

    // Call FUSE release
    fs.release(req, ino, 0, 0, 0, false)
        .await
        .expect("Release should succeed");

    // Verify local active write block 0 has been cleaned up/deleted post-flush
    assert!(
        fs.router.cache.nvme.read_staged(&cache_key).is_none(),
        "Local active write block 0 must be cleaned up post-flush"
    );

    // Retrieve block map id from redis to verify block keys and contents in S3
    let block_map_id: Option<String> = con
        .hget(format!("metadata:inode_{}", ino), "block_map_id")
        .await
        .unwrap();
    assert!(block_map_id.is_some());
    let map_key = format!("block_map:{}", block_map_id.unwrap());
    let block_keys: Vec<String> = con.hvals(&map_key).await.unwrap();
    assert!(!block_keys.is_empty());
    for bk in &block_keys {
        let (be_id, real_key) = squeezefs::backend::parse_backend_and_key(bk);
        assert_eq!(be_id, "backend_0");
        let data = backend
            .get_object(&real_key)
            .await
            .expect("Block must exist in S3");
        assert!(!data.is_empty());
    }
}

#[tokio::test]
async fn test_parallel_reads() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();

    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // Create a file and write 12MB (spans 3 blocks of 4MB)
    let file_name = OsStr::new("parallel_read.bin");
    let reply_create = fs.create(req, 1, file_name, 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    let initial_data = vec![8u8; 12 * 1024 * 1024];
    fs.write(req, ino, 0, 0, &initial_data, 0, 0).await.unwrap();
    fs.flush(req, ino, 0, 0).await.unwrap();

    // Read back a range spanning multiple blocks
    let read_result = fs.read(req, ino, 0, 0, 12 * 1024 * 1024).await.unwrap();
    assert_eq!(read_result.data.len(), 12 * 1024 * 1024);
    assert_eq!(read_result.data[..], initial_data[..]);
}

#[tokio::test]
async fn test_multi_backend_routing() {
    let mut con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    // 1. Format volume with S3 config
    format_volume(
        &redis_url,
        "multibackend",
        4 * 1024 * 1024,
        100 * 1024 * 1024,
        0,
        // inodes limit
        "none",
        // compression
        "none",
        // encrypt_algo
        None,
        // encrypt_key
        None,
        None,
        None,
        Some("http://127.0.0.1:9000"),
        Some("minioadmin"),
        Some("minioadmin"),
        Some("test-bucket"),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("Format volume should succeed");

    // 2. Query status to ensure it contains S3 configs
    let status = get_volume_status(&redis_url).await.unwrap();
    assert_eq!(
        status["Setting"]["StorageBackends"]["backend_0"]["endpoint"],
        "http://127.0.0.1:9000"
    );
    assert_eq!(
        status["Setting"]["StorageBackends"]["backend_0"]["bucket"],
        "test-bucket"
    );

    let dlm = DlmClient::new(&redis_url).unwrap();

    // Create multi-backend client and register two distinct mock backends
    let multi_backend = MultiBackendClient::new();
    let backend_0 = RustFsClient::new_mock();
    let backend_1 = RustFsClient::new_mock();

    multi_backend.register_backend("backend_0", backend_0.clone());
    multi_backend.register_backend("backend_1", backend_1.clone());

    // Initially backend_1 is active for writing
    multi_backend.set_active_backend_id("backend_1".to_string());

    let temp_dir = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_dir.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend_0.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm.clone(), multi_backend.clone(), cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // Create a file
    let file_name = OsStr::new("multi_backend_routing_test.bin");
    let reply_create = fs.create(req, 1, file_name, 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    // Write 5MB (which triggers a striped write and bypasses KV inlining)
    let write_data = vec![99u8; 5 * 1024 * 1024];
    fs.write(req, ino, 0, 0, &write_data, 0, 0).await.unwrap();
    fs.flush(req, ino, 0, 0).await.unwrap();

    // Retrieve block map id from redis to verify the key starts with "backend_1:"
    let block_map_id: Option<String> = con
        .hget(format!("metadata:inode_{}", ino), "block_map_id")
        .await
        .unwrap();
    assert!(block_map_id.is_some());
    let map_key = format!("block_map:{}", block_map_id.unwrap());
    let block_keys: Vec<String> = con.hvals(&map_key).await.unwrap();
    assert!(!block_keys.is_empty());
    for bk in &block_keys {
        let (be_id, real_key) = squeezefs::backend::parse_backend_and_key(bk);
        let expected_backend = multi_backend.get_backend_for_key(&real_key);
        assert_eq!(be_id, expected_backend);

        let active_client = if be_id == "backend_0" {
            &backend_0
        } else {
            &backend_1
        };
        let other_client = if be_id == "backend_0" {
            &backend_1
        } else {
            &backend_0
        };

        // Verify active backend indeed contains the block!
        let data = active_client
            .get_object(&real_key)
            .await
            .expect("Block must exist in active backend storage");
        assert!(!data.is_empty());

        // Verify default backend does NOT contain it
        let default_res = other_client.get_object(&real_key).await;
        assert!(
            default_res.is_err(),
            "Block should not exist in other backend storage"
        );
    }

    // Now let's change the active backend to backend_0
    multi_backend.set_active_backend_id("backend_0".to_string());

    // Attempt to read the data back. It should successfully route to backend_1 and retrieve the data
    let read_result = fs.read(req, ino, 0, 0, 5 * 1024 * 1024).await.unwrap();
    assert_eq!(read_result.data.len(), 5 * 1024 * 1024);
    assert_eq!(read_result.data, write_data);

    // Delete the file. It should clean up the blocks from backend_1
    fs.unlink(req, 1, file_name).await.unwrap();

    // Confirm that the block mapping is deleted from Garnet/Redis
    let mapping_exists: bool = con.exists(&map_key).await.unwrap();
    assert!(!mapping_exists, "Block mapping should be deleted");

    // Also assert that the block itself was physically deleted from backend_1!
    for bk in &block_keys {
        let (_, real_key) = squeezefs::backend::parse_backend_and_key(bk);
        let check_res = backend_1.get_object(&real_key).await;
        assert!(
            check_res.is_err(),
            "Block should have been physically deleted from backend_1: {}",
            real_key
        );
    }
}

#[tokio::test]
async fn test_mount_uid_gid_override() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();

    let router = DataRouter::new(dlm.clone(), backend, cache);

    // Instantiate filesystem with custom overridden UID 5001 and GID 5002
    let fs = SqueezefsFilesystem::new(router, dlm, 5001, 5002);

    let req = Request {
        unique: 1,
        uid: 5001,
        gid: 5002,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // Query root directory attributes (inode 1)
    let root_attr = fs.getattr(req, 1, None, 0).await.unwrap();
    assert_eq!(
        root_attr.attr.uid, 5001,
        "Root directory UID must match overridden mount UID"
    );
    assert_eq!(
        root_attr.attr.gid, 5002,
        "Root directory GID must match overridden mount GID"
    );
}

#[tokio::test]
async fn test_fuse_create_write_read_cycle() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();

    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // 1. Create file via FUSE
    let file_name = OsStr::new("cycle_test.txt");
    let reply_create = fs.create(req, 1, file_name, 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    // 2. Mock test file locks (getlk / setlk)
    fs.setlk(req, ino, 0, 9999, 0, 100, libc::F_WRLCK as u32, 1234, false)
        .await
        .unwrap();
    let reply_lock = fs
        .getlk(req, ino, 0, 9999, 0, 100, libc::F_WRLCK as u32, 1234)
        .await
        .unwrap();
    assert_eq!(reply_lock.r#type, libc::F_UNLCK as u32);

    // 3. Write data via FUSE
    let initial_data = b"hello, squeezefs FUSE read-write cycle test!";
    fs.write(req, ino, 0, 0, initial_data, 0, 0).await.unwrap();
    fs.flush(req, ino, 0, 0).await.unwrap();

    // 4. Read back via FUSE without relying on LRU cache
    fs.router.cache.write_lru.remove(&format!("inode_{}", ino));
    fs.router.cache.read_lru.remove(&format!("inode_{}", ino));
    fs.router.metadata_cache.remove(&format!("inode_{}", ino));

    let read_result = fs
        .read(req, ino, 0, 0, initial_data.len() as u32)
        .await
        .unwrap();
    assert_eq!(read_result.data[..], initial_data[..]);
}

#[tokio::test]
async fn test_vim_swap_file_simulation() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();

    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // 1. Create file
    let file_name = OsStr::new(".test.swp");
    let reply_create = fs.create(req, 1, file_name, 0o600, 0).await.unwrap();
    let ino = reply_create.attr.ino;
    let fh = reply_create.fh;

    // 2. Write 1024 bytes (header) at offset 0
    let header_data = vec![0x55u8; 1024];
    let reply_write = fs.write(req, ino, fh, 0, &header_data, 0, 0).await.unwrap();
    assert_eq!(reply_write.written, 1024);

    // 3. Call fsync
    fs.fsync(req, ino, fh, false).await.unwrap();

    // 4. Call flush
    fs.flush(req, ino, fh, 0).await.unwrap();

    // 5. Call release
    fs.release(req, ino, fh, 0, 0, false).await.unwrap();

    // 5. Query attributes - size must be 1024!
    let attr_reply = fs.getattr(req, ino, None, 0).await.unwrap();
    assert_eq!(attr_reply.attr.size, 1024);

    // 6. Read back data
    let read_reply = fs.read(req, ino, fh, 0, 1024).await.unwrap();
    assert_eq!(read_reply.data.len(), 1024);
    assert_eq!(&read_reply.data[..], &header_data[..]);
}

#[tokio::test]
async fn test_config_sqz_virtual_file() {
    let mut _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    // Format volume to populate squeezefs:format and backends
    format_volume(
        &redis_url,
        "testvolume_config",
        1024 * 1024,
        1000 * 1024 * 1024,
        0,
        // inodes limit
        "none",
        // compression
        "none",
        // encrypt_algo
        None,
        // encrypt_key
        Some("64MB"),
        Some("100MB"),
        None,
        Some("http://s3.local"),
        Some("my-access-key"),
        Some("my-secret-key"),
        Some("my-bucket"),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("Format volume should succeed");

    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();

    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // 1. Lookup ".config" under parent 1 (root)
    let lookup_reply = fs.lookup(req, 1, OsStr::new(".config")).await.unwrap();
    let config_ino = lookup_reply.attr.ino;
    assert_eq!(config_ino, 0xffff_ffff_ffff_fffe); // CONFIG_INODE
    assert_eq!(lookup_reply.attr.perm, 0o444); // Read-only

    // 2. GetAttr CONFIG_INODE
    let attr_reply = fs.getattr(req, config_ino, None, 0).await.unwrap();
    assert_eq!(attr_reply.attr.ino, config_ino);
    assert_eq!(attr_reply.attr.perm, 0o444);
    let expected_size = attr_reply.attr.size;

    // 3. Read config data
    let read_reply = fs.read(req, config_ino, 0, 0, 8192).await.unwrap();
    let config_json = String::from_utf8(read_reply.data.to_vec()).unwrap();
    assert_eq!(config_json.len() as u64, expected_size);

    // Verify config JSON content and masked credentials
    let parsed: serde_json::Value = serde_json::from_str(&config_json).unwrap();
    assert!(parsed.get("client_version").is_some());
    assert_eq!(parsed["backends"]["backend_0"]["access_key"], "******");
    assert_eq!(parsed["backends"]["backend_0"]["secret_key"], "******");
    assert_eq!(
        parsed["backends"]["backend_0"]["endpoint"],
        "http://s3.local"
    );

    // 4. Try to write to CONFIG_INODE -> EACCES
    let write_res = fs.write(req, config_ino, 0, 0, b"data", 0, 0).await;
    assert!(write_res.is_err());
    let err_code = write_res.err().unwrap();
    assert_eq!(err_code, fuse3::Errno::from(libc::EACCES));

    // 5. Try to setattr of CONFIG_INODE -> EACCES
    let set_attr = SetAttr {
        size: Some(10),
        ..Default::default()
    };
    let setattr_res = fs.setattr(req, config_ino, None, set_attr).await;
    assert!(setattr_res.is_err());
    assert_eq!(setattr_res.err().unwrap(), fuse3::Errno::from(libc::EACCES));

    // 6. Try to unlink ".config" -> EPERM
    let unlink_res = fs.unlink(req, 1, OsStr::new(".config")).await;
    assert!(unlink_res.is_err());
    assert_eq!(unlink_res.err().unwrap(), fuse3::Errno::from(libc::EPERM));

    // 7. Try to rename ".config" -> EPERM
    let rename_res = fs
        .rename(req, 1, OsStr::new(".config"), 1, OsStr::new("new.config"))
        .await;
    assert!(rename_res.is_err());
    assert_eq!(rename_res.err().unwrap(), fuse3::Errno::from(libc::EPERM));

    // 8. Verify readdir contains ".config"
    let readdir_reply = fs.readdir(req, 1, 0, 0).await.unwrap();
    use futures::StreamExt;
    let entries: Vec<_> = readdir_reply.entries.collect().await;
    let config_entry = entries
        .iter()
        .map(|r| r.as_ref().unwrap())
        .find(|e| e.name == ".config")
        .expect("Readdir must contain .config entry");
    assert_eq!(config_entry.inode, config_ino);

    // 9. Verify readdirplus contains ".config"
    let readdirplus_reply = fs.readdirplus(req, 1, 0, 0, 0).await.unwrap();
    let entries_plus: Vec<_> = readdirplus_reply.entries.collect().await;
    let config_entry_plus = entries_plus
        .iter()
        .map(|r| r.as_ref().unwrap())
        .find(|e| e.name == ".config")
        .expect("Readdirplus must contain .config entry");
    assert_eq!(config_entry_plus.inode, config_ino);
    assert_eq!(config_entry_plus.attr.ino, config_ino);
    assert_eq!(config_entry_plus.attr.perm, 0o444);
}

#[tokio::test]
async fn test_inode_quota_enforcement() {
    let mut _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    // Format volume to populate squeezefs:format, max inodes set to 3
    format_volume(
        &redis_url,
        "quota_vol",
        1024 * 1024,
        1000 * 1024 * 1024,
        3,
        "none",
        "none",
        None,
        Some("64MB"),
        Some("100MB"),
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
    .await
    .expect("Format volume should succeed");

    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();

    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // Verify initial statfs report
    let stat1 = fs.statfs(req, 1).await.unwrap();
    assert_eq!(stat1.files, 3);
    assert_eq!(stat1.ffree, 2); // 3 total - 1 (root directory) = 2 free

    // 1. Create first file (should succeed, inode 2)
    let create1 = fs
        .create(req, 1, OsStr::new("file1"), 0o644, 0)
        .await
        .unwrap();
    assert_eq!(create1.attr.ino, 2);

    let stat2 = fs.statfs(req, 1).await.unwrap();
    assert_eq!(stat2.ffree, 1);

    // 2. Create second file (should succeed, inode 3)
    let create2 = fs
        .create(req, 1, OsStr::new("file2"), 0o644, 0)
        .await
        .unwrap();
    assert_eq!(create2.attr.ino, 3);

    let stat3 = fs.statfs(req, 1).await.unwrap();
    assert_eq!(stat3.ffree, 0); // No free inodes left

    // 3. Create third file (should fail with ENOSPC)
    let create3_res = fs.create(req, 1, OsStr::new("file3"), 0o644, 0).await;
    assert!(create3_res.is_err());
    assert_eq!(create3_res.err().unwrap(), fuse3::Errno::from(libc::ENOSPC));

    // 4. Delete one file (file1)
    fs.unlink(req, 1, OsStr::new("file1")).await.unwrap();

    let stat4 = fs.statfs(req, 1).await.unwrap();
    assert_eq!(stat4.ffree, 1); // 1 free inode now

    // 5. Try creating again (should succeed now, allocating a new inode counter value e.g. 4)
    let create4 = fs
        .create(req, 1, OsStr::new("file3"), 0o644, 0)
        .await
        .unwrap();
    assert_eq!(create4.attr.ino, 4);

    let stat5 = fs.statfs(req, 1).await.unwrap();
    assert_eq!(stat5.ffree, 0);
}

#[tokio::test]
async fn test_capacity_quota_enforcement() {
    let mut _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    // Format volume to populate squeezefs:format, capacity limit set to 100 bytes
    format_volume(
        &redis_url,
        "capacity_vol",
        1024 * 1024,
        100,
        // capacity limit: 100 bytes
        0,
        // inodes limit: unlimited
        "none",
        // compression
        "none",
        // encrypt_algo
        None,
        // encrypt_key
        Some("64MB"),
        Some("100MB"),
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
    .await
    .expect("Format volume should succeed");

    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();

    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // 1. Create file1 (inode 2)
    let create1 = fs
        .create(req, 1, OsStr::new("file1"), 0o644, 0)
        .await
        .unwrap();
    let ino = create1.attr.ino;
    assert_eq!(ino, 2);

    // 2. Write 60 bytes (should succeed)
    let data1 = vec![42u8; 60];
    let write1 = fs.write(req, ino, 0, 0, &data1, 0, 0).await.unwrap();
    assert_eq!(write1.written, 60);

    // 3. Write another 50 bytes at offset 60 (total would be 110, exceeds 100 capacity -> should fail with ENOSPC)
    let data2 = vec![42u8; 50];
    let write2_res = fs.write(req, ino, 0, 60, &data2, 0, 0).await;
    assert!(write2_res.is_err());
    assert_eq!(write2_res.err().unwrap(), fuse3::Errno::from(libc::ENOSPC));

    // 4. Truncate / setattr to 120 bytes (exceeds capacity -> should fail with ENOSPC)
    let set_attr_large = SetAttr {
        size: Some(120),
        ..Default::default()
    };
    let setattr_large_res = fs.setattr(req, ino, None, set_attr_large).await;
    assert!(setattr_large_res.is_err());
    assert_eq!(
        setattr_large_res.err().unwrap(),
        fuse3::Errno::from(libc::ENOSPC)
    );

    // 5. Shrink / truncate file size to 30 bytes (should succeed)
    let set_attr_small = SetAttr {
        size: Some(30),
        ..Default::default()
    };
    let setattr_small_res = fs.setattr(req, ino, None, set_attr_small).await.unwrap();
    assert_eq!(setattr_small_res.attr.size, 30);

    // 6. Now we have 70 bytes free space. Write 50 bytes (should succeed now)
    let data3 = vec![42u8; 50];
    let write3 = fs.write(req, ino, 0, 30, &data3, 0, 0).await.unwrap();
    assert_eq!(write3.written, 50);

    // 7. Write 30 bytes (total 110 -> should fail with ENOSPC)
    let data4 = vec![42u8; 30];
    let write4_res = fs.write(req, ino, 0, 80, &data4, 0, 0).await;
    assert!(write4_res.is_err());
    assert_eq!(write4_res.err().unwrap(), fuse3::Errno::from(libc::ENOSPC));
}

#[tokio::test]
async fn test_compression_and_encryption_flow() {
    let _ = env_logger::builder().is_test(true).try_init();
    let mut con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    use rsa::pkcs1::EncodeRsaPrivateKey;
    let mut rng = rand::thread_rng();
    let priv_key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let pem = priv_key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();

    let redis_url = get_redis_url();
    // Format volume with lz4 compression and aes256gcm-rsa encryption
    format_volume(
        &redis_url,
        "crypto_vol",
        1024 * 1024,
        1000 * 1024 * 1024,
        0,
        // inodes limit
        "lz4",
        // compression
        "aes256gcm-rsa",
        // encrypt_algo
        Some(&pem),
        // encrypt_key (PEM string)
        Some("64MB"),
        Some("100MB"),
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
    .await
    .expect("Format volume should succeed");

    let dlm = DlmClient::new(&redis_url).unwrap();
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
    .unwrap();

    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.unwrap();

    // 1. Create file (inode 2)
    let create_res = fs
        .create(req, 1, OsStr::new("encrypted_file"), 0o644, 0)
        .await
        .unwrap();
    let ino = create_res.attr.ino;
    assert_eq!(ino, 2);

    // 2. Write a compressible payload
    let plaintext =
        b"Hello World! This is a test of client-side encryption and compression. ".repeat(10);
    let write_res = fs.write(req, ino, 0, 0, &plaintext, 0, 0).await.unwrap();
    assert_eq!(write_res.written as usize, plaintext.len());

    // 3. Read it back and verify it matches plaintext
    let read_res = fs
        .read(req, ino, 0, 0, plaintext.len() as u32)
        .await
        .unwrap();
    assert_eq!(read_res.data.as_ref(), plaintext.as_slice());

    // 4. Verify that the raw data stored in Garnet (since it's inline under 64KB)
    // is encrypted and compressed (not equal to plaintext, and doesn't contain "Hello World!")
    let inline_key = "inline_data:inode_2";
    let stored_bytes: Vec<u8> = redis::cmd("GET")
        .arg(inline_key)
        .query_async(&mut con)
        .await
        .unwrap();

    assert_ne!(stored_bytes, plaintext);
    let contains_plaintext = stored_bytes.windows(12).any(|w| w == b"Hello World!");
    assert!(
        !contains_plaintext,
        "Stored bytes should be encrypted and must not expose plaintext"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn test_real_mount_and_browseable() {
    let _ = env_logger::try_init();

    // 1. Check if /dev/fuse is accessible
    if !std::path::Path::new("/dev/fuse").exists() {
        println!("Skipping real FUSE mount test: /dev/fuse does not exist");
        return;
    }

    let _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let temp_dir_cache = tempdir().unwrap();
    let temp_dir_mount = tempdir().unwrap();
    let mount_path = temp_dir_mount.path().to_path_buf();

    let redis_url = get_redis_url();
    format_volume(
        &redis_url,
        "mount_test_vol",
        4 * 1024 * 1024,
        1024 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        Some("1GB"),
        Some("10GB"),
        Some(&[temp_dir_cache.path().to_path_buf()]),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("Format volume should succeed");

    // Spawn mount task in background via sudo
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let mount_path_clone = mount_path.clone();
    let redis_url_clone = redis_url.clone();
    let cache_dir_path = temp_dir_cache.path().to_path_buf();

    let mut mount_child = std::process::Command::new("sudo")
        .arg("./target/debug/squeezefs")
        .arg("--garnet-url")
        .arg(&redis_url_clone)
        .arg("mount")
        .arg(&mount_path_clone)
        .arg("--cache-dir")
        .arg(&cache_dir_path)
        .arg("--uid")
        .arg(uid.to_string())
        .arg("--gid")
        .arg(gid.to_string())
        .arg("--allow-other")
        .spawn()
        .expect("Failed to spawn squeezefs mount via sudo");

    // Wait for the mountpoint to become ready and check if browseable
    let mut ready = false;
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(metadata) = std::fs::metadata(&mount_path) {
            use std::os::unix::fs::MetadataExt;
            if metadata.ino() == 1 {
                ready = true;
                break;
            }
        }
    }

    if !ready {
        // Unmount before failing to be clean
        let _ = std::process::Command::new("sudo")
            .arg("fusermount3")
            .arg("-u")
            .arg("-z")
            .arg(&mount_path)
            .output();
        let _ = mount_child.kill();
        panic!("FUSE mount failed to become ready at {:?}", mount_path);
    }

    // Perform file operations to verify browseability
    let test_file = mount_path.join("real_mount_test.txt");
    std::fs::write(&test_file, "hello real mount").expect("Should write file to mountpoint");
    let content = std::fs::read_to_string(&test_file).expect("Should read file from mountpoint");
    assert_eq!(content, "hello real mount");

    // Clean up mountpoint by running fusermount3 -u via sudo
    let unmount_res = std::process::Command::new("sudo")
        .arg("fusermount3")
        .arg("-u")
        .arg(&mount_path)
        .output();

    if let Ok(output) = unmount_res {
        if !output.status.success() {
            // Lazy unmount as fallback
            let _ = std::process::Command::new("sudo")
                .arg("fusermount3")
                .arg("-u")
                .arg("-z")
                .arg(&mount_path)
                .output();
        }
    }

    // Wait for the child mount process to terminate
    let _ = mount_child.wait();
}
