use fuse3::raw::{prelude::*, Request};
use redis::AsyncCommands;
use squeezefs::backend::{MultiBackendClient, RustFsClient};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    format_volume, GdsReadArgs, SqueezefsFilesystem, SQUEEZEFS_IOC_GDS_READ,
};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::path::PathBuf;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn clean_db() -> Option<()> {
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
    Some(())
}

#[tokio::test]
async fn test_gds_hardening_ioctl_flow() {
    if clean_db().await.is_none() {
        println!("Skipping test: Garnet/Redis not available");
        return;
    }

    let redis_url = get_redis_url();
    let fs_name = "gds_test_vol";

    // Format the volume: set block size to 1MB
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
        Some(&[PathBuf::from("/tmp/squeezefs_staging_gds")]),
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
    .unwrap();

    let dlm = DlmClient::new(&redis_url).unwrap();
    let backend = RustFsClient::new_mock();
    let multi_backend = MultiBackendClient::new();
    multi_backend.register_backend("backend_0", backend.clone());

    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("128MB"),
        Some("500MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    // Enable GDS simulated mode by forcing it to report as available
    cache
        .gds
        .force_available
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let router = DataRouter::new(dlm.clone(), multi_backend.clone(), cache.clone());
    let fs = SqueezefsFilesystem::new(router.clone(), dlm.clone(), 1000, 1000);

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: std::process::id(), // Pass the test process's actual PID!
    };
    fs.init(req).await.unwrap();

    // Create file
    let name = "gds_large_file.bin";
    let reply_create = fs.create(req, 1, OsStr::new(name), 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    let mut con = dlm.get_connection().await.unwrap();
    let meta_key = format!("metadata:inode_{}", ino);
    // Explicitly set type to striped so the Progressive layout promotes it immediately
    let _: () = con.hset(&meta_key, "type", "striped").await.unwrap();

    // Write 1.5MB of data (2 blocks)
    let write_data = vec![0xAB; 1500000];
    fs.write(req, ino, 0, 0, &write_data, 0, 0).await.unwrap();

    // Flush and release to commit blocks to S3/mock backend and setup s3_single mappings
    fs.flush(req, ino, 0, 0).await.unwrap();
    fs.release(req, ino, 0, 0, 0, false).await.unwrap();

    // Clear caches to force download from backend
    fs.router.cache.read_lru.clear();

    // Prepare GdsReadArgs structure on the stack
    let args = GdsReadArgs {
        vram_address: 0xDEADBEEF0000, // Mock GPU VRAM address pointer
        offset: 100,                  // Offset to read from
        size: 5000,                   // Size to read
    };

    // Obtain the pointer to args in this process's memory
    let args_ptr = &args as *const GdsReadArgs as u64;

    // Invoke FUSE ioctl (SQUEEZEFS_IOC_GDS_READ) directly
    let ioctl_res = fs
        .ioctl(req, ino, 0, 0, SQUEEZEFS_IOC_GDS_READ, args_ptr, 0, 0)
        .await;

    assert!(ioctl_res.is_ok(), "GDS ioctl failed: {:?}", ioctl_res);
    let reply_ioctl = ioctl_res.unwrap();
    assert_eq!(reply_ioctl.result, 0);

    // Verify that the mock backend served the object read requests
    assert!(
        backend.mock_range_get_count() > 0 || backend.mock_get_count() > 0,
        "Backend should have been queried during simulated GDS read"
    );
}
