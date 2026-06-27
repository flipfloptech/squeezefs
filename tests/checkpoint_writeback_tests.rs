use fuse3::raw::{prelude::*, Request};
use redis::AsyncCommands;
use squeezefs::backend::{MultiBackendClient, RustFsClient};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{format_volume, SqueezefsFilesystem};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::path::PathBuf;
use tempfile::tempdir;

#[allow(dead_code)]
fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn clean_db(redis_url: &str) -> Option<()> {
    let mut con = redis::Client::open(redis_url)
        .ok()?
        .get_multiplexed_tokio_connection()
        .await
        .ok()?;
    let _: () = redis::cmd("FLUSHDB")
        .query_async(&mut con)
        .await
        .unwrap_or(());
    Some(())
}

#[tokio::test]
async fn test_checkpoint_writeback_and_multipart_upload() {
    let redis_url = "redis://127.0.0.1:6379/11".to_string();
    if clean_db(&redis_url).await.is_none() {
        println!("Skipping test: Garnet/Redis not available");
        return;
    }

    let fs_name = "checkpoint_test_vol";

    // Format the volume: let's set block size to 1MB to make testing faster
    format_volume(
        &redis_url,
        fs_name,
        1024 * 1024, // 1MB block size
        100 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        Some("128MB"),
        Some("500MB"),
        Some(&[PathBuf::from("/tmp/squeezefs_staging_checkpoint")]),
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
    .expect("Failed");

    let dlm = DlmClient::new(&redis_url).expect("Failed");
    let backend = RustFsClient::new_mock();
    let multi_backend = MultiBackendClient::new();
    multi_backend.register_backend("backend_0", backend.clone());

    let temp_staging = tempdir().expect("Failed");
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("128MB"),
        Some("500MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .expect("Failed");

    let router = DataRouter::new(dlm.clone(), multi_backend.clone(), cache.clone());
    let fs = SqueezefsFilesystem::new(router.clone(), dlm.clone(), 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.expect("Failed");

    // Set file as striped
    let name = "checkpoint.bin";
    let reply_create = fs
        .create(req, 1, OsStr::new(name), 0o644, 0)
        .await
        .expect("Failed");
    let ino = reply_create.attr.ino;

    let mut con = dlm.get_connection().await.expect("Failed");
    let meta_key = format!("metadata:inode_{}", ino);
    // Explicitly set type to striped so the Progressive layout promotes it immediately
    let _: () = con
        .hset(&meta_key, "type", "striped")
        .await
        .expect("Failed");

    // Prepare 2.5MB data (3 blocks: 1MB, 1MB, 0.5MB)
    let write_data = vec![0xAA; 2500000]; // 2,500,000 bytes

    // Write file sequentially
    fs.write(req, ino, 0, 0, &write_data, 0, 0)
        .await
        .expect("Failed");

    // Flush the blocks to trigger upload/multipart initialization!
    fs.flush(req, ino, 0, 0).await.expect("Failed");

    // Check that read_lru has 0 bytes (bypassed entirely!)
    assert_eq!(fs.router.cache.read_lru.current_bytes(), 0);

    // Verify active multipart hash is stored in Redis
    let active_mp_key = format!("squeezefs:active_multipart:{}", ino);
    let upload_id: Option<String> = con.hget(&active_mp_key, "upload_id").await.expect("Failed");
    assert!(
        upload_id.is_some(),
        "Should have active multipart upload_id in Redis before release"
    );

    // Release/Close the file
    fs.release(req, ino, 0, 0, 0, false).await.expect("Failed");

    // Verify active multipart hash is deleted from Redis
    let upload_id_post: Option<String> =
        con.hget(&active_mp_key, "upload_id").await.expect("Failed");
    assert!(
        upload_id_post.is_none(),
        "Active multipart metadata should be deleted from Redis after release"
    );

    // Verify block map has been updated to use s3_single
    let block_map_id_opt: Option<String> =
        con.hget(&meta_key, "block_map_id").await.expect("Failed");
    let block_map_id = block_map_id_opt.expect("Failed");
    let block_map_key = format!("block_map:{}", block_map_id);

    let b0_key: Option<String> = con.hget(&block_map_key, "0").await.expect("Failed");
    let b1_key: Option<String> = con.hget(&block_map_key, "1").await.expect("Failed");
    let b2_key: Option<String> = con.hget(&block_map_key, "2").await.expect("Failed");

    assert!(b0_key.expect("Failed").contains("s3_single"));
    assert!(b1_key.expect("Failed").contains("s3_single"));
    assert!(b2_key.expect("Failed").contains("s3_single"));

    // Verify data integrity on read back!
    let read_buf = vec![0u8; write_data.len()];
    let read_reply = fs
        .read(req, ino, 0, 0, read_buf.len() as u32)
        .await
        .expect("Failed");
    assert_eq!(read_reply.data.len(), write_data.len());
    assert_eq!(read_reply.data.as_ref(), write_data.as_slice());

    // Clean up
    drop(fs);
}

#[tokio::test]
async fn test_checkpoint_writeback_existing_file_fallback() {
    let redis_url = "redis://127.0.0.1:6379/12".to_string();
    if clean_db(&redis_url).await.is_none() {
        return;
    }
    let fs_name = "checkpoint_test_vol_fallback";

    format_volume(
        &redis_url,
        fs_name,
        1024 * 1024, // 1MB block size
        100 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        Some("128MB"),
        Some("500MB"),
        Some(&[PathBuf::from("/tmp/squeezefs_staging_checkpoint_fallback")]),
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
    .expect("Failed");

    let dlm = DlmClient::new(&redis_url).expect("Failed");
    let backend = RustFsClient::new_mock();
    let multi_backend = MultiBackendClient::new();
    multi_backend.register_backend("backend_0", backend.clone());

    let temp_staging = tempdir().expect("Failed");
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("128MB"),
        Some("500MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .expect("Failed");

    let router = DataRouter::new(dlm.clone(), multi_backend.clone(), cache.clone());
    let fs = SqueezefsFilesystem::new(router.clone(), dlm.clone(), 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    fs.init(req).await.expect("Failed");

    let name = "checkpoint_fallback.bin";
    let reply_create = fs
        .create(req, 1, OsStr::new(name), 0o644, 0)
        .await
        .expect("Failed");
    let ino = reply_create.attr.ino;

    let mut con = dlm.get_connection().await.expect("Failed");
    let meta_key = format!("metadata:inode_{}", ino);
    let _: () = con
        .hset(&meta_key, "type", "striped")
        .await
        .expect("Failed");

    // 1. Initial write to populate block map
    let initial_data = vec![0xBB; 512 * 1024]; // 512KB
    fs.write(req, ino, 0, 0, &initial_data, 0, 0)
        .await
        .expect("Failed");

    fs.flush(req, ino, 0, 0).await.expect("Failed");
    fs.release(req, ino, 0, 0, 0, false).await.expect("Failed");

    // 2. Perform another write on the existing file (block map is not empty)
    let overwrite_data = vec![0xCC; 1024 * 1024]; // 1MB
                                                  // Open the file again
    fs.open(req, ino, 0).await.expect("Failed");

    fs.write(req, ino, 0, 0, &overwrite_data, 0, 0)
        .await
        .expect("Failed");

    // Verify active multipart hash is NOT stored in Redis because it fallbacks to standard blocks!
    let active_mp_key = format!("squeezefs:active_multipart:{}", ino);
    let upload_id: Option<String> = con.hget(&active_mp_key, "upload_id").await.expect("Failed");
    assert!(
        upload_id.is_none(),
        "Should NOT initialize multipart upload for existing/non-empty file writes"
    );

    fs.flush(req, ino, 0, 0).await.expect("Failed");
    fs.release(req, ino, 0, 0, 0, false).await.expect("Failed");
}
