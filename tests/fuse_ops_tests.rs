use fuse3::raw::Filesystem;
use fuse3::raw::Request;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
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
async fn test_fuse_create_returns_zero_flags() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
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

    // 1. Create file via FUSE with O_CREAT | O_EXCL (0301 octal = 193)
    let file_name = OsStr::new("create_flags_test.swp");
    let flags = libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY;
    let reply_create = fs
        .create(req, 1, file_name, 0o644, flags as u32)
        .await
        .unwrap();

    // IMPORTANT: The FUSE reply flags should NOT be the POSIX flags!
    // They should be FOPEN_* flags. Default is 0.
    assert_eq!(
        reply_create.flags, 0,
        "FUSE create should not echo POSIX open flags back to kernel"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fuse_open_and_opendir_success() {
    let _con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
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

    // 1. Create file and dir via mknod / mkdir to simulate existing
    let file_name = OsStr::new("open_test.txt");
    let reply_mknod = fs
        .mknod(req, 1, file_name, 0o644 | libc::S_IFREG, 0)
        .await
        .unwrap();
    let file_ino = reply_mknod.attr.ino;

    let dir_name = OsStr::new("open_test_dir");
    let reply_mkdir = fs.mkdir(req, 1, dir_name, 0o755, 0).await.unwrap();
    let dir_ino = reply_mkdir.attr.ino;

    // 2. Open file
    let reply_open = fs.open(req, file_ino, libc::O_RDWR as u32).await.unwrap();
    assert_eq!(reply_open.fh, file_ino);
    assert_eq!(reply_open.flags, 0);

    // 3. Open directory
    let reply_opendir = fs
        .opendir(req, dir_ino, libc::O_RDONLY as u32)
        .await
        .unwrap();
    assert_eq!(reply_opendir.fh, dir_ino);
    assert_eq!(reply_opendir.flags, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fuse_setattr_truncation_clears_data() {
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

    let file_name = OsStr::new("truncate_test.txt");
    let reply_create = fs.create(req, 1, file_name, 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;
    let fh = reply_create.fh;

    // Write some data
    let write_data = vec![0xAAu8; 100];
    fs.write(req, ino, fh, 0, &write_data, 0, 0).await.unwrap();
    fs.fsync(req, ino, fh, false).await.unwrap();

    // Verify it is there
    let read_reply = fs.read(req, ino, fh, 0, 100).await.unwrap();
    assert_eq!(read_reply.data.len(), 100);

    // Truncate to 0 bytes
    use fuse3::SetAttr;
    let set_attr = SetAttr {
        size: Some(0), // Truncate to 0
        ..Default::default()
    };
    fs.setattr(req, ino, None, set_attr).await.unwrap();

    // Write again but less data
    let new_write_data = vec![0xBBu8; 10];
    fs.write(req, ino, fh, 0, &new_write_data, 0, 0)
        .await
        .unwrap();
    fs.fsync(req, ino, fh, false).await.unwrap();

    // Read it back. It should be only 10 bytes, NOT 100 bytes!
    let read_reply2 = fs.read(req, ino, fh, 0, 100).await.unwrap();
    assert_eq!(
        read_reply2.data.len(),
        10,
        "Truncation did not clear old data!"
    );
    assert_eq!(&read_reply2.data[..], &new_write_data[..]);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fuse_fallocate_fsyncdir_forget() {
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

    let file_name = OsStr::new("misc_ops_test.txt");
    let reply_create = fs.create(req, 1, file_name, 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;
    let fh = reply_create.fh;

    // fallocate - should succeed and do nothing or return Ok
    let res = fs.fallocate(req, ino, fh, 0, 1024, 0).await;
    assert!(res.is_ok(), "fallocate should be implemented and return Ok");

    // fsyncdir - should succeed and do nothing
    let res_fsync = fs.fsyncdir(req, 1, 1, false).await;
    assert!(
        res_fsync.is_ok(),
        "fsyncdir should be implemented and return Ok"
    );

    // forget - should not crash
    fs.forget(req, ino, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fuse_xattr() {
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

    let file_name = OsStr::new("xattr_test.txt");
    let reply_create = fs.create(req, 1, file_name, 0o644, 0).await.unwrap();
    let ino = reply_create.attr.ino;

    // Set xattr
    let xattr_name = OsStr::new("user.test_attr");
    let xattr_val = b"hello_xattr";
    fs.setxattr(req, ino, xattr_name, xattr_val, 0, 0)
        .await
        .unwrap();

    // Get xattr size
    let reply_size = fs.getxattr(req, ino, xattr_name, 0).await.unwrap();
    match reply_size {
        fuse3::raw::reply::ReplyXAttr::Size(s) => assert_eq!(s as usize, xattr_val.len()),
        _ => panic!("Expected ReplyXAttr::Size"),
    }

    // Get xattr data
    let reply_data = fs
        .getxattr(req, ino, xattr_name, xattr_val.len() as u32)
        .await
        .unwrap();
    match reply_data {
        fuse3::raw::reply::ReplyXAttr::Data(d) => assert_eq!(&d[..], xattr_val),
        _ => panic!("Expected ReplyXAttr::Data"),
    }

    // List xattr
    let reply_list = fs.listxattr(req, ino, 0).await.unwrap();
    match reply_list {
        fuse3::raw::reply::ReplyXAttr::Size(s) => assert!(s > 0),
        _ => panic!("Expected ReplyXAttr::Size"),
    }
    // Remove xattr
    fs.removexattr(req, ino, xattr_name).await.unwrap();

    // Verify it is gone
    let reply_gone = fs.getxattr(req, ino, xattr_name, 0).await;
    assert!(reply_gone.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_vim_swap_lifecycle() {
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

    let file_name = OsStr::new(".test.txt.swp");

    // 1. Create with O_EXCL should succeed atomically
    let flags = libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY;
    let reply_create = fs
        .create(req, 1, file_name, 0o644, flags as u32)
        .await
        .unwrap();
    let ino = reply_create.attr.ino;
    let fh = reply_create.fh;

    // 2. Second create with O_EXCL should fail with EEXIST
    let reply_create2 = fs.create(req, 1, file_name, 0o644, flags as u32).await;
    match reply_create2 {
        Err(e) if e.is_exist() => {}
        _ => panic!(
            "Expected EEXIST for second O_EXCL create, got {:?}",
            reply_create2
        ),
    }

    // 3. setlk should succeed via the DLM for .swp files
    let setlk_res = fs
        .setlk(req, ino, fh, 123, 0, 100, libc::F_WRLCK as u32, 1234, false)
        .await;
    assert!(setlk_res.is_ok(), "setlk should succeed via DLM");

    // 4. fsync should succeed silently
    let fsync_res = fs.fsync(req, ino, fh, false).await;
    assert!(fsync_res.is_ok(), "fsync should return Ok");

    // 5. flush should succeed silently
    let flush_res = fs.flush(req, ino, fh, 123).await;
    assert!(flush_res.is_ok(), "flush should return Ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fuse_write_updates_blocks_cache() {
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

    let file_name = OsStr::new("write_blocks_test.bin");
    let flags = libc::O_CREAT | libc::O_RDWR;
    let reply_create = fs
        .create(req, 1, file_name, 0o644, flags as u32)
        .await
        .unwrap();
    let ino = reply_create.attr.ino;
    let fh = reply_create.fh;

    // Verify initial state
    let attr_pre = fs.getattr(req, ino, None, 0).await.unwrap().attr;
    assert_eq!(attr_pre.size, 0);
    assert_eq!(attr_pre.blocks, 0);

    // Write 1000 bytes
    let data = vec![0u8; 1000];
    let reply_write = fs.write(req, ino, fh, 0, &data, 0, 0).await.unwrap();
    assert_eq!(reply_write.written, 1000);

    // Call getattr immediately (should hit cached attributes)
    let attr_post = fs.getattr(req, ino, None, 0).await.unwrap().attr;
    assert_eq!(attr_post.size, 1000);
    assert_eq!(
        attr_post.blocks, 2,
        "Blocks attribute in cache must be updated to size.div_ceil(512)"
    );
}
