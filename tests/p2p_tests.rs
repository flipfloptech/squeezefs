use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::p2p::{P2pClient, P2pServer};
use squeezefs::routing::DataRouter;
use std::time::Duration;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn get_dlm_client() -> Option<DlmClient> {
    let url = get_redis_url();
    let client = DlmClient::new(&url).ok()?;
    let redis_client = redis::Client::open(url).ok()?;
    let _con = redis_client.get_multiplexed_tokio_connection().await.ok()?;
    Some(client)
}

async fn clear_garnet_keys(dlm: &DlmClient) {
    if let Ok(mut con) = dlm.meta_client().get_connection().await {
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg("block_peers:*")
            .query_async(&mut con)
            .await
            .unwrap_or_default();
        for key in keys {
            let _: () = con.del(key).await.unwrap_or_default();
        }
    }
}

#[tokio::test]
async fn test_p2p_happy_path() {
    let dlm_a = match get_dlm_client().await {
        Some(d) => d,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };
    clear_garnet_keys(&dlm_a).await;

    let temp_dir_a = tempdir().unwrap();
    let backend_a = RustFsClient::new_mock();
    let p2p_addr_a = "127.0.0.1:29099".to_string();

    let cache_a = TieredCache::new(
        vec![temp_dir_a.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend_a.clone(),
        dlm_a.meta_client().clone(),
    )
    .expect("Should create cache A");
    let _ = cache_a.nvme.p2p_addr.set(p2p_addr_a.clone());

    let router_a = DataRouter::new(dlm_a.clone(), backend_a, cache_a);

    // Start P2P server A
    let server_a = P2pServer::new(p2p_addr_a.clone(), router_a.cache.nvme.clone());
    let server_task = tokio::spawn(async move {
        let _ = server_a.run().await;
    });

    // Write a block locally to A's cache and register it
    let block_key = "backend_0/part_happy_p2p";
    let block_data = vec![42u8; 1000];
    router_a
        .cache
        .nvme
        .cache_read_block(block_key, &block_data)
        .expect("Should cache locally on A");

    // Wait a brief moment to ensure registration and server startup
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify registration on Garnet
    let mut con = dlm_a
        .get_connection()
        .await
        .expect("Should connect to Garnet");
    let safe_name = block_key.replace(['/', ':'], "_");
    let peers: Vec<String> = con
        .smembers(format!("block_peers:{}", safe_name))
        .await
        .expect("Should get members");
    assert!(peers.contains(&p2p_addr_a));

    // Setup Node B (client)
    let dlm_b = DlmClient::new(&get_redis_url()).unwrap();
    let temp_dir_b = tempdir().unwrap();
    let backend_b = RustFsClient::new_mock();
    let p2p_addr_b = "127.0.0.1:29100".to_string();

    let cache_b = TieredCache::new(
        vec![temp_dir_b.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend_b.clone(),
        dlm_b.meta_client().clone(),
    )
    .expect("Should create cache B");
    let _ = cache_b.nvme.p2p_addr.set(p2p_addr_b.clone());

    let _router_b = DataRouter::new(dlm_b.clone(), backend_b, cache_b);

    // Read the block from B using P2P client
    let client = P2pClient::new();
    let downloaded = client
        .download_block_from_peer(&p2p_addr_a, block_key)
        .await
        .expect("Should download block from peer A");

    assert_eq!(downloaded, block_data);

    // Clean up server
    server_task.abort();
}

#[tokio::test]
async fn test_p2p_fallback_path() {
    let dlm = match get_dlm_client().await {
        Some(d) => d,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };
    clear_garnet_keys(&dlm).await;

    // S3 mock client prepopulated with block data
    let backend = RustFsClient::new_mock();
    let block_key = "backend_0/part_fallback_p2p/part_0";
    let block_data = vec![77u8; 1000];
    backend
        .put_object(block_key, block_data.clone(), 0)
        .await
        .expect("Should seed mock S3 block");

    // Register a dead/non-responsive peer in Garnet for this block
    let dead_peer = "127.0.0.1:29098".to_string();
    let safe_name = block_key.replace(['/', ':'], "_");
    let mut con = dlm.get_connection().await.expect("Should connect");
    let _: () = con
        .sadd(format!("block_peers:{}", safe_name), &dead_peer)
        .await
        .expect("Should sadd");

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
    .expect("Should create cache");
    let _ = cache.nvme.p2p_addr.set("127.0.0.1:29101".to_string());

    let router = DataRouter::new(dlm.clone(), backend, cache);

    // Write metadata for striped file using this block
    let file_path = "fallback_striped.bin";
    let mut con_meta = dlm.get_connection().await.unwrap();
    let meta_key = format!("metadata:{}", file_path);
    let _: () = redis::pipe()
        .hset(&meta_key, "type", "striped")
        .hset(&meta_key, "size", 1000u64)
        .hset(&meta_key, "block_prefix", "backend_0/part_fallback_p2p")
        .query_async(&mut con_meta)
        .await
        .unwrap();

    // Read range: should attempt dead peer, fail-fast (timeout), and successfully fallback to S3
    let read_res = router
        .read_file_range(file_path, 0, 1000)
        .await
        .expect("Should read range via S3 fallback");

    assert_eq!(read_res, block_data);

    // Wait a brief moment to ensure asynchronous registration has completed
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

    // Assert B registered itself in Garnet since it downloaded and cached the block
    let peers: Vec<String> = con_meta
        .smembers(format!("block_peers:{}", safe_name))
        .await
        .unwrap();
    assert!(peers.contains(&"127.0.0.1:29101".to_string()));
}

#[tokio::test]
async fn test_p2p_ttl_expiration() {
    let dlm = match get_dlm_client().await {
        Some(d) => d,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };
    clear_garnet_keys(&dlm).await;

    let temp_dir = tempdir().unwrap();
    let backend = RustFsClient::new_mock();
    let cache = TieredCache::new(
        vec![temp_dir.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .expect("Should create cache");
    let _ = cache.nvme.p2p_addr.set("127.0.0.1:29102".to_string());

    let block_key = "backend_0/part_ttl_p2p";
    // Directly cache block to trigger registration with expiration (we can mock local registration or just test the key expiration)
    cache
        .nvme
        .cache_read_block(block_key, &[1, 2, 3])
        .expect("Should cache");

    // Wait a brief moment to make sure async task completes registration
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut con = dlm.get_connection().await.unwrap();
    let safe_name = block_key.replace(['/', ':'], "_");
    let ttl: i64 = redis::cmd("TTL")
        .arg(format!("block_peers:{}", safe_name))
        .query_async(&mut con)
        .await
        .unwrap_or(0);

    // TTL should be positive (and <= 60 seconds)
    assert!(ttl > 0 && ttl <= 60);
}

#[tokio::test]
async fn test_p2p_cooperative_read_after_write() {
    let dlm_a = match get_dlm_client().await {
        Some(d) => d,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };
    clear_garnet_keys(&dlm_a).await;

    // Set up Node A (Writer / Server)
    let temp_dir_a = tempdir().unwrap();
    let backend_a = RustFsClient::new_mock();
    let p2p_addr_a = "127.0.0.1:29103".to_string();

    let cache_a = TieredCache::new(
        vec![temp_dir_a.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend_a.clone(),
        dlm_a.meta_client().clone(),
    )
    .expect("Should create cache A");
    let _ = cache_a.nvme.p2p_addr.set(p2p_addr_a.clone());

    let router_a = DataRouter::new(dlm_a.clone(), backend_a.clone(), cache_a);

    // Start P2P server A
    let server_a = P2pServer::new(p2p_addr_a.clone(), router_a.cache.nvme.clone());
    let server_task = tokio::spawn(async move {
        let _ = server_a.run().await;
    });

    // Seed the S3 mock store with empty default data for our test block
    let block_key = "backend_0/part_coop_p2p/part_0";
    let default_s3_data = vec![0u8; 1000];
    backend_a
        .put_object(block_key, default_s3_data.clone(), 0)
        .await
        .expect("Should seed mock S3 block");

    // Node A writes actual data to the file, transitioning to striped layout
    let file_path = "coop_striped.bin";
    let mut con_meta = dlm_a.get_connection().await.unwrap();
    let meta_key = format!("metadata:{}", file_path);
    let _: () = redis::pipe()
        .hset(&meta_key, "type", "striped")
        .hset(&meta_key, "size", 1000u64)
        .hset(&meta_key, "block_prefix", "backend_0/part_coop_p2p")
        .query_async(&mut con_meta)
        .await
        .unwrap();

    let write_data = vec![99u8; 1000];
    // Node A writes the block. This caches it on Node A and registers it under block_peers
    router_a
        .cache
        .nvme
        .cache_read_block(block_key, &write_data)
        .expect("Should cache write data on Node A");

    // Wait a brief moment to ensure P2P server is ready and registered
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Set up Node B (Reader / Client)
    let dlm_b = DlmClient::new(&get_redis_url()).unwrap();
    let temp_dir_b = tempdir().unwrap();
    let backend_b = RustFsClient::new_mock(); // Fresh S3 client with default_s3_data
    backend_b
        .put_object(block_key, default_s3_data.clone(), 0)
        .await
        .expect("Should seed mock S3 block on B");

    let p2p_addr_b = "127.0.0.1:29104".to_string();

    let cache_b = TieredCache::new(
        vec![temp_dir_b.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend_b.clone(),
        dlm_b.meta_client().clone(),
    )
    .expect("Should create cache B");
    let _ = cache_b.nvme.p2p_addr.set(p2p_addr_b.clone());

    let router_b = DataRouter::new(dlm_b.clone(), backend_b, cache_b);

    // Node B reads the file range.
    // Since Node A has registered itself as a peer for this block, Node B should read it
    // directly from Node A's cache (returning write_data: vec![99; 1000]) instead of S3 (which has vec![0; 1000]).
    let read_res = router_b
        .read_file_range(file_path, 0, 1000)
        .await
        .expect("Should read range via P2P");

    assert_eq!(read_res, write_data);

    // Clean up server
    server_task.abort();
}

#[tokio::test]
async fn test_p2p_fast_fail_timeout() {
    // Start a dummy TCP server that accepts connections but never responds
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();

    // Spawn task to accept connection but just hold/sleep
    let _handle = tokio::spawn(async move {
        if let Ok((_stream, _)) = listener.accept().await {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    let client = P2pClient::new();
    let start = std::time::Instant::now();
    let res = client.download_block_from_peer(&addr, "test_block").await;
    let elapsed = start.elapsed();

    assert!(res.is_err(), "Expected timeout error");
    assert!(
        elapsed >= Duration::from_millis(50),
        "Should take at least 50ms, got {}ms",
        elapsed.as_millis()
    );
    assert!(
        elapsed < Duration::from_millis(150),
        "Should time out well before 300ms (timeout target is 50ms), got {}ms",
        elapsed.as_millis()
    );
}

#[tokio::test]
async fn test_p2p_random_subset_limit() {
    let dlm = match get_dlm_client().await {
        Some(d) => d,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };
    clear_garnet_keys(&dlm).await;

    // S3 mock client prepopulated with block data
    let backend = RustFsClient::new_mock();
    let block_key = "backend_0/part_random_subset/part_0";
    let block_data = vec![88u8; 1000];
    backend
        .put_object(block_key, block_data.clone(), 0)
        .await
        .expect("Should seed mock S3 block");

    // Start 5 dummy listeners
    let mut listeners = Vec::new();
    let mut dead_peers = Vec::new();
    for _ in 0..5 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        dead_peers.push(addr);
        listeners.push(listener);
    }

    let connection_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Accept connections but don't respond
    for listener in listeners {
        let conn_count = connection_count.clone();
        tokio::spawn(async move {
            if let Ok((_stream, _)) = listener.accept().await {
                conn_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        });
    }

    // Register all 5 dead/non-responsive peers in Garnet
    let safe_name = block_key.replace(['/', ':'], "_");
    let mut con = dlm.get_connection().await.expect("Should connect");
    for peer in &dead_peers {
        let _: () = con
            .sadd(format!("block_peers:{}", safe_name), peer)
            .await
            .expect("Should sadd");
    }

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
    .expect("Should create cache");
    // Ensure we don't match own address
    let _ = cache.nvme.p2p_addr.set("127.0.0.1:29105".to_string());

    let router = DataRouter::new(dlm.clone(), backend, cache);

    // Write metadata for striped file using this block
    let file_path = "random_subset.bin";
    let mut con_meta = dlm.get_connection().await.unwrap();
    let meta_key = format!("metadata:{}", file_path);
    let _: () = redis::pipe()
        .hset(&meta_key, "type", "striped")
        .hset(&meta_key, "size", 1000u64)
        .hset(&meta_key, "block_prefix", "backend_0/part_random_subset")
        .query_async(&mut con_meta)
        .await
        .unwrap();

    // Read range: should try at most 3 peers, each timing out in 50ms, then fallback to S3.
    let start = std::time::Instant::now();
    let read_res = router
        .read_file_range(file_path, 0, 1000)
        .await
        .expect("Should read range via S3 fallback");
    let elapsed = start.elapsed();

    assert_eq!(read_res, block_data);

    // Assert we queried exactly 3 peers
    let count = connection_count.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        count, 3,
        "Expected exactly 3 peers to be queried, got {}",
        count
    );

    // Assert the timing is reasonable (should be around 150ms, definitely < 240ms)
    assert!(
        elapsed < Duration::from_millis(240),
        "Elapsed time {}ms is too high; queried too many peers?",
        elapsed.as_millis()
    );
}
