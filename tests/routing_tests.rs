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
}
