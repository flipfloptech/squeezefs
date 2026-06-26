use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::METRICS;
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

fn make_staging_dir(test_name: &str) -> std::path::PathBuf {
    let path = std::env::current_dir()
        .unwrap()
        .join("target")
        .join("test-staging")
        .join(format!("{}-{}", test_name, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn make_unique_file_name(prefix: &str) -> String {
    format!("{}-{}.bin", prefix, uuid::Uuid::new_v4())
}

async fn setup_router() -> Option<DataRouter> {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).ok()?;

    // Check if redis connection works
    let con_res = redis::Client::open(redis_url.clone())
        .ok()?
        .get_multiplexed_tokio_connection()
        .await;
    if con_res.is_err() {
        return None;
    }

    let backend = RustFsClient::new().await;
    let staging_path = make_staging_dir("read-range-online");
    let cache = TieredCache::new(
        vec![staging_path],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .ok()?;

    Some(DataRouter::new(dlm, backend, cache))
}

#[tokio::test]
async fn test_block_level_read_range_striped() {
    let router = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let file_path = make_unique_file_name("striped_range_test");

    // We write a large file > 4MB (e.g. 5MB) so it has exactly 2 blocks (Block 0 = 4MB, Block 1 = 1MB)
    let block_size = 4 * 1024 * 1024;
    let file_size = 5 * 1024 * 1024;

    let mut data = vec![0u8; file_size];
    // Fill block 0 and block 1 with distinct values to verify correctness
    for (i, val) in data.iter_mut().enumerate() {
        if i < block_size {
            *val = (i % 256) as u8;
        } else {
            *val = ((i - block_size) % 256) as u8;
        }
    }

    // Write file. This will merge/flush it to striped layout because it is > 4MB
    router
        .write_file(&file_path, 0, &data, 301)
        .await
        .expect("Should write large file");

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

    // Wait a brief moment to ensure any asynchronous cache write tasks from write_file have finished writing to NVMe
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Clear System RAM cache and NVMe block cache to force reading from S3
    router.cache().write_lru.remove(&file_path);
    router.cache().read_lru.remove(&file_path);
    for bk in &block_keys {
        router.cache().read_lru.remove(bk);
        let safe_name = bk.replace(['/', ':'], "_");
        for dir in router.cache().nvme.staging_dirs() {
            let block_path = dir.join("cache").join(format!("block_{}.block", safe_name));
            if block_path.exists() {
                let _ = std::fs::remove_file(block_path);
            }
        }
    }

    // Record initial metrics
    let hits_before = METRICS.cache_hits.load(Ordering::Relaxed);
    let s3_gets_before = METRICS.get_obj.load(Ordering::Relaxed);

    // 1. Read a range in block 0: offset = 1MB, size = 100KB
    let range_offset = 1024 * 1024;
    let range_size = 100 * 1024;
    let read_range = router
        .read_file_range(&file_path, range_offset, range_size)
        .await
        .expect("Should read range from block 0");

    assert_eq!(
        read_range,
        data[range_offset as usize..(range_offset + range_size as u64) as usize]
    );

    // Check S3 GETs increased (since block 0 was downloaded)
    let s3_gets_after_first = METRICS.get_obj.load(Ordering::Relaxed);
    assert!(
        s3_gets_after_first > s3_gets_before,
        "Block 0 must be fetched from S3 on cache miss"
    );

    // 2. Perform the exact same read again (should hit local NVMe block cache)
    // Wait a brief moment to ensure asynchronous cache write has finished
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let read_range_again = router
        .read_file_range(&file_path, range_offset, range_size)
        .await
        .expect("Should read range from block 0 again");

    assert_eq!(read_range_again, read_range);

    let s3_gets_after_second = METRICS.get_obj.load(Ordering::Relaxed);
    assert_eq!(
        s3_gets_after_second, s3_gets_after_first,
        "Subsequent read must not issue new S3 GET requests"
    );

    let hits_after = METRICS.cache_hits.load(Ordering::Relaxed);
    assert!(
        hits_after > hits_before,
        "Block cache hit count must increase"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_cold_block_reads_are_singleflight() {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
    let backend = RustFsClient::new_mock();
    backend.set_mock_get_delay(Duration::from_millis(75));

    let staging_path = make_staging_dir("read-range-singleflight");
    let cache = TieredCache::new(
        vec![staging_path],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm, backend.clone(), cache);
    let file_path = make_unique_file_name("singleflight_range_test");
    let block_map_id = "singleflight_map".to_string();
    let physical_block_key = "blocks/singleflight/block_0";
    let stored_block_key = format!("backend_0:{}", physical_block_key);
    let block_data = bytes::Bytes::from(vec![0x5Au8; 256 * 1024]);

    backend
        .put_object(physical_block_key, block_data.clone(), 1)
        .await
        .unwrap();

    router.metadata_cache.insert(
        file_path.clone(),
        squeezefs::routing::CachedMetadata {
            file_type: "striped".to_string(),
            size: block_data.len() as u64,
            block_map_id: Some(block_map_id.clone()),
            block_prefix: None,
            file_id: None,
            cached_at: std::time::Instant::now(),
            data_key: None,
        },
    );
    router.block_map_cache.insert(
        (block_map_id, 0),
        (Some(stored_block_key.clone()), std::time::Instant::now()),
    );

    let concurrency = 16;
    let start = std::sync::Arc::new(tokio::sync::Barrier::new(concurrency + 1));
    let mut tasks = Vec::new();

    for _ in 0..concurrency {
        let router = router.clone();
        let start = start.clone();
        let expected = block_data.clone();
        let file_path = file_path.clone();
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            let read = router
                .read_file_range(&file_path, 0, expected.len() as u32)
                .await
                .unwrap();
            assert_eq!(read, expected);
        }));
    }

    start.wait().await;

    for task in tasks {
        task.await.unwrap();
    }

    assert_eq!(
        backend.mock_get_count(),
        1,
        "concurrent cold reads of the same block should collapse to one backend fetch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sequential_striped_reads_prefetch_upcoming_blocks() {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
    let backend = RustFsClient::new_mock();
    backend.set_mock_get_delay(Duration::from_millis(10));

    let staging_path = make_staging_dir("read-range-prefetch");
    let cache = TieredCache::new(
        vec![staging_path],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm, backend.clone(), cache);
    let block_size = 1024u64;
    router.set_block_size(block_size);

    let file_path = make_unique_file_name("prefetch_range_test");
    let block_map_id = format!("prefetch-map-{}", uuid::Uuid::new_v4());
    let total_blocks = 12u32;
    let mut expected_file_data = Vec::new();

    for block_idx in 0..total_blocks {
        let physical_block_key = format!("blocks/prefetch/block_{}", block_idx);
        let stored_block_key = format!("backend_0:{}", physical_block_key);
        let block_data = bytes::Bytes::from(vec![block_idx as u8; block_size as usize]);

        backend
            .put_object(&physical_block_key, block_data.clone(), 1)
            .await
            .unwrap();

        router.block_map_cache.insert(
            (block_map_id.clone(), block_idx),
            (Some(stored_block_key), std::time::Instant::now()),
        );
        expected_file_data.extend_from_slice(&block_data);
    }

    router.metadata_cache.insert(
        file_path.clone(),
        squeezefs::routing::CachedMetadata {
            file_type: "striped".to_string(),
            size: expected_file_data.len() as u64,
            block_map_id: Some(block_map_id),
            block_prefix: None,
            file_id: None,
            cached_at: std::time::Instant::now(),
            data_key: None,
        },
    );

    let first_block = router
        .read_file_range(&file_path, 0, block_size as u32)
        .await
        .unwrap();
    assert_eq!(first_block, expected_file_data[..block_size as usize]);

    let second_block = router
        .read_file_range(&file_path, block_size, block_size as u32)
        .await
        .unwrap();
    assert_eq!(
        second_block,
        expected_file_data[block_size as usize..(2 * block_size) as usize]
    );

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if backend.mock_get_count() >= 11 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sequential striped reads should trigger background prefetch");

    assert_eq!(
        backend.mock_get_count(),
        11,
        "two sequential block reads should prefetch blocks 2 through 10"
    );
}
