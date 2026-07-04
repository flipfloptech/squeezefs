use squeezefs::dlm::MetaClient;
use squeezefs::error::Result;

#[test]
fn test_sentinel_url_parsing() {
    let sentinel_url = "redis-sentinel://127.0.0.1:26379,127.0.0.1:26380/mymaster";
    let meta_client = MetaClient::new(sentinel_url).unwrap();
    assert!(matches!(meta_client, MetaClient::Sentinel { .. }));
}

#[tokio::test]
async fn test_sentinel_connection_caching() -> Result<()> {
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return Ok(());
    }

    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = MetaClient::new(&redis_url)?;

    // 1. Get a valid multiplexed connection to Garnet/Redis
    let mut real_conn = client.get_connection().await?;
    match redis::cmd("PING")
        .query_async::<_, String>(&mut real_conn)
        .await
    {
        Ok(pong) => println!("PING real_conn succeeded: {}", pong),
        Err(e) => println!("PING real_conn failed: {:?}", e),
    }

    let conn_val = match &real_conn {
        squeezefs::dlm::MetaConnection::Single { conn, .. } => conn.clone(),
        _ => panic!("Expected MetaConnection::Single"),
    };

    // 2. Pre-populate SENTINEL_CONN_POOL with this valid connection under service name "fake_master"
    squeezefs::dlm::SENTINEL_CONN_POOL.insert("fake_master".to_string(), conn_val);

    // 3. Create a Sentinel MetaClient pointing to dummy sentinel servers but service name "fake_master"
    let dummy_sentinel_url = "redis-sentinel://127.0.0.1:26379,127.0.0.1:26380/fake_master";
    let sentinel_client = MetaClient::new(dummy_sentinel_url)?;

    // 4. Call get_connection on the sentinel client. It should HIT the cache and succeed!
    let mut sentinel_conn = sentinel_client.get_connection().await?;

    // 5. Verify we can run operations
    match redis::cmd("PING")
        .query_async::<_, String>(&mut sentinel_conn)
        .await
    {
        Ok(res) => {
            println!("PING sentinel_conn succeeded: {}", res);
            assert_eq!(res, "PONG");
        }
        Err(e) => {
            println!("PING sentinel_conn failed: {:?}", e);
            squeezefs::dlm::SENTINEL_CONN_POOL.remove("fake_master");
            return Err(e.into());
        }
    }

    // Clean up
    squeezefs::dlm::SENTINEL_CONN_POOL.remove("fake_master");

    Ok(())
}

async fn is_db_available() -> bool {
    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = match redis::Client::open(redis_url) {
        Ok(c) => c,
        Err(_) => return false,
    };
    client.get_multiplexed_tokio_connection().await.is_ok()
}

#[tokio::test]
async fn test_connection_resilience() -> Result<()> {
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return Ok(());
    }

    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = MetaClient::new(&redis_url)?;

    // Acquire connection
    let _conn = client.get_connection().await?;

    // Re-getting connection should succeed
    let _conn2 = client.get_connection().await?;

    Ok(())
}

#[tokio::test]
async fn test_single_bound_reconnection() -> Result<()> {
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return Ok(());
    }

    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let local_ip = "127.0.0.1".parse::<std::net::IpAddr>().unwrap();

    let client = MetaClient::new_with_local_ips(&redis_url, vec![local_ip]).await?;

    let mut conn = client.get_connection().await?;
    let res: String = redis::cmd("PING").query_async(&mut conn).await?;
    assert_eq!(res, "PONG");

    Ok(())
}

#[tokio::test]
async fn test_backend_health_check_and_failover() -> Result<()> {
    if !is_db_available().await {
        println!("Skipping test: Redis/Garnet not available");
        return Ok(());
    }

    let redis_url =
        std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let fs_name = "test_failover_fs";

    // Setup backend_0 (default) with a valid 16MB temp file
    let temp_dir = tempfile::tempdir().unwrap();
    let path_0 = temp_dir.path().join("backend_0.img");
    std::fs::write(&path_0, vec![0u8; 16 * 1024 * 1024]).unwrap();

    let meta = std::sync::Arc::new(MetaClient::new(&redis_url)?);
    let dev_0 = std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        path_0.to_str().unwrap(),
    ));
    let alloc_0 = std::sync::Arc::new(
        squeezefs::block_allocator::BlockAllocator::new(meta.clone(), fs_name).await?,
    );

    let block_size = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(4 * 1024 * 1024));
    let backend_router = std::sync::Arc::new(squeezefs::routing::BackendRouter::new(
        alloc_0, dev_0, block_size,
    ));

    // Setup backend_1 with a valid 16MB temp file
    let path_1 = temp_dir.path().join("backend_1.img");
    std::fs::write(&path_1, vec![0u8; 16 * 1024 * 1024]).unwrap();

    let dev_1 = std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        path_1.to_str().unwrap(),
    ));
    let alloc_1 = std::sync::Arc::new(
        squeezefs::block_allocator::BlockAllocator::new(
            meta.clone(),
            &format!("{}:backend_1", fs_name),
        )
        .await?,
    );

    backend_router.backends.insert(
        "backend_1".to_string(),
        std::sync::Arc::new(squeezefs::routing::StorageBackend {
            device: dev_1,
            block_allocator: alloc_1,
        }),
    );

    // Set active backend to backend_1
    backend_router
        .active_write_backend
        .store(std::sync::Arc::new("backend_1".to_string()));

    // Verify initially backend_1 is healthy and returned as active
    assert!(backend_router.is_backend_healthy("backend_1"));
    let (active_id, _, _) = backend_router.get_active_backend()?;
    assert_eq!(active_id, "backend_1");

    // Start background health check worker (period is 5s, but we will test inline failover instantly)
    backend_router.start_health_check_worker(redis_url.clone(), fs_name.to_string());

    // 1. Simulate target drop by deleting backend_1 device file
    std::fs::remove_file(&path_1).unwrap();

    // Verify it is now unhealthy
    assert!(!backend_router.is_backend_healthy("backend_1"));

    // Verify INLINE failover: get_active_backend should instantly fall back to backend_0
    let (active_id_after, _, _) = backend_router.get_active_backend()?;
    assert_eq!(active_id_after, "backend_0");

    // 2. Wait for the background worker to detect and commit failover to Redis
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;

    // Verify background failover: active_write_backend in router has been updated to backend_0
    let current_be = (*backend_router.active_write_backend.load_full()).clone();
    assert_eq!(current_be, "backend_0");

    // Verify value in Garnet/Redis format hash has been updated
    let mut con = meta.get_connection().await?;
    let format_key = format!("{}:format", fs_name);
    let db_active: String = redis::cmd("HGET")
        .arg(&format_key)
        .arg("active_write_backend")
        .query_async(&mut con)
        .await?;
    assert_eq!(db_active, "backend_0");

    Ok(())
}
