/*
 * SqueezeFS, Copyright 2026 Juicedata, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

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
        let _: () = redis::cmd("DEL")
            .arg("squeezefs:active_clients")
            .query_async(&mut con)
            .await
            .unwrap_or_default();
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
    let server_task_a = tokio::spawn(async move {
        let _ = server_a.run().await;
    });

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

    let router_b = DataRouter::new(dlm_b.clone(), backend_b, cache_b);

    // Start P2P server B so B has a DHT node initialized
    let server_b = P2pServer::new(p2p_addr_b.clone(), router_b.cache.nvme.clone());
    let server_task_b = tokio::spawn(async move {
        let _ = server_b.run().await;
    });

    // Wait a brief moment to ensure server startup
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Get the DHT nodes
    let dht_a = router_a.cache.nvme.dht_node.get().expect("A's DHT Node should be set");
    let dht_b = router_b.cache.nvme.dht_node.get().expect("B's DHT Node should be set");

    // Link them manually
    dht_a.add_peer(p2p_addr_b.clone());
    dht_b.add_peer(p2p_addr_a.clone());

    // Write a block locally to A's cache and register it in DHT
    let block_key = "backend_0/part_happy_p2p";
    let block_data = vec![42u8; 1000];
    router_a
        .cache
        .nvme
        .cache_read_block(block_key, &block_data)
        .expect("Should cache locally on A");

    // Wait for DHT registration to propagate locally
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Read the block from B using P2P client
    let client = P2pClient::new();
    let downloaded = client
        .download_block_from_peer(dht_b, block_key)
        .await
        .expect("Should download block from peer A");

    assert_eq!(downloaded, block_data);

    // Clean up
    server_task_a.abort();
    server_task_b.abort();
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
    let server_task_a = tokio::spawn(async move {
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
    // Node A writes the block. This caches it on Node A and registers it in DHT
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

    // Start P2P server B so B has a DHT node initialized
    let server_b = P2pServer::new(p2p_addr_b.clone(), router_b.cache.nvme.clone());
    let server_task_b = tokio::spawn(async move {
        let _ = server_b.run().await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Get the DHT nodes
    let dht_a = router_a.cache.nvme.dht_node.get().expect("A's DHT Node should be set");
    let dht_b = router_b.cache.nvme.dht_node.get().expect("B's DHT Node should be set");

    // Link them manually
    dht_a.add_peer(p2p_addr_b.clone());
    dht_b.add_peer(p2p_addr_a.clone());

    // Node B reads the file range.
    // Since Node A has registered itself as a peer for this block in the DHT, Node B should read it
    // directly from Node A's cache (returning write_data: vec![99; 1000]) instead of S3 (which has vec![0; 1000]).
    let read_res = router_b
        .read_file_range(file_path, 0, 1000)
        .await
        .expect("Should read range via P2P");

    assert_eq!(read_res, write_data);

    // Clean up
    server_task_a.abort();
    server_task_b.abort();
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
    let p2p_addr = "127.0.0.1:29101".to_string();
    let _ = cache.nvme.p2p_addr.set(p2p_addr.clone());

    let router = DataRouter::new(dlm.clone(), backend, cache);

    // Start P2P server so DHT node is initialized
    let server = P2pServer::new(p2p_addr.clone(), router.cache.nvme.clone());
    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let dht = router.cache.nvme.dht_node.get().expect("DHT Node should be set");
    
    // Add a dead peer to DHT routing table
    dht.add_peer("127.0.0.1:29098".to_string());

    // Register the dead peer as provider of this block key hash in the DHT.
    // We send a RegisterProvider message to our own DHT node claiming that the dead peer is the provider.
    let key_hash = xxhash_rust::xxh3::xxh3_64(block_key.as_bytes());
    
    // Send register message to our own listener
    let mut stream = tokio::net::TcpStream::connect(&p2p_addr).await.unwrap();
    let reg_msg = hypertier::dht::Message::RegisterProvider {
        key_hash,
        provider_addr: "127.0.0.1:29098".to_string(),
    };
    hypertier::dht::write_msg(&mut stream, &reg_msg).await.unwrap();
    
    // Wait for registration
    tokio::time::sleep(Duration::from_millis(100)).await;

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

    // Read range: should attempt dead peer, fail, and successfully fallback to S3
    let read_res = router
        .read_file_range(file_path, 0, 1000)
        .await
        .expect("Should read range via S3 fallback");

    assert_eq!(read_res, block_data);

    server_task.abort();
}
