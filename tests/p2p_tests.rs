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
    let block_key = "backend_0/part_fallback_p2p";
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
