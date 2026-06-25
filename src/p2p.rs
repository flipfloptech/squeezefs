use crate::cache::nvme::NvmeStaging;
use crate::error::{Result, SqueezefsError};
use log::{debug, error, info};
use redis::AsyncCommands;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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
        let listener = TcpListener::bind(&self.addr).await.map_err(|e| {
            error!("P2P Server: Failed to bind to {}: {:?}", self.addr, e);
            SqueezefsError::Io(e)
        })?;

        info!("P2P Server: Listening on {}", self.addr);

        // Start heartbeat loop
        let redis_client = self.cache.redis_client().clone();
        let p2p_addr = self.addr.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(mut con) = redis_client.get_connection().await {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let _: std::result::Result<(), redis::RedisError> = redis::cmd("ZADD")
                        .arg("squeezefs:active_clients")
                        .arg(now + 30)
                        .arg(&p2p_addr)
                        .query_async(&mut con)
                        .await;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });

        // Start health checker loop
        let cache_ref = self.cache.clone();
        let p2p_addr_for_check = self.addr.clone();
        let checker_server = Self::new(p2p_addr_for_check, cache_ref);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if let Err(e) = checker_server.check_and_prune_peers().await {
                    debug!("P2P Server: Health check loop error: {:?}", e);
                }
            }
        });

        loop {
            match listener.accept().await {
                Ok((stream, client_addr)) => {
                    debug!("P2P Server: Accepted connection from {:?}", client_addr);
                    let cache_clone = self.cache.clone();
                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection(cache_clone, stream).await {
                            debug!("P2P Server: Connection handler error: {:?}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("P2P Server: Error accepting connection: {:?}", e);
                }
            }
        }
    }

    pub async fn check_and_prune_peers(&self) -> Result<()> {
        let redis_client = self.cache.redis_client();
        let mut con = redis_client.get_connection().await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Lua script to atomically fetch and lease a batch of up to 10 peers
        let lua_script = r#"
            local peers = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1], 'LIMIT', 0, ARGV[2])
            if #peers > 0 then
                for _, peer in ipairs(peers) do
                    redis.call('ZADD', KEYS[1], ARGV[3], peer)
                end
            end
            return peers
        "#;

        let script = redis::Script::new(lua_script);
        let leased_peers: Vec<String> = script
            .key("squeezefs:peer_health_check_schedule")
            .arg(now)
            .arg(10)
            .arg(now + 60)
            .invoke_async(&mut con)
            .await
            .map_err(SqueezefsError::Redis)?;

        if leased_peers.is_empty() {
            return Ok(());
        }

        debug!(
            "P2P Server: Leased {} peers for health checks",
            leased_peers.len()
        );

        let mut handles = Vec::new();
        for peer in leased_peers {
            let redis_client_clone = redis_client.clone();
            let peer_addr = peer.clone();
            handles.push(tokio::spawn(async move {
                let is_alive = matches!(
                    tokio::time::timeout(
                        Duration::from_millis(50),
                        TcpStream::connect(&peer_addr),
                    )
                    .await,
                    Ok(Ok(_))
                );

                if let Ok(mut con) = redis_client_clone.get_connection().await {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    if is_alive {
                        debug!(
                            "P2P Server: Peer {} is alive. Rescheduling check.",
                            peer_addr
                        );
                        let _: std::result::Result<(), redis::RedisError> = redis::cmd("ZADD")
                            .arg("squeezefs:peer_health_check_schedule")
                            .arg(now + 120)
                            .arg(&peer_addr)
                            .query_async(&mut con)
                            .await;
                    } else {
                        info!(
                            "P2P Server: Peer {} is dead/unresponsive. Pruning references.",
                            peer_addr
                        );
                        let mut pipe = redis::pipe();
                        pipe.cmd("ZREM")
                            .arg("squeezefs:peer_health_check_schedule")
                            .arg(&peer_addr)
                            .cmd("ZREM")
                            .arg("squeezefs:active_clients")
                            .arg(&peer_addr);

                        let peer_blocks_key = format!("squeezefs:peer_blocks:{}", peer_addr);
                        if let Ok(blocks) = con.smembers::<_, Vec<String>>(&peer_blocks_key).await {
                            for block in blocks {
                                let safe_name = block.replace(['/', ':'], "_");
                                pipe.srem(format!("block_peers:{}", safe_name), &peer_addr);
                            }
                        }
                        pipe.del(&peer_blocks_key);

                        let _: std::result::Result<(), redis::RedisError> =
                            pipe.query_async(&mut con).await;
                    }
                }
            }));
        }

        for h in handles {
            let _ = h.await;
        }

        Ok(())
    }

    async fn handle_connection(cache: NvmeStaging, mut stream: TcpStream) -> Result<()> {
        // 1. Read 4-byte key length
        let key_len = stream.read_u32().await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("Failed to read key length: {:?}", e),
            ))
        })? as usize;

        if key_len > 4096 {
            return Err(SqueezefsError::InvalidOperation(
                "P2P Server: Request key length too large".to_string(),
            ));
        }

        // 2. Read block key
        let mut key_buf = vec![0u8; key_len];
        stream.read_exact(&mut key_buf).await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("Failed to read key bytes: {:?}", e),
            ))
        })?;

        let block_key = String::from_utf8(key_buf).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("P2P Server: Invalid UTF-8 key: {:?}", e))
        })?;

        debug!("P2P Server: Received request for block {}", block_key);

        // 3. Query local NVMe staging cache
        if let Some(block_data) = cache.get_cached_read_block(&block_key) {
            // Write success status
            stream.write_u8(1).await.map_err(SqueezefsError::Io)?;
            // Write data length
            stream
                .write_u32(block_data.len() as u32)
                .await
                .map_err(SqueezefsError::Io)?;
            // Write data
            stream
                .write_all(&block_data)
                .await
                .map_err(SqueezefsError::Io)?;
            debug!(
                "P2P Server: Successfully served block {} ({} bytes)",
                block_key,
                block_data.len()
            );
        } else {
            // Write not found status
            stream.write_u8(0).await.map_err(SqueezefsError::Io)?;
            debug!("P2P Server: Block {} not found in local cache", block_key);
        }

        stream.flush().await.map_err(SqueezefsError::Io)?;
        Ok(())
    }
}

