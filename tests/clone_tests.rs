use fuse3::raw::{prelude::*, Request};
use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::routing::DataRouter;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn cleanup_keys(src: &str, dest: &str) {
    let redis_url = get_redis_url();
    let client = match redis::Client::open(redis_url) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut con = match client.get_multiplexed_tokio_connection().await {
        Ok(conn) => conn,
        Err(_) => return,
    };

    let src_meta = format!("metadata:{}", src);
    let dest_meta = format!("metadata:{}", dest);

    // Clean up striped block maps and refcounts if they exist
    if let Ok(Some(id)) = con
        .hget::<_, _, Option<String>>(&src_meta, "block_map_id")
        .await
    {
        let map_key = format!("block_map:{}", id);
        if let Ok(mappings) = con
            .hgetall::<_, std::collections::HashMap<String, String>>(&map_key)
            .await
        {
            for (_, bk) in mappings {
                let _: () = con
                    .hdel("squeezefs:block_refcounts", bk)
                    .await
                    .unwrap_or(());
            }
        }
        let _: () = con.del(&map_key).await.unwrap_or(());
    }

    if let Ok(Some(id)) = con
        .hget::<_, _, Option<String>>(&dest_meta, "block_map_id")
        .await
    {
        let map_key = format!("block_map:{}", id);
        if let Ok(mappings) = con
            .hgetall::<_, std::collections::HashMap<String, String>>(&map_key)
            .await
        {
            for (_, bk) in mappings {
                let _: () = con
                    .hdel("squeezefs:block_refcounts", bk)
                    .await
                    .unwrap_or(());
            }
        }
        let _: () = con.del(&map_key).await.unwrap_or(());
    }

    let src_ino: Option<u64> = con.hget("squeezefs:dir:1", src).await.unwrap_or(None);
    let dest_ino: Option<u64> = con.hget("squeezefs:dir:1", dest).await.unwrap_or(None);

    let mut pipe = redis::pipe();
    if let Some(ino) = src_ino {
        pipe.del(format!("squeezefs:attr:{}", ino));
    }
    if let Some(ino) = dest_ino {
        pipe.del(format!("squeezefs:attr:{}", ino));
    }
    pipe.del(&src_meta)
        .del(&dest_meta)
        .del(format!("inline_data:{}", src))
        .del(format!("inline_data:{}", dest))
        .del(format!("mapping:{}", src))
        .del(format!("mapping:{}", dest))
        .hdel("squeezefs:dir:1", src)
        .hdel("squeezefs:dir:1", dest);
    let _: () = pipe.query_async(&mut con).await.unwrap_or(());
}

async fn is_db_available() -> bool {
    let redis_url = get_redis_url();
    let client = match redis::Client::open(redis_url) {
        Ok(c) => c,
        Err(_) => return false,
    };
    client.get_multiplexed_tokio_connection().await.is_ok()
}

async fn setup_router() -> Option<(DataRouter, tempfile::TempDir)> {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).ok()?;

    let mut con = match redis::Client::open(redis_url.clone())
        .ok()?
        .get_multiplexed_tokio_connection()
        .await
    {
        Ok(c) => c,
        Err(_) => return None,
    };
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
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .ok()?;

    Some((DataRouter::new(dlm, backend, cache), temp_dir))
}

#[tokio::test]
async fn test_clone_inline() {
    let src = "src_inline.bin";
    let dest = "dest_inline.bin";
    cleanup_keys(src, dest).await;

    let (router, _temp_dir) = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let src_data = vec![7; 1024]; // 1KB

    // Write src
    router.write_file(src, 0, &src_data, 201).await.unwrap();

    // Clone
    router
        .clone_file(src, dest)
        .await
        .expect("Should clone inline file");

    // Verify same size and content
    assert_eq!(router.get_file_size(dest).await.unwrap(), 1024);
    assert_eq!(router.read_file(dest).await.unwrap(), src_data);

    // Modify dest
    let patch = vec![3; 512];
    router.write_file(dest, 256, &patch, 202).await.unwrap();

    // Verify dest modified, src unchanged
    let dest_data = router.read_file(dest).await.unwrap();
    let read_src = router.read_file(src).await.unwrap();

    assert_eq!(read_src, src_data); // unchanged
    assert_eq!(dest_data[256..768], patch);
}

#[tokio::test]
async fn test_clone_staged() {
    let src = "src_staged.bin";
    let dest = "dest_staged.bin";
    cleanup_keys(src, dest).await;

    let (router, _temp_dir) = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let src_data = vec![5; 128 * 1024]; // 128KB

    // Write src
    router.write_file(src, 0, &src_data, 301).await.unwrap();

    // Clone
    router
        .clone_file(src, dest)
        .await
        .expect("Should clone staged file");

    // Verify same size and content
    assert_eq!(router.get_file_size(dest).await.unwrap(), 128 * 1024);
    assert_eq!(router.read_file(dest).await.unwrap(), src_data);

    // Modify dest
    let patch = vec![1; 1024];
    router
        .write_file(dest, 64 * 1024, &patch, 302)
        .await
        .unwrap();

    // Verify dest modified, src unchanged
    let dest_data = router.read_file(dest).await.unwrap();
    let read_src = router.read_file(src).await.unwrap();

    assert_eq!(read_src, src_data); // unchanged
    assert_eq!(dest_data[64 * 1024..(64 * 1024 + 1024)], patch);
}

