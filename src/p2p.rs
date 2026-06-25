use crate::cache::nvme::NvmeStaging;
use crate::error::{Result, SqueezefsError};
use log::{debug, error, info};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct P2pServer {
    addr: String,
    cache: NvmeStaging,
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
