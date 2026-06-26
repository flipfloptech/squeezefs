use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::routing::DataRouter;
use std::time::Duration;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_prefetch_io_uring_page_prefetch_triggered() {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
    let backend = RustFsClient::new_mock();

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

    let router = DataRouter::new(dlm, backend.clone(), cache);
    let block_size = 1024u64;
    router.set_block_size(block_size);

    let file_path = "io_uring_prefetch_test";
    let block_map_id = format!("prefetch-map-{}", uuid::Uuid::new_v4());
    let total_blocks = 12u32;
    let mut expected_file_data = Vec::new();

    // Cache the blocks in the local NVMe cache so they are hits
    for block_idx in 0..total_blocks {
        let physical_block_key = format!("blocks/prefetch_io_uring/block_{}", block_idx);
        let stored_block_key = format!("backend_0:{}", physical_block_key);
        let block_data = vec![block_idx as u8; block_size as usize];

        // Cache it locally in NVMe cache
        router
            .cache
            .nvme
            .cache_read_block(&stored_block_key, &block_data)
            .unwrap();

        router.block_map_cache.insert(
            (block_map_id.clone(), block_idx),
            (Some(stored_block_key), std::time::Instant::now()),
        );
        expected_file_data.extend_from_slice(&block_data);
    }

    router.metadata_cache.insert(
        file_path.to_string(),
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

    // Read block 0, then block 1 sequentially. This should trigger prefetching on blocks 2 through 10.
    let first_block = router
        .read_file_range(file_path, 0, block_size as u32)
        .await
        .unwrap();
    assert_eq!(first_block, expected_file_data[..block_size as usize]);

    let second_block = router
        .read_file_range(file_path, block_size, block_size as u32)
        .await
        .unwrap();
    assert_eq!(
        second_block,
        expected_file_data[block_size as usize..(2 * block_size) as usize]
    );

    // Wait a brief moment for the prefetcher tasks to run
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Check that prefetch_count was incremented, meaning io_uring prefetch was successfully triggered!
    let count = router
        .prefetcher
        .prefetch_count
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        count > 0,
        "io_uring page prefetching should have been triggered"
    );
    println!("Verified: io_uring prefetch count is {}", count);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_prefetch_gds_aware_prefetch_triggered() {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).unwrap();
    let backend = RustFsClient::new_mock();

    let temp_dir = tempdir().unwrap();
    let mut cache = TieredCache::new(
        vec![temp_dir.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();

    // Enable GDS for testing
    cache.gds.force_available = true;

    let router = DataRouter::new(dlm, backend.clone(), cache);
    let block_size = 1024u64;
    router.set_block_size(block_size);

    let file_path = "gds_prefetch_test";
    let block_map_id = format!("prefetch-map-{}", uuid::Uuid::new_v4());
    let total_blocks = 12u32;
    let mut expected_file_data = Vec::new();

    for block_idx in 0..total_blocks {
        let physical_block_key = format!("blocks/prefetch_gds/block_{}", block_idx);
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
        file_path.to_string(),
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

    // Read block 0, then block 1 sequentially. This should trigger prefetching.
    let first_block = router
        .read_file_range(file_path, 0, block_size as u32)
        .await
        .unwrap();
    assert_eq!(first_block, expected_file_data[..block_size as usize]);

    let second_block = router
        .read_file_range(file_path, block_size, block_size as u32)
        .await
        .unwrap();
    assert_eq!(
        second_block,
        expected_file_data[block_size as usize..(2 * block_size) as usize]
    );

    // Wait a brief moment for the GDS prefetcher tasks to run and write files
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify that at least one .gds_cache file was successfully created in the staging directory!
    let mut gds_files_found = 0;
    let read_dir = std::fs::read_dir(temp_dir.path()).unwrap();
    for entry in read_dir {
        let entry = entry.unwrap();
        let path = entry.path();
        if let Some(ext) = path.extension() {
            if ext == "gds_cache" {
                gds_files_found += 1;
            }
        }
    }

    assert!(
        gds_files_found > 0,
        "GDS-aware prefetching should have written at least one .gds_cache file"
    );
    println!(
        "Verified: GDS-aware prefetching created {} .gds_cache files",
        gds_files_found
    );
}