#[tokio::test]
async fn test_clone_striped_cow() {
    let src = "src_striped.bin";
    let dest = "dest_striped.bin";
    cleanup_keys(src, dest).await;

    let (router, _temp_dir) = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let src_data = vec![2; 5 * 1024 * 1024]; // 5MB (2 blocks: 4MB + 1MB)

    // Write src
    router.write_file(src, 0, &src_data, 401).await.unwrap();

    // Clone
    router
        .clone_file(src, dest)
        .await
        .expect("Should clone striped file");

    // Verify same size and content
    assert_eq!(router.get_file_size(dest).await.unwrap(), 5 * 1024 * 1024);
    assert_eq!(router.read_file(dest).await.unwrap(), src_data);

    // Modify dest at block 0 (offset 1MB)
    let patch = vec![9; 1024];
    router
        .write_file(dest, 1024 * 1024, &patch, 402)
        .await
        .unwrap();

    // Verify dest modified, src unchanged (COW success)
    let dest_data = router.read_file(dest).await.unwrap();
    let read_src = router.read_file(src).await.unwrap();

    assert_eq!(read_src, src_data); // unchanged
    assert_eq!(dest_data[1024 * 1024..(1024 * 1024 + 1024)], patch);
}

#[tokio::test]
async fn test_copy_file_range_refclone() {
    let _ = env_logger::builder().is_test(true).try_init();
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return;
    }

    let src = "src_range.bin";
    let dest = "dest_range.bin";
    cleanup_keys(src, dest).await;

    let redis_url = get_redis_url();
    let mut con = redis::Client::open(redis_url.clone())
        .unwrap()
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());

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
    let fs = squeezefs::fuse_client::SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let req = Request {
        unique: 501,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    fs.init(req).await.unwrap();

    // 1. Create source file
    let reply_src = fs
        .create(req, 1, std::ffi::OsStr::new(src), 0o644, 0)
        .await
        .unwrap();
    let src_ino = reply_src.attr.ino;

    // 2. Create dest file
    let reply_dest = fs
        .create(req, 1, std::ffi::OsStr::new(dest), 0o644, 0)
        .await
        .unwrap();
    let dest_ino = reply_dest.attr.ino;

    // 3. Write data to source file
    let src_data = vec![4u8; 100 * 1024]; // 100KB
    fs.write(req, src_ino, 101, 0, &src_data, 0, 0)
        .await
        .unwrap();
    fs.flush(req, src_ino, 0, 0).await.unwrap();

    // Release the lease so copy_file_range can lock the file
    fs.release(req, src_ino, 101, 0, 0, false).await.unwrap();

    // 4. Call copy_file_range (whole file clone)
    let reply_copy = fs
        .copy_file_range(req, src_ino, 101, 0, dest_ino, 102, 0, 100 * 1024, 0)
        .await
        .unwrap();
    assert_eq!(reply_copy.copied, 100 * 1024);

    // 5. Read back from dest and verify content
    let read_reply = fs.read(req, dest_ino, 102, 0, 100 * 1024).await.unwrap();
    assert_eq!(read_reply.data.as_ref(), &src_data);
}

#[tokio::test]
async fn test_clone_path_full() {
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return;
    }

    let src = "src_path_clone.bin";
    let dest = "dest_path_clone.bin";
    cleanup_keys(src, dest).await;

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
    let fs = squeezefs::fuse_client::SqueezefsFilesystem::new(router.clone(), dlm, 1000, 1000);

    let req = Request {
        unique: 601,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    fs.init(req).await.unwrap();

    // 1. Create source file inside the FUSE mount namespace (parent root inode 1)
    let reply_src = fs
        .create(req, 1, std::ffi::OsStr::new(src), 0o644, 0)
        .await
        .unwrap();
    let src_ino = reply_src.attr.ino;

    // 2. Write data to source file
    let src_data = vec![6u8; 128 * 1024]; // 128KB
    fs.write(req, src_ino, 201, 0, &src_data, 0, 0)
        .await
        .unwrap();
    fs.flush(req, src_ino, 0, 0).await.unwrap();

    // Release the lease so clone_path can lock the file
    fs.release(req, src_ino, 201, 0, 0, false).await.unwrap();

    // 3. Call clone_path using logical paths
    router
        .clone_path(src, dest)
        .await
        .expect("clone_path should succeed");

    // 4. Verify dest file exists in directory root hash
    let mut con = redis::Client::open(redis_url)
        .unwrap()
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let dest_ino: u64 = con.hget("squeezefs:dir:1", dest).await.unwrap();
    assert!(dest_ino > 1);

    // 5. Read dest file back via FUSE client and verify size & content
    let read_reply = fs.read(req, dest_ino, 202, 0, 128 * 1024).await.unwrap();
    assert_eq!(read_reply.data.as_ref(), &src_data);
}
