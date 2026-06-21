use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::routing::DataRouter;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn setup_router() -> Option<(DataRouter, tempfile::TempDir)> {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).ok()?;

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
async fn test_clone_inline() {
    let (router, _temp_dir) = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let src = "src_inline.bin";
    let dest = "dest_inline.bin";
    let src_data = vec![7; 1024]; // 1KB

    // Write src
    router.write_file(src, 0, &src_data, 201).await.unwrap();

    // Clone
    router.clone_file(src, dest).await.expect("Should clone inline file");

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
    let (router, _temp_dir) = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let src = "src_staged.bin";
    let dest = "dest_staged.bin";
    let src_data = vec![5; 128 * 1024]; // 128KB

    // Write src
    router.write_file(src, 0, &src_data, 301).await.unwrap();

    // Clone
    router.clone_file(src, dest).await.expect("Should clone staged file");

    // Verify same size and content
    assert_eq!(router.get_file_size(dest).await.unwrap(), 128 * 1024);
    assert_eq!(router.read_file(dest).await.unwrap(), src_data);

    // Modify dest
    let patch = vec![1; 1024];
    router.write_file(dest, 64 * 1024, &patch, 302).await.unwrap();

    // Verify dest modified, src unchanged
    let dest_data = router.read_file(dest).await.unwrap();
    let read_src = router.read_file(src).await.unwrap();

    assert_eq!(read_src, src_data); // unchanged
    assert_eq!(dest_data[64 * 1024..(64 * 1024 + 1024)], patch);
}

#[tokio::test]
async fn test_clone_striped_cow() {
    let (router, _temp_dir) = match setup_router().await {
        Some(r) => r,
        None => {
            println!("Skipping test: Redis/Garnet or S3 not available");
            return;
        }
    };

    let src = "src_striped.bin";
    let dest = "dest_striped.bin";
    let src_data = vec![2; 5 * 1024 * 1024]; // 5MB (2 blocks: 4MB + 1MB)

    // Write src
    router.write_file(src, 0, &src_data, 401).await.unwrap();

    // Clone
    router.clone_file(src, dest).await.expect("Should clone striped file");

    // Verify same size and content
    assert_eq!(router.get_file_size(dest).await.unwrap(), 5 * 1024 * 1024);
    assert_eq!(router.read_file(dest).await.unwrap(), src_data);

    // Modify dest at block 0 (offset 1MB)
    let patch = vec![9; 1024];
    router.write_file(dest, 1024 * 1024, &patch, 402).await.unwrap();

    // Verify dest modified, src unchanged (COW success)
    let dest_data = router.read_file(dest).await.unwrap();
    let read_src = router.read_file(src).await.unwrap();

    assert_eq!(read_src, src_data); // unchanged
    assert_eq!(dest_data[1024 * 1024..(1024 * 1024 + 1024)], patch);
}
