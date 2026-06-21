use crate::backend::RustFsClient;
use crate::cache::TieredCache;
use crate::dlm::DlmClient;
use crate::error::{Result, SqueezefsError};
use log::{debug, info};
use redis::AsyncCommands;
use uuid::Uuid;

#[derive(Clone)]
pub struct DataRouter {
    dlm: DlmClient,
    backend: RustFsClient,
    cache: TieredCache,
}

impl DataRouter {
    pub fn new(dlm: DlmClient, backend: RustFsClient, cache: TieredCache) -> Self {
        Self {
            dlm,
            backend,
            cache,
        }
    }

    /// Write file data using progressive data layout routing.
    pub async fn write_file(&self, file_path: &str, data: &[u8], fencing_token: u64) -> Result<()> {
        let size = data.len();

        if size < 64 * 1024 {
            // 1. Micro-Files (< 64KB): KV Inlining
            info!(
                "Routing: '{}' is a Micro-File ({} bytes < 64KB). Inlining into Garnet.",
                file_path, size
            );

            let mut con = self
                .dlm
                .redis_client()
                .get_multiplexed_tokio_connection()
                .await?;
            let inline_key = format!("inline_data:{}", file_path);
            let meta_key = format!("metadata:{}", file_path);

            // Execute inlining atomically (pipelined)
            let _: () = redis::pipe()
                .set(&inline_key, data)
                .hset(&meta_key, "size", size)
                .hset(&meta_key, "type", "inline")
                .hset(&meta_key, "fencing_token", fencing_token)
                .query_async(&mut con)
                .await?;

            // Cache in Tier 2: System RAM
            self.cache.lru.put(file_path, data.to_vec());
        } else if size <= 4 * 1024 * 1024 {
            // 2. Small Files (64KB - 4MB): Local NVMe Staging
            info!(
                "Routing: '{}' is a Small File ({} bytes). Staging to local NVMe.",
                file_path, size
            );

            let file_id = Uuid::new_v4().to_string();
            let mut con = self
                .dlm
                .redis_client()
                .get_multiplexed_tokio_connection()
                .await?;
            let meta_key = format!("metadata:{}", file_path);

            // Save metadata first
            let _: () = redis::pipe()
                .hset(&meta_key, "size", size)
                .hset(&meta_key, "type", "staged")
                .hset(&meta_key, "file_id", &file_id)
                .hset(&meta_key, "fencing_token", fencing_token)
                .query_async(&mut con)
                .await?;

            // Stage to local NVMe (writes locally and notifies background merge thread)
            self.cache
                .nvme
                .stage_write(file_path, &file_id, data, fencing_token)
                .await?;

            // Cache in Tier 2: System RAM
            self.cache.lru.put(file_path, data.to_vec());
        } else {
            // 3. Large Files (> 4MB): Parallel Striping
            info!(
                "Routing: '{}' is a Large File ({} bytes > 4MB). Striping blocks to RustFS.",
                file_path, size
            );

            let block_size = 4 * 1024 * 1024;
            let mut futures = Vec::new();
            let file_uuid = Uuid::new_v4().to_string();
            let mut offset = 0;
            let mut block_count = 0;

            while offset < size {
                let end = std::cmp::min(offset + block_size, size);
                let chunk = data[offset..end].to_vec();
                let block_key = format!("blocks/{}/part_{}", file_uuid, block_count);

                let backend_clone = self.backend.clone();
                let task = tokio::spawn(async move {
                    backend_clone
                        .put_object(&block_key, chunk, fencing_token)
                        .await
                });

                futures.push(task);
                offset = end;
                block_count += 1;
            }

            // Wait for all block uploads concurrently
            for f in futures {
                f.await.map_err(|e| {
                    SqueezefsError::Io(std::io::Error::other(format!(
                        "Stripe upload task failed: {:?}",
                        e
                    )))
                })??;
            }

            // Record striped metadata in Garnet
            let mut con = self
                .dlm
                .redis_client()
                .get_multiplexed_tokio_connection()
                .await?;
            let meta_key = format!("metadata:{}", file_path);
            let _: () = redis::pipe()
                .hset(&meta_key, "size", size)
                .hset(&meta_key, "type", "striped")
                .hset(&meta_key, "block_prefix", format!("blocks/{}", file_uuid))
                .hset(&meta_key, "num_blocks", block_count)
                .hset(&meta_key, "fencing_token", fencing_token)
                .query_async(&mut con)
                .await?;

            // Cache in Tier 2: System RAM
            self.cache.lru.put(file_path, data.to_vec());
        }

        Ok(())
    }

