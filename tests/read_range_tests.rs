use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::METRICS;
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn setup_router() -> Option<(DataRouter, tempfile::TempDir)> {
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
async fn test_block_level_read_range_striped() {
    let (router, _temp_dir) = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let file_path = "striped_range_test.bin";

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
        .write_file(file_path, 0, &data, 301)
        .await
        .expect("Should write large file");

    // Clear System RAM cache first to force reading from S3/NVMe Block Cache
    router.cache().lru.remove(file_path);

    // Record initial metrics
    let hits_before = METRICS.cache_hits.load(Ordering::Relaxed);
    let s3_gets_before = METRICS.get_obj.load(Ordering::Relaxed);

    // 1. Read a range in block 0: offset = 1MB, size = 100KB
    let range_offset = 1024 * 1024;
    let range_size = 100 * 1024;
    let read_range = router
        .read_file_range(file_path, range_offset, range_size)
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
    let read_range_again = router
        .read_file_range(file_path, range_offset, range_size)
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
