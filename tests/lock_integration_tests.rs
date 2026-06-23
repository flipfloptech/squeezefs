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
async fn test_fuse_posix_lock_acquisition_and_release() {
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

    // 1. Create a file
    let reply_created = fs
        .create(req, 1, OsStr::new("lock_test_file.bin"), 0o644, 0)
        .await
        .expect("Create file should succeed");
    let ino = reply_created.attr.ino;

    // 2. Process A (owner 1001) locks range [0, 100]
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
    .expect("Lock acquisition by Process A should succeed");

    // 3. Process B (owner 1002) checks range [50, 150] (overlaps [0, 100])
    let reply_get = fs
        .getlk(req, ino, 102, 1002, 50, 150, libc::F_WRLCK as u32, 5678)
        .await
        .expect("Getlk should succeed");
    assert_eq!(
        reply_get.r#type,
        libc::F_WRLCK as u32,
        "Should return that lock is held"
    );

    // 4. Process B checks range [101, 200] (does not overlap [0, 100])
    let reply_get_non_overlap = fs
        .getlk(req, ino, 102, 1002, 101, 200, libc::F_WRLCK as u32, 5678)
        .await
        .expect("Getlk should succeed");
    assert_eq!(
        reply_get_non_overlap.r#type,
        libc::F_UNLCK as u32,
        "Should return that range is unlocked"
    );

    // 5. Process A unlocks range [0, 100]
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
    .expect("Unlock by Process A should succeed");

    // 6. Process B checks range [50, 150] again, should now be unlocked
    let reply_get_after = fs
        .getlk(req, ino, 102, 1002, 50, 150, libc::F_WRLCK as u32, 5678)
        .await
        .expect("Getlk should succeed");
    assert_eq!(
        reply_get_after.r#type,
        libc::F_UNLCK as u32,
        "Should be unlocked after release"
    );
}

#[tokio::test]
async fn test_fuse_posix_lock_blocking() {
    let (fs, _temp_dir) = match setup_fs().await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let req = Request {
        unique: 11,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    let reply_created = fs
        .create(req, 1, OsStr::new("lock_test_blocking.bin"), 0o644, 0)
        .await
        .expect("Create file should succeed");
    let ino = reply_created.attr.ino;

    // Process A locks range [0, 100]
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
    .expect("Lock A should succeed");

    // Process B tries to acquire lock on range [50, 150] in blocking mode
    let start_time = std::time::Instant::now();
    let res = fs
        .setlk(
            req,
            ino,
            102,
            1002,
            50,
            150,
            libc::F_WRLCK as u32,
            5678,
            true,
        )
        .await;

    assert!(
        res.is_err(),
        "Blocking lock on held range should fail after timeout"
    );
    let elapsed = start_time.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1500),
        "Should block/retry for at least 1.5 seconds before failing"
    );
    assert!(
        elapsed <= Duration::from_millis(2500),
        "Should fail-fast within ~2 seconds"
    );
}