    /// Read file data, attempting to satisfy the read via the fastest cache tier.
    pub async fn read_file(&self, file_path: &str) -> Result<Vec<u8>> {
        // Tier 2 check: System RAM LRU Cache
        if let Some(cached_data) = self.cache.lru.get(file_path) {
            debug!(
                "Routing: Cache hit (Tier 2 - Unified System RAM) for '{}'",
                file_path
            );
            return Ok(cached_data);
        }

        // Fetch file metadata from Garnet
        let mut con = self
            .dlm
            .redis_client()
            .get_multiplexed_tokio_connection()
            .await?;
        let meta_key = format!("metadata:{}", file_path);

        let file_type: Option<String> = con.hget(&meta_key, "type").await?;
        let file_type = file_type.ok_or_else(|| {
            SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("File not found: {}", file_path),
            ))
        })?;

        let data = match file_type.as_str() {
            "inline" => {
                // Micro-File: retrieve raw payload directly from Garnet
                debug!(
                    "Routing: File '{}' inline read from metadata server.",
                    file_path
                );
                let inline_key = format!("inline_data:{}", file_path);
                let bytes: Vec<u8> = con.get(&inline_key).await?;
                bytes
            }
            "staged" => {
                // Small File: check Tier 3 (NVMe Staging) first
                let file_id_opt: Option<String> = con.hget(&meta_key, "file_id").await?;
                let file_id = file_id_opt.ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Missing file_id for staged file".to_string())
                })?;

                if let Some(staged_data) = self.cache.nvme.read_staged(&file_id) {
                    debug!(
                        "Routing: Cache hit (Tier 3 - NVMe Staging) for '{}' (ID: {})",
                        file_path, file_id
                    );
                    staged_data
                } else {
                    // NVMe staging file was flushed/merged. Read the packed block from S3
                    debug!(
                        "Routing: Staged file '{}' (ID: {}) already merged. Reading packed block.",
                        file_path, file_id
                    );
                    let mapping_key = format!("mapping:{}", file_id);
                    let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                    let offset: Option<u64> = con.hget(&mapping_key, "offset").await?;
                    let size: Option<u64> = con.hget(&mapping_key, "size").await?;

                    if let (Some(bk), Some(off), Some(sz)) = (block_key, offset, size) {
                        let packed_bytes = self.backend.get_object(&bk).await?;
                        let start = off as usize;
                        let end = (off + sz) as usize;
                        packed_bytes[start..end].to_vec()
                    } else {
                        return Err(SqueezefsError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Staged file ID {} mapping not found in Garnet", file_id),
                        )));
                    }
                }
            }
            "striped" => {
                // Large File: fetch blocks from S3 in parallel
                debug!(
                    "Routing: Striped file '{}' reading blocks in parallel.",
                    file_path
                );
                let block_prefix_opt: Option<String> = con.hget(&meta_key, "block_prefix").await?;
                let block_prefix = block_prefix_opt.ok_or_else(|| {
                    SqueezefsError::InvalidOperation(
                        "Missing block_prefix for striped file".to_string(),
                    )
                })?;
                let num_blocks_opt: Option<u32> = con.hget(&meta_key, "num_blocks").await?;
                let num_blocks = num_blocks_opt.ok_or_else(|| {
                    SqueezefsError::InvalidOperation(
                        "Missing num_blocks for striped file".to_string(),
                    )
                })?;

                let mut futures = Vec::new();
                for i in 0..num_blocks {
                    let block_key = format!("{}/part_{}", block_prefix, i);
                    let backend_clone = self.backend.clone();
                    let task =
                        tokio::spawn(async move { backend_clone.get_object(&block_key).await });
                    futures.push(task);
                }

                let mut file_data = Vec::new();
                for f in futures {
                    let block_data = f.await.map_err(|e| {
                        SqueezefsError::Io(std::io::Error::other(format!(
                            "Stripe download block panicked: {:?}",
                            e
                        )))
                    })??;
                    file_data.extend_from_slice(&block_data);
                }
                file_data
            }
            _ => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "Unknown file type: {}",
                    file_type
                )))
            }
        };

        // Cache in Tier 2: System RAM
        self.cache.lru.put(file_path, data.clone());

        Ok(data)
    }

    /// Retrieve the file size from metadata.
    pub async fn get_file_size(&self, file_path: &str) -> Result<u64> {
        let mut con = self
            .dlm
            .redis_client()
            .get_multiplexed_tokio_connection()
            .await?;
        let meta_key = format!("metadata:{}", file_path);
        let size: Option<u64> = con.hget(&meta_key, "size").await?;
        size.ok_or_else(|| {
            SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("File not found: {}", file_path),
            ))
        })
    }

    pub fn cache(&self) -> &TieredCache {
        &self.cache
    }
}
