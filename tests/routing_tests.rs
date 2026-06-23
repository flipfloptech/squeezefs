use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::routing::DataRouter;
use std::time::Duration;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn setup_router() -> Option<(DataRouter, tempfile::TempDir)> {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).ok()?;

    // Check if redis connection works and flush DB
    let client = redis::Client::open(redis_url.clone()).ok()?;
    let mut con = client.get_multiplexed_tokio_connection().await.ok()?;
    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());

    // Format the volume to initialize metadata
    let _ = squeezefs::fuse_client::format_volume(
        &redis_url,
        "routing_test_vol",
        4 * 1024 * 1024,
        100 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        None,
        None,
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
    .await;

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

    Some((DataRouter::new(dlm, backend, cache), temp_dir))
}

#[tokio::test]
async fn test_route_micro_file_inline() {
    let (router, _temp_dir) = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let file_path = "micro_file.bin";
    let data = vec![9; 1024]; // 1KB (< 64KB)

    // Write file
    router
        .write_file(file_path, 0, &data, 101)
        .await
        .expect("Should write micro file");

    // Read size
    let size = router
        .get_file_size(file_path)
        .await
        .expect("Should read size");
    assert_eq!(size, 1024);

    // Read back
    let read_data = router
        .read_file(file_path)
        .await
        .expect("Should read micro file");
    assert_eq!(read_data, data);
}

#[tokio::test]
async fn test_route_small_file_staged() {
    let (router, _temp_dir) = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let file_path = "small_file.bin";
    let data = vec![5; 128 * 1024]; // 128KB (between 64KB and 4MB)

    // Write file
    router
        .write_file(file_path, 0, &data, 102)
        .await
        .expect("Should write small file");

    // Verify staged file is readable locally
    let size = router
        .get_file_size(file_path)
        .await
        .expect("Should read size");
    assert_eq!(size, 128 * 1024);

    let read_data = router
        .read_file(file_path)
        .await
        .expect("Should read small file");
    assert_eq!(read_data, data);

    // Wait for NVMe staging merge flusher to run (timeout is 500ms, wait 800ms)
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Read back again (now from merged S3 backend block)
    let read_data_post_merge = router
        .read_file(file_path)
        .await
        .expect("Should read small file post-merge");
    assert_eq!(read_data_post_merge, data);

    // Verify physical storage write to S3 mock/real backend
    let client = redis::Client::open(get_redis_url()).unwrap();
    let mut con = client.get_multiplexed_tokio_connection().await.unwrap();
    let meta_key = format!("metadata:{}", file_path);
    let file_id: String = con.hget(&meta_key, "file_id").await.unwrap();
    let mapping_key = format!("mapping:{}", file_id);
    let block_key: Option<String> = con.hget(&mapping_key, "block").await.unwrap();
    assert!(block_key.is_some());
    let bk = block_key.unwrap();
    let (be_id, real_key) = squeezefs::backend::parse_backend_and_key(&bk);
    let block_data = router
        .backend
        .get_object(&be_id, &real_key)
        .await
        .expect("Merged block must exist in storage");
    assert!(!block_data.is_empty());
}

#[tokio::test]
async fn test_route_large_file_striped() {
    let (router, _temp_dir) = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let file_path = "large_file.bin";
    // 5MB (> 4MB block size limit)
    let data = vec![3; 5 * 1024 * 1024];

    // Write file
    router
        .write_file(file_path, 0, &data, 103)
        .await
        .expect("Should write large file");

    // Verify size
    let size = router
        .get_file_size(file_path)
        .await
        .expect("Should read size");
    assert_eq!(size, 5 * 1024 * 1024);

    // Read back in parallel
    let read_data = router
        .read_file(file_path)
        .await
        .expect("Should read large file");
    assert_eq!(read_data, data);

    // Verify physical storage writes to S3 mock/real backend
    let client = redis::Client::open(get_redis_url()).unwrap();
    let mut con = client.get_multiplexed_tokio_connection().await.unwrap();
    let meta_key = format!("metadata:{}", file_path);
    let block_map_id: Option<String> = con.hget(&meta_key, "block_map_id").await.unwrap();
    assert!(block_map_id.is_some());
    let map_key = format!("block_map:{}", block_map_id.unwrap());
    let block_keys: Vec<String> = con.hvals(&map_key).await.unwrap();
    assert!(!block_keys.is_empty());
    for bk in &block_keys {
        let (be_id, real_key) = squeezefs::backend::parse_backend_and_key(bk);
        let block_data = router
            .backend
            .get_object(&be_id, &real_key)
            .await
            .expect("Block must exist in storage");
        assert!(!block_data.is_empty());
    }
}

#[tokio::test]
async fn test_write_fallback_on_cache_full() {
    let redis_url = get_redis_url();
    let dlm = match DlmClient::new(&redis_url) {
        Ok(d) => d,
        Err(_) => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };
    // Flush db to get a clean slate
    let client = redis::Client::open(redis_url.clone()).unwrap();
    let mut con = client.get_multiplexed_tokio_connection().await.unwrap();
    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());

    // Format volume to initialize metadata settings
    let _ = squeezefs::fuse_client::format_volume(
        &redis_url,
        "fallback_test_vol",
        4 * 1024 * 1024,
        100 * 1024 * 1024 * 1024,
        0,
        "none",
        "none",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some("10KB"),
        Some("10KB"),
        None,
        None,
    )
    .await;

    let backend = RustFsClient::new_mock();
    let temp_dir = tempdir().unwrap();

    // 10KB write staging limit, 10KB read cache limit
    let cache = TieredCache::new(
        vec![temp_dir.path().to_path_buf()],
        None,
        None,
        Some("10KB"),
        Some("10KB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm, backend, cache);

    // 1. Write of 128KB (exceeds 10KB write capacity, triggers direct S3 upload fallback)
    let file1 = "file1.bin";
    let data1 = vec![1u8; 128 * 1024];
    router
        .write_file(file1, 0, &data1, 100)
        .await
        .expect("Write should succeed via direct fallback");

    // 2. Read back and verify size of file1
    let size1 = router.get_file_size(file1).await.expect("Should read size");
    assert_eq!(size1, 128 * 1024);

    let read_data1 = router.read_file(file1).await.expect("Should read file1");
    assert_eq!(read_data1, data1);

    // 3. Verify in Garnet that file1 has a direct-block mapping
    let client = redis::Client::open(redis_url).unwrap();
    let mut con = client.get_multiplexed_tokio_connection().await.unwrap();
    let meta_key = format!("metadata:{}", file1);
    let file_id: String = con.hget(&meta_key, "file_id").await.unwrap();
    let mapping_key = format!("mapping:{}", file_id);
    let block: String = con.hget(&mapping_key, "block").await.unwrap();
    assert!(
        block.contains("blocks/direct/"),
        "Block path '{}' should contain 'blocks/direct/'",
        block
    );
}
