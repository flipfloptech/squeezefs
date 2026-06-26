use fuse3::raw::{Filesystem, Request};
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

async fn setup_two_fs() -> Option<(
    SqueezefsFilesystem,
    SqueezefsFilesystem,
    tempfile::TempDir,
    tempfile::TempDir,
)> {
    let redis_url = get_redis_url();
    let dlm1 = DlmClient::new(&redis_url).ok()?;
    let dlm2 = DlmClient::new(&redis_url).ok()?;

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
    let temp_dir1 = tempdir().unwrap();
    let temp_dir2 = tempdir().unwrap();

    let cache1 = TieredCache::new(
        vec![temp_dir1.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm1.meta_client().clone(),
    )
    .ok()?;

    let cache2 = TieredCache::new(
        vec![temp_dir2.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm2.meta_client().clone(),
    )
    .ok()?;

    let router1 = DataRouter::new(dlm1.clone(), backend.clone(), cache1);
    let fs1 = SqueezefsFilesystem::new(router1, dlm1, 1000, 1000);

    let router2 = DataRouter::new(dlm2.clone(), backend, cache2);
    let fs2 = SqueezefsFilesystem::new(router2, dlm2, 1000, 1000);

    // Call init on fs1
    let req1 = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    let _ = fs1.init(req1).await.ok()?;

    // Call init on fs2
    let req2 = Request {
        unique: 2,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    let _ = fs2.init(req2).await.ok()?;

    Some((fs1, fs2, temp_dir1, temp_dir2))
}

#[tokio::test]
async fn test_lock_delegation_local_happy_path() {
    let (fs1, _fs2, _t1, _t2) = match setup_two_fs().await {
        Some(res) => res,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };

    let req = Request {
        unique: 10,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    // 1. Create a file on fs1
    let reply_created = fs1
        .create(req, 1, OsStr::new("delegation_happy.bin"), 0o644, 0)
        .await
        .expect("Create file should succeed");
    let ino = reply_created.attr.ino;

    // Verify initially no delegation
    assert!(!fs1.has_delegation(ino));

    // 2. Process A locks range [0, 100] on fs1
    let owner = 1001;
    fs1.setlk(
        req,
        ino,
        101,
        owner,
        0,
        100,
        libc::F_WRLCK as u32,
        1234,
        false,
    )
    .await
    .expect("Lock range [0, 100] should succeed");

    // 3. Verify delegation is held and lock is local
    assert!(fs1.has_delegation(ino));
    assert!(fs1.has_local_posix_lock(ino, owner, 0, 100));

    // 4. Verify no global lock key is present in Redis directly
    let redis_url = get_redis_url();
    let mut con = redis::Client::open(redis_url)
        .unwrap()
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let lock_key = format!("lock:inode_{}:range:0-100", ino);
    let exists: bool = redis::cmd("EXISTS")
        .arg(&lock_key)
        .query_async(&mut con)
        .await
        .unwrap();
    assert!(!exists, "Lock should be purely local, not in Redis");

    // 5. Unlock the range on fs1
    fs1.setlk(
        req,
        ino,
        101,
        owner,
        0,
        100,
        libc::F_UNLCK as u32,
        1234,
        false,
    )
    .await
    .expect("Unlock range should succeed");

    // 6. Verify lock is gone, but delegation is still held locally (prevent thrashing)
    assert!(!fs1.has_local_posix_lock(ino, owner, 0, 100));
    assert!(fs1.has_delegation(ino));
}

#[tokio::test]
async fn test_lock_delegation_recall_conflict() {
    let (fs1, fs2, _t1, _t2) = match setup_two_fs().await {
        Some(res) => res,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };

    let req1 = Request {
        unique: 10,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    let req2 = Request {
        unique: 20,
        uid: 1000,
        gid: 1000,
        pid: 5678,
    };

    // 1. Create a file on fs1
    let reply_created = fs1
        .create(req1, 1, OsStr::new("delegation_recall.bin"), 0o644, 0)
        .await
        .expect("Create file should succeed");
    let ino = reply_created.attr.ino;

    // 2. Lock range [0, 100] on fs1
    let owner1 = 1001;
    fs1.setlk(
        req1,
        ino,
        101,
        owner1,
        0,
        100,
        libc::F_WRLCK as u32,
        1234,
        false,
    )
    .await
    .expect("Lock range on fs1 should succeed");

    assert!(fs1.has_delegation(ino));
    assert!(fs1.has_local_posix_lock(ino, owner1, 0, 100));

    // 3. Try to acquire conflicting lock range [50, 150] on fs2
    let owner2 = 1002;
    let res = fs2
        .setlk(
            req2,
            ino,
            201,
            owner2,
            50,
            150,
            libc::F_WRLCK as u32,
            5678,
            false,
        )
        .await;

    // 4. Because range [50, 150] overlaps [0, 100], and the lock from fs1 is flushed,
    // fs2 should fail with EAGAIN.
    assert!(res.is_err(), "Conflicting lock should fail");

    // Give it a brief moment for the recall pub/sub and lock flushing/delegation release to complete
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 5. Verify delegation states
    assert!(
        !fs1.has_delegation(ino),
        "fs1 should have yielded the delegation"
    );
    assert!(
        fs2.has_delegation(ino),
        "fs2 should now hold the delegation"
    );

    // 6. Verify fs1 lock was flushed/promoted to global
    assert!(fs1.has_global_posix_lock(ino, owner1, 0, 100));

    // 7. Verify fs2 has loaded the lock from Redis as remote
    assert!(fs2.has_remote_posix_lock(ino, u64::MAX, 0, 100));
}
