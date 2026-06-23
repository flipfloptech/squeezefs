use squeezefs::backend::RustFsClient;
use squeezefs::cache::lru::LruCache;
use squeezefs::cache::nvme::NvmeStaging;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn test_lru_eviction() {
    // Construct cache with max capacity 1000 bytes
    let cache = LruCache::with_capacity(1000);

    // Insert 40 blocks of 100 bytes (total 4000 bytes > 1000 bytes)
    for i in 0..40 {
        cache.put(&format!("key{}", i), std::sync::Arc::new(vec![i as u8; 100]));
    }
    cache.run_pending_tasks();

    // Verify cache size is within limits (moka evicts asynchronously but run_pending_tasks makes it synchronous)
    assert!(cache.current_bytes() <= 1000, "Cache size {} exceeded capacity 1000", cache.current_bytes());

    // Verify that at least some keys were evicted
    let mut present = 0;
    for i in 0..40 {
        if cache.get(&format!("key{}", i)).is_some() {
            present += 1;
        }
    }
    assert!(present < 40, "No keys were evicted");
    assert!(present > 0, "All keys were evicted");
}

#[tokio::test]
async fn test_nvme_staging_and_merge() {
    let temp_dir = tempdir().unwrap();
    let mock_backend = RustFsClient::new_mock();

    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    let nvme = NvmeStaging::new(
        vec![temp_dir.path().to_path_buf()],
        100 * 1024 * 1024,
        mock_backend.clone(),
        squeezefs::dlm::MetaClient::Single(redis_client),
    )
    .expect("Should construct NVMe staging");

    let file_id = "test-file-id-123";
    let data = vec![7; 1000];

    // Stage write (returns immediately)
    nvme.stage_write("file.txt", file_id, &data, 42)
        .await
        .expect("Should stage write successfully");

    // Immediately after write, the staged file MUST be readable locally from NVMe staging
    let staged_data = nvme
        .read_staged(file_id)
        .expect("Should find staged file locally");
    assert_eq!(staged_data, data);

    // Wait for the background worker to flush the batch (timeout is 500ms, let's wait 800ms)
    tokio::time::sleep(Duration::from_millis(800)).await;

    // After flush, the local file should be deleted (cleaned up)
    assert!(nvme.read_staged(file_id).is_none());
}

#[test]
fn test_cache_size_parser() {
    use squeezefs::cache::parse_size_string;

    // Test percentage of system/total
    let size = parse_size_string("50%", 1000).unwrap();
    assert_eq!(size, 500);

    // Test exact sizes
    assert_eq!(parse_size_string("100B", 1000).unwrap(), 100);
    assert_eq!(parse_size_string("10KB", 1000).unwrap(), 10240);
    assert_eq!(parse_size_string("5MB", 1000).unwrap(), 5 * 1024 * 1024);
    assert_eq!(
        parse_size_string("2GB", 1000).unwrap(),
        2 * 1024 * 1024 * 1024
    );

    // Test case insensitivity and spaces
    assert_eq!(parse_size_string("  1.5 gb  ", 1000).unwrap(), 1610612736);

    // Test errors
    assert!(parse_size_string("invalid", 1000).is_err());
}

#[tokio::test]
async fn test_multi_disk_distribution() {
    let dir1 = tempdir().unwrap();
    let dir2 = tempdir().unwrap();
    let dir3 = tempdir().unwrap();

    let staging_dirs = vec![
        dir1.path().to_path_buf(),
        dir2.path().to_path_buf(),
        dir3.path().to_path_buf(),
    ];

    let mock_backend = RustFsClient::new_mock();
    let redis_client = redis::Client::open("redis://127.0.0.1:6379").unwrap();

    // Max capacity 100MB
    let nvme = NvmeStaging::new(
        staging_dirs.clone(),
        100 * 1024 * 1024,
        mock_backend,
        squeezefs::dlm::MetaClient::Single(redis_client),
    )
    .expect("Should construct multi-disk NVMe staging");

    // Let's write 6 files and verify they are distributed
    for i in 0..6 {
        let file_id = format!("file-id-{}", i);
        let data = vec![i as u8; 100];
        nvme.stage_write(&format!("file_{}.txt", i), &file_id, &data, 100 + i as u64)
            .await
            .unwrap();

        // Verify read_staged can read it back successfully
        let read_data = nvme.read_staged(&file_id).unwrap();
        assert_eq!(read_data, data);
    }

    // Verify files were actually placed in the respective directories
    let mut total_files = 0;
    for dir in &staging_dirs {
        let entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|res| res.unwrap().path())
            .collect();
        for path in entries {
            if path.extension().is_some_and(|ext| ext == "staged") {
                total_files += 1;
            }
        }
    }
    assert_eq!(total_files, 6);
}
