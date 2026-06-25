use fuse3::raw::Filesystem;
use fuse3::raw::Request;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::tempdir;

async fn clean_db() -> Option<redis::aio::MultiplexedConnection> {
    let client = match redis::Client::open("redis://127.0.0.1:6379/") {
        Ok(c) => c,
        Err(_) => return None,
    };
    let mut con = match client.get_multiplexed_async_connection().await {
        Ok(c) => c,
        Err(_) => return None,
    };
    let _: () = redis::cmd("FLUSHDB").query_async(&mut con).await.unwrap();
    Some(con)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_client_directory_cache() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => return,
    };

    let redis_url = "redis://127.0.0.1:6379/";
    let dlm = DlmClient::new(redis_url).unwrap();
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

    // Verify cache is initially empty
    assert_eq!(fs.dir_entry_cache.entry_count(), 0);

    // Call lookup - should miss and populate cache
    let file_name = OsStr::new("test_cache_file.txt");
    let _ = fs.lookup(req, 1, file_name).await; // Should ENOENT but parent cache will populate

    assert!(
        fs.dir_entry_cache.get(&1).is_some(),
        "Parent directory entries should be cached after lookup"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_directory_cache_invalidation() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => return,
    };

    let redis_url = "redis://127.0.0.1:6379/";
    let dlm = DlmClient::new(redis_url).unwrap();
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

    // Perform lookup to populate cache
    let file_name = OsStr::new("test_inv_file.txt");
    let _ = fs.lookup(req, 1, file_name).await;
    assert!(fs.dir_entry_cache.get(&1).is_some());

    // Create file - should invalidate cache
    let _reply = fs.create(req, 1, file_name, 0o644, 0).await.unwrap();
    assert!(
        fs.dir_entry_cache.get(&1).is_none(),
        "Cache must be invalidated on create"
    );

    // Re-populate
    let _ = fs.lookup(req, 1, file_name).await;
    assert!(fs.dir_entry_cache.get(&1).is_some());

    // Unlink file - should invalidate cache
    fs.unlink(req, 1, file_name).await.unwrap();
    assert!(
        fs.dir_entry_cache.get(&1).is_none(),
        "Cache must be invalidated on unlink"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_metadata_lua_scripts() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => return,
    };

    let redis_url = "redis://127.0.0.1:6379/";
    let dlm = DlmClient::new(redis_url).unwrap();
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

    // 1. Create file (will use Lua script)
    let file_name = OsStr::new("lua_test_file.txt");
    let reply_create = fs.create(req, 1, file_name, 0o644, 0).await.unwrap();
    let file_ino = reply_create.attr.ino;

    // Verify it exists in directory listing and lookups
    let reply_lookup = fs.lookup(req, 1, file_name).await.unwrap();
    assert_eq!(reply_lookup.attr.ino, file_ino);

    // 2. Create directory (will use Lua script)
    let dir_name = OsStr::new("lua_test_dir");
    let reply_mkdir = fs.mkdir(req, 1, dir_name, 0o755, 0).await.unwrap();
    let dir_ino = reply_mkdir.attr.ino;

    let reply_lookup_dir = fs.lookup(req, 1, dir_name).await.unwrap();
    assert_eq!(reply_lookup_dir.attr.ino, dir_ino);

    // 3. Unlink file (will use Lua script)
    fs.unlink(req, 1, file_name).await.unwrap();
    assert!(fs.lookup(req, 1, file_name).await.is_err());

    // 4. Rmdir directory (will use Lua script)
    fs.rmdir(req, 1, dir_name).await.unwrap();
    assert!(fs.lookup(req, 1, dir_name).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_staging_backpressure_wait() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => return,
    };

    let temp_dir = tempdir().unwrap();
    let mock_backend = RustFsClient::new_mock();
    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();

    // 5KB staging capacity limit
    let nvme = squeezefs::cache::NvmeStaging::new(
        vec![temp_dir.path().to_path_buf()],
        5 * 1024,
        100 * 1024 * 1024,
        mock_backend,
        squeezefs::dlm::MetaClient::Single(redis_client),
    )
    .expect("Should construct NVMe staging");

    // Fill staging with 4KB (padded size is aligned to 4096 so it is exactly 4096 bytes)
    let data1 = vec![1u8; 2000];
    nvme.stage_write("f1", "id1", &data1, 1).await.unwrap();

    let nvme_clone = nvme.clone();
    // Spawn task to write data2 (2KB) -> total would be 6KB > 5KB capacity.
    // It should block and wait.
    let write_handle = tokio::spawn(async move {
        let start = std::time::Instant::now();
        let data2 = vec![2u8; 1000];
        // This stage_write should wait on backpressure
        nvme_clone
            .stage_write("f2", "id2", &data2, 2)
            .await
            .unwrap();
        start.elapsed()
    });

    // Let the second write block for 500ms
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Simulate merge worker freeing space by purging "id1"
    let staged_path = temp_dir.path().join("staging").join("file_id1.staged");
    let _ = std::fs::remove_file(staged_path);
    nvme.space_freed_notify.notify_waiters();
    // Directly subtract usage
    nvme.current_staged_write_bytes.store(0, Ordering::Relaxed);
    nvme.space_freed_notify.notify_waiters();

    let elapsed = write_handle.await.unwrap();
    assert!(
        elapsed >= Duration::from_millis(400),
        "Should wait at least until space is freed (elapsed: {:?})",
        elapsed
    );
}

#[test]
fn test_fuse_mount_custom_options() {
    let opts = "max_read=1048576,entry_timeout=1.5,attr_timeout=2.0";
    let parsed = squeezefs::fuse_client::parse_custom_options(opts);
    let parsed_str = parsed.to_string_lossy();
    assert!(parsed_str.contains("max_read=1048576"));
    assert!(!parsed_str.contains("entry_timeout=1.5"));
    assert!(!parsed_str.contains("attr_timeout=2.0"));
}
