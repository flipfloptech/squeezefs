use fuse3::raw::Filesystem;
use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::routing::DataRouter;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn is_db_available() -> bool {
    let redis_url = get_redis_url();
    let client = match redis::Client::open(redis_url) {
        Ok(c) => c,
        Err(_) => return false,
    };
    client.get_multiplexed_tokio_connection().await.is_ok()
}

#[tokio::test]
async fn test_metadata_cache_effectiveness() {
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return;
    }

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
    let file_path = "perf_metadata_test.bin";

    // Write file (staged layout since it is 100KB)
    let file_size = 100 * 1024;
    let data = vec![65u8; file_size];
    router.write_file(file_path, 0, &data, 401).await.unwrap();

    // Clear System RAM data cache to force reading
    router.cache().write_lru.remove(file_path);
    router.cache().read_lru.remove(file_path);

    // 1. First read range (should fetch and cache metadata/mappings from Garnet)
    let read1 = router.read_file_range(file_path, 0, 1000).await.unwrap();
    assert_eq!(read1.len(), 1000);

    // Delete metadata and mapping keys from Garnet to prove cache isolation
    let mut con = dlm.get_connection().await.unwrap();
    let meta_key = format!("metadata:{}", file_path);
    let file_id: String = con.hget(&meta_key, "file_id").await.unwrap();
    let mapping_key = format!("mapping:{}", file_id);

    let _: () = con.del(&[&meta_key, &mapping_key]).await.unwrap();

    // 2. Second read range (should succeed using cached metadata/mappings)
    let read2 = router.read_file_range(file_path, 1000, 1000).await.unwrap();
    assert_eq!(read2.len(), 1000);
}

#[tokio::test]
async fn test_attr_cache_effectiveness() {
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return;
    }

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
    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let ino = 9999u64;
    let attr_key = format!("squeezefs:attr:{}", ino);
    let mut con = dlm.get_connection().await.unwrap();

    // Setup dummy attributes in Garnet
    let _: () = redis::pipe()
        .hset(&attr_key, "ino", ino)
        .hset(&attr_key, "size", 4096)
        .hset(&attr_key, "kind", 1)
        .hset(&attr_key, "perm", 0o644)
        .query_async(&mut con)
        .await
        .unwrap();

    // 1. Request attributes (should fetch and cache)
    let req = fuse3::raw::Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };
    let reply1 = fs.getattr(req, ino, None, 0).await.unwrap();
    assert_eq!(reply1.attr.size, 4096);

    // Delete attributes key from Garnet
    let _: () = con.del(&attr_key).await.unwrap();

    // 2. Request attributes again (should succeed using cached attributes)
    let reply2 = fs.getattr(req, ino, None, 0).await.unwrap();
    assert_eq!(reply2.attr.size, 4096);
}

#[tokio::test]
async fn test_get_cached_read_block_range() {
    let temp_dir = tempdir().unwrap();
    let dir_path = temp_dir.path().to_path_buf();

    // Write a dummy block file of 1MB
    let block_size = 1024 * 1024;
    let block_data: Vec<u8> = (0..block_size).map(|i| (i % 256) as u8).collect();

    let block_key = "test/block/key";
    let safe_name = block_key.replace('/', "_");
    let block_file_path = dir_path.join(format!("{}.block", safe_name));

    std::fs::write(&block_file_path, &block_data).unwrap();

    let backend = RustFsClient::new_mock();
    let redis_client = squeezefs::dlm::MetaClient::new("redis://127.0.0.1:6379").unwrap();

    let cache = TieredCache::new(
        vec![dir_path],
        None,
        None,
        None,
        None,
        backend,
        redis_client,
    )
    .unwrap();

    // Call get_cached_read_block_range
    let offset = 500000u64;
    let size = 10000u32;
    let res = cache
        .nvme
        .get_cached_read_block_range(block_key, offset, size)
        .unwrap();

    assert_eq!(res.len(), size as usize);
    assert_eq!(
        res,
        block_data[offset as usize..(offset + size as u64) as usize]
    );
}
