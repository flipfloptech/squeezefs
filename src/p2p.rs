use crate::cache::TieredCache;
use crate::error::{Result, SqueezefsError};
use bytes::Bytes;
use log::{error, info};
use redis::AsyncCommands;
use std::sync::Arc;
use std::time::Duration;
use xxhash_rust::xxh3::xxh3_64;

struct SqueezefsLocalCacheReader {
    cache: TieredCache,
}

impl crate::tiering::dht::LocalCacheReader for SqueezefsLocalCacheReader {
    fn get_local(&self, key: &Bytes) -> Option<Bytes> {
        let key_str = String::from_utf8_lossy(key).to_string();

        // 1. Try RAM LRU cache (Tier 2)
        if let Some(data) = self.cache.read_lru.get(&key_str) {
            return Some(data.clone());
        }

        // 2. Try staging NVMe cache (Tier 3)
        if let Some(staged) = self.cache.nvme.read_staged(&key_str) {
            return Some(Bytes::from(staged));
        }

        // 3. Try read NVMe cache (Tier 3)
        if let Some(guard) = self.cache.nvme.read_nvme_cache.get(key) {
            return Some(Bytes::copy_from_slice(
                &guard.guard.mmap[guard.offset..guard.offset + guard.len],
            ));
        }

        None
    }

    fn put_local(&self, key: Bytes, value: Bytes) {
        let block_key = String::from_utf8_lossy(&key).to_string();
        let _ = self.cache.nvme.cache_read_block(&block_key, value);
    }
}

#[derive(Clone)]
pub struct P2pServer {
    addr: String,
    cache: TieredCache,
    security_config: crate::tiering::dht::ClusterSecurityConfig,
}

impl Drop for P2pServer {
    fn drop(&mut self) {
        let redis_client = self.cache.nvme.redis_client().clone();
        let addr = self.addr.clone();
        tokio::spawn(async move {
            if let Ok(mut con) = redis_client.get_connection().await {
                let _: std::result::Result<(), redis::RedisError> = redis::cmd("ZREM")
                    .arg("squeezefs:active_clients")
                    .arg(&addr)
                    .query_async(&mut con)
                    .await;
            }
        });
    }
}

impl P2pServer {
    pub fn new(
        addr: String,
        cache: TieredCache,
        security_config: crate::tiering::dht::ClusterSecurityConfig,
    ) -> Self {
        Self {
            addr,
            cache,
            security_config,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let reader = Arc::new(SqueezefsLocalCacheReader {
            cache: self.cache.clone(),
        });

        let dht_node = Arc::new(crate::tiering::dht::DhtNode::new_with_security(
            self.addr.clone(),
            reader,
            self.security_config.clone(),
        ));

        // Store DhtNode in NvmeStaging so clients can retrieve it
        let _ = self.cache.nvme.dht_node.set(dht_node.clone());

        // Start DHT TCP listener
        dht_node.clone().start().await.map_err(|e| {
            error!(
                "P2P DHT: Failed to start listener on {}: {:?}",
                self.addr, e
            );
            SqueezefsError::Io(e)
        })?;

        info!("P2P DHT Server: Listening on {}", self.addr);

        // Start heartbeat and peer discovery loop
        let redis_client = self.cache.nvme.redis_client().clone();
        let p2p_addr = self.addr.clone();
        let dht_clone = dht_node.clone();

        tokio::spawn(async move {
            loop {
                if let Ok(mut con) = redis_client.get_connection().await {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    // 1. Heartbeat: register self in Redis active clients
                    let _: std::result::Result<(), redis::RedisError> = redis::cmd("ZADD")
                        .arg("squeezefs:active_clients")
                        .arg(now + 30)
                        .arg(&p2p_addr)
                        .query_async(&mut con)
                        .await;

                    // 2. Discover: pull other active clients to update routing table
                    if let Ok(peers) = con
                        .zrangebyscore::<_, _, _, Vec<String>>(
                            "squeezefs:active_clients",
                            now as f64,
                            "+inf",
                        )
                        .await
                    {
                        let mut active_set = std::collections::HashSet::new();
                        for peer in peers {
                            if peer != p2p_addr {
                                active_set.insert(peer);
                            }
                        }
                        dht_clone.set_peers(active_set);
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });

        // Keep running (the server tasks run in the background, we just sleep)
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
}

pub struct P2pClient;

impl P2pClient {
    pub fn new() -> Self {
        Self
    }

    pub async fn download_block_from_peer(
        &self,
        dht_node: &crate::tiering::dht::DhtNode,
        block_key: &str,
    ) -> Result<Vec<u8>> {
        let key_hash = xxh3_64(block_key.as_bytes());
        let target_nodes = dht_node.find_closest_peers(key_hash, 3);
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());

        for provider_addr in target_nodes {
            if provider_addr == dht_node.peer_addr() {
                if let Some(val) = dht_node.get_local_value(&key_bytes) {
                    return Ok(val.to_vec());
                }
            } else {
                match dht_node
                    .fetch_remote_value(&provider_addr, key_bytes.clone())
                    .await
                {
                    Ok(Some(val_bytes)) => {
                        return Ok(val_bytes.to_vec());
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        error!(
                            "P2P DHT: Failed to fetch block from peer {}: {:?}",
                            provider_addr, e
                        );
                        continue;
                    }
                }
            }
        }

        Err(SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Block {} not found on any of the 3 designated owner nodes",
                block_key
            ),
        )))
    }
}

impl Default for P2pClient {
    fn default() -> Self {
        Self::new()
    }
}
