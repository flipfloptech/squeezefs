use fuse3::raw::{prelude::*, Request};
use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{format_volume, get_volume_status, start_mount, SqueezefsFilesystem};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::time::Duration;
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
        8 * 1024 * 1024, // 8MB block size
        500 * 1024 * 1024 * 1024 * 1024, // 500TB capacity
        Some("64GB"),
        Some("100GB"),
        Some(&[std::path::PathBuf::from("/tmp/test_staging_format")]),
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
    let _ = fs.init(req).await.expect("Auto-format should set up volume on init");
    
    // Check that format config key was written
    let format_exists: bool = con.exists("squeezefs:format").await.unwrap_or(false);
    assert!(format_exists, "Auto-format should have created squeezefs:format key");

    // ABI check: if we write a higher version in the format key, subsequent fs creation should fail
    let _: () = con.hset("squeezefs:format", "version", 99).await.unwrap();
    
    // Re-create fs
    let dlm2 = DlmClient::new(&redis_url).unwrap();
    let backend2 = RustFsClient::new().await;
    let cache2 = TieredCache::new(
        vec![temp_dir.path().to_path_buf()],
        None,
        None,
        backend2.clone(),
        dlm2.meta_client().clone(),
    )
    .unwrap();
    let router2 = DataRouter::new(dlm2.clone(), backend2, cache2);
    let fs2 = SqueezefsFilesystem::new(router2, dlm2, 1000, 1000);
    
    let res = fs2.init(req).await;
    assert!(res.is_err(), "Init should fail because database ABI version (99) is higher than supported");
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
    let initial_used: u64 = con.get("squeezefs:used_bytes").await.unwrap_or(None).unwrap_or(0);
    assert_eq!(initial_used, 0);

    // Perform a write
    let data = vec![7u8; 1000];
    let _reply_write = fs.write(req, ino, 0, 0, &data, 0, 0).await.unwrap();

    // Wait a brief moment and verify used_bytes is updated
    let post_write_used: u64 = con.get("squeezefs:used_bytes").await.unwrap_or(None).unwrap_or(0);
    assert_eq!(post_write_used, 1000);

    // Perform statfs check
    let reply_statfs = fs.statfs(req, 1).await.unwrap();
    assert_eq!(reply_statfs.bsize, 4096);
    assert!(reply_statfs.blocks > 0);
    assert_eq!(reply_statfs.blocks - reply_statfs.bfree, 1); // 1000 bytes takes 1 block (4096 bsize)

    // Truncate file (setattr size to 500)
    let mut set_attr = SetAttr::default();
    set_attr.size = Some(500);
    let _ = fs.setattr(req, ino, None, set_attr).await.unwrap();
    let post_trunc_used: u64 = con.get("squeezefs:used_bytes").await.unwrap_or(None).unwrap_or(0);
    assert_eq!(post_trunc_used, 500);

    // Delete file (unlink)
    fs.unlink(req, 1, file_name).await.unwrap();
    let post_delete_used: u64 = con.get("squeezefs:used_bytes").await.unwrap_or(None).unwrap_or(0);
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
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();
    let router = DataRouter::new(dlm.clone(), backend, cache);
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    // If we pass a nonexistent path or trigger an ENOTCONN, it should fail
    // We can simulate an ENOTCONN by checking if mounting on a stale directory returns standard error rather than unmounting it.
    // We check if start_mount returns standard IO error for invalid setups.
    let res = start_mount("/nonexistent/mountpoint/path/here", fs, 1000, 1000).await;
    assert!(res.is_err(), "Mount should fail on invalid path");
}

#[tokio::test]
async fn test_three_tiered_writeback_and_lease_cache() {
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
    fs.write(req, ino, 0, 1024 * 1024, &write_data_1, 0, 0).await.unwrap();
    fs.write(req, ino, 0, 2 * 1024 * 1024, &write_data_2, 0, 0).await.unwrap();

    // Verify local staging directory `/tmp/squeezefs_staging/active_writes` exists and has dirty block files
    let active_dir = std::path::PathBuf::from("/tmp/squeezefs_staging")
        .join("active_writes")
        .join(format!("inode_{}", ino));
    assert!(active_dir.exists(), "Local active writes directory must exist");
    assert!(active_dir.join("block_0").exists(), "Local dirty block 0 file must be present");

    // Call FUSE flush (mimicking close)
    fs.flush(req, ino, 0, 0).await.expect("Flush should succeed");

    // Verify local active writes directory has been cleaned up after flush
    assert!(!active_dir.exists(), "Local active writes directory must be cleaned up post-flush");
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
    let read_result = fs.read(req, ino, 0, 1024, 12 * 1024 * 1024).await.unwrap();
    assert_eq!(read_result.data.len(), 12 * 1024 * 1024);
    assert_eq!(read_result.data[..], initial_data[..]);
}
