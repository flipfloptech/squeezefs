use crate::cache::TieredCache;
use crate::error::{Result, SqueezefsError};
use bytes::Bytes;
use log::{error, info};
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
        // Peer stores are non-owner publishes and can be arbitrarily stale:
        // incarnation-validated or not at all (generic/074 stale-fill family).
        let _ = self
            .cache
            .nvme
            .cache_read_block_validated_self(&block_key, value);
    }
}

#[derive(Clone)]
pub struct P2pServer {
    addr: String,
    cache: TieredCache,
    security_config: crate::tiering::dht::ClusterSecurityConfig,
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