pub struct P2pClient;

impl P2pClient {
    pub fn new() -> Self {
        Self
    }

    pub async fn download_block_from_peer(
        &self,
        peer_addr: &str,
        block_key: &str,
    ) -> Result<Vec<u8>> {
        let block_key = block_key.to_string();
        let peer_addr = peer_addr.to_string();
        let log_addr = peer_addr.clone();

        let download_future = async move {
            let mut stream = TcpStream::connect(&peer_addr).await.map_err(|e| {
                debug!(
                    "P2P Client: Failed to connect to peer {}: {:?}",
                    peer_addr, e
                );
                SqueezefsError::Io(e)
            })?;

            let key_bytes = block_key.as_bytes();
            // Send 4-byte key length + key bytes
            stream
                .write_u32(key_bytes.len() as u32)
                .await
                .map_err(SqueezefsError::Io)?;
            stream
                .write_all(key_bytes)
                .await
                .map_err(SqueezefsError::Io)?;
            stream.flush().await.map_err(SqueezefsError::Io)?;

            // Read 1-byte status
            let status = stream.read_u8().await.map_err(|e| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("Failed to read status from peer {}: {:?}", peer_addr, e),
                ))
            })?;

            if status == 1 {
                // Read 4-byte data length
                let data_len = stream.read_u32().await.map_err(|e| {
                    SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "Failed to read data length from peer {}: {:?}",
                            peer_addr, e
                        ),
                    ))
                })? as usize;

                if data_len > 16 * 1024 * 1024 {
                    // Sanity check: blocks should not exceed 16MB (expected max 4MB)
                    return Err(SqueezefsError::InvalidOperation(
                        "P2P Client: Received block size too large".to_string(),
                    ));
                }

                let mut data_buf = vec![0u8; data_len];
                stream.read_exact(&mut data_buf).await.map_err(|e| {
                    SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "Failed to read data payload from peer {}: {:?}",
                            peer_addr, e
                        ),
                    ))
                })?;

                Ok(data_buf)
            } else {
                Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("Block not found on peer {}", peer_addr),
                )))
            }
        };

        // Enforce 50ms fail-fast timeout
        match tokio::time::timeout(Duration::from_millis(50), download_future).await {
            Ok(res) => res,
            Err(_) => {
                debug!("P2P Client: Download from peer {} timed out", log_addr);
                Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "P2P peer download timed out",
                )))
            }
        }
    }
}

impl Default for P2pClient {
    fn default() -> Self {
        Self::new()
    }
}
