use crate::cache::nvme::NvmeStaging;
use crate::error::{Result, SqueezefsError};
use log::{error, info};
use std::time::Duration;
use bytes::Bytes;
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;
use redis::AsyncCommands;

struct SqueezefsLocalCacheReader {
    cache: NvmeStaging,
}

impl hypertier::dht::LocalCacheReader for SqueezefsLocalCacheReader {
    fn get_local(&self, key: &Bytes) -> Option<Bytes> {
        let guard = self.cache.read_nvme_cache.get(key)?;
        Some(Bytes::copy_from_slice(&guard.guard.mmap[guard.offset..guard.offset + guard.len]))
    }
}

#[derive(Clone)]
pub struct P2pServer {
    addr: String,
    cache: NvmeStaging,
}

impl Drop for P2pServer {
    fn drop(&mut self) {
        let redis_client = self.cache.redis_client().clone();
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
    pub fn new(addr: String, cache: NvmeStaging) -> Self {
        Self { addr, cache }
    }

    pub async fn run(&self) -> Result<()> {
        let reader = Arc::new(SqueezefsLocalCacheReader {
            cache: self.cache.clone(),
        });

        let dht_node = Arc::new(hypertier::dht::DhtNode::new(self.addr.clone(), reader));
        
        // Store DhtNode in NvmeStaging so clients can retrieve it
        let _ = self.cache.dht_node.set(dht_node.clone());

        // Start DHT TCP listener
        dht_node.clone().start().await.map_err(|e| {
            error!("P2P DHT: Failed to start listener on {}: {:?}", self.addr, e);
            SqueezefsError::Io(e)
        })?;

        info!("P2P DHT Server: Listening on {}", self.addr);

        // Start heartbeat and peer discovery loop
        let redis_client = self.cache.redis_client().clone();
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

                    // 2. Discover: pull other active clients to add to DHT routing table
                    if let Ok(peers) = con.zrangebyscore::<_, _, _, Vec<String>>("squeezefs:active_clients", now as f64, "+inf").await {
                        for peer in peers {
                            if peer != p2p_addr {
                                dht_clone.add_peer(peer);
                            }
                        }
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
        dht_node: &hypertier::dht::DhtNode,
        block_key: &str,
    ) -> Result<Vec<u8>> {
        let key_hash = xxh3_64(block_key.as_bytes());
        let provider_addr = dht_node.find_provider(key_hash).await.map_err(|e| {
            SqueezefsError::Io(e)
        })?.ok_or_else(|| {
            SqueezefsError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "No provider found in DHT"))
        })?;

        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let val_bytes = dht_node.fetch_remote_value(&provider_addr, key_bytes).await.map_err(|e| {
            SqueezefsError::Io(e)
        })?.ok_or_else(|| {
            SqueezefsError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "Block not found on provider node"))
        })?;

        Ok(val_bytes.to_vec())
    }
}

impl Default for P2pClient {
    fn default() -> Self {
        Self::new()
    }
}
