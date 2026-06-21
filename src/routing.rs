use crate::backend::RustFsClient;
use crate::cache::TieredCache;
use crate::dlm::DlmClient;
use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use log::debug;
use redis::AsyncCommands;
use std::sync::atomic::Ordering;
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

    /// Write file data using progressive data layout routing with offset support (POSIX random-access RMW).
    pub async fn write_file(
        &self,
        file_path: &str,
        offset: u64,
        data: &[u8],
        fencing_token: u64,
    ) -> Result<()> {
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);

        let mut con = self.dlm.get_connection().await?;
        let meta_key = format!("metadata:{}", file_path);

        let file_type: Option<String> = con.hget(&meta_key, "type").await?;

        // 1. If file is already striped, perform RMW block-by-block without loading the whole file
        if file_type.as_deref() == Some("striped") {
            self.write_striped(file_path, &meta_key, offset, data, fencing_token, &mut con)
                .await?;
            return Ok(());
        }

        // 2. Fetch existing data for inline or staged layouts
        let mut existing_data = match file_type.as_deref() {
            Some("inline") => {
                let inline_key = format!("inline_data:{}", file_path);
                let bytes: Option<Vec<u8>> = con.get(&inline_key).await?;
                bytes.unwrap_or_default()
            }
            Some("staged") => {
                let file_id_opt: Option<String> = con.hget(&meta_key, "file_id").await?;
                if let Some(file_id) = file_id_opt {
                    if let Some(staged_data) = self.cache.nvme.read_staged(&file_id) {
                        staged_data
                    } else {
                        let mapping_key = format!("mapping:{}", file_id);
                        let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                        let off_val: Option<u64> = con.hget(&mapping_key, "offset").await?;
                        let sz_val: Option<u64> = con.hget(&mapping_key, "size").await?;

                        if let (Some(bk), Some(off), Some(sz)) = (block_key, off_val, sz_val) {
                            let packed_bytes = self.backend.get_object(&bk).await?;
                            let start = off as usize;
                            let end = (off + sz) as usize;
                            packed_bytes[start..end].to_vec()
                        } else {
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };

        // 3. Patch the in-memory buffer
        let end_offset = (offset as usize) + data.len();
        if existing_data.len() < end_offset {
            existing_data.resize(end_offset, 0);
        }
        existing_data[offset as usize..end_offset].copy_from_slice(data);
        let new_size = existing_data.len();

        // 4. Save back with appropriate layout routing
        if new_size < 64 * 1024 {
            // Layout: inline
            let inline_key = format!("inline_data:{}", file_path);
            let mut pipe = redis::pipe();
            pipe.set(&inline_key, &existing_data)
                .hset(&meta_key, "size", new_size)
                .hset(&meta_key, "type", "inline")
                .hset(&meta_key, "fencing_token", fencing_token);

            let old_file_id: Option<String> = if file_type.as_deref() == Some("staged") {
                pipe.hdel(&meta_key, "file_id");
                con.hget(&meta_key, "file_id").await?
            } else {
                None
            };

            let _: () = pipe.query_async(&mut con).await?;

            if let Some(old_id) = old_file_id {
                let old_data_path = self
                    .cache
                    .nvme
                    .get_staged_path(&old_id)
                    .join(format!("{}.data", old_id));
                let old_meta_path = self
                    .cache
                    .nvme
                    .get_staged_path(&old_id)
                    .join(format!("{}.meta", old_id));
                let _ = tokio::fs::remove_file(old_data_path).await;
                let _ = tokio::fs::remove_file(old_meta_path).await;
                let mapping_key = format!("mapping:{}", old_id);
                let _: () = con.del(&mapping_key).await.unwrap_or(());
            }

            self.cache.lru.put(file_path, existing_data);
        } else if new_size <= 4 * 1024 * 1024 {
            // Layout: staged
            let new_file_id = Uuid::new_v4().to_string();
            let mut pipe = redis::pipe();
            pipe.hset(&meta_key, "size", new_size)
                .hset(&meta_key, "type", "staged")
                .hset(&meta_key, "file_id", &new_file_id)
                .hset(&meta_key, "fencing_token", fencing_token);

            if file_type.as_deref() == Some("inline") {
                let inline_key = format!("inline_data:{}", file_path);
                pipe.del(&inline_key);
            }

            let old_file_id: Option<String> = if file_type.as_deref() == Some("staged") {
                con.hget(&meta_key, "file_id").await?
            } else {
                None
            };

            let _: () = pipe.query_async(&mut con).await?;

            // Stage write locally
            self.cache
                .nvme
                .stage_write(file_path, &new_file_id, &existing_data, fencing_token)
                .await?;

            if let Some(old_id) = old_file_id {
                let old_data_path = self
                    .cache
                    .nvme
                    .get_staged_path(&old_id)
                    .join(format!("{}.data", old_id));
                let old_meta_path = self
                    .cache
                    .nvme
                    .get_staged_path(&old_id)
                    .join(format!("{}.meta", old_id));
                let _ = tokio::fs::remove_file(old_data_path).await;
                let _ = tokio::fs::remove_file(old_meta_path).await;
                let mapping_key = format!("mapping:{}", old_id);
                let _: () = con.del(&mapping_key).await.unwrap_or(());
            }

            self.cache.lru.put(file_path, existing_data);
        } else {
            // Layout: striped
            let file_uuid = Uuid::new_v4().to_string();
            let block_size = 4 * 1024 * 1024;
            let mut futures = Vec::new();
            let mut offset_cursor = 0;
            let mut block_count = 0;

            while offset_cursor < new_size {
                let end = std::cmp::min(offset_cursor + block_size, new_size);
                let chunk = existing_data[offset_cursor..end].to_vec();
                let block_key = format!("blocks/{}/part_{}", file_uuid, block_count);

                let backend_clone = self.backend.clone();
                let task = tokio::spawn(async move {
                    backend_clone
                        .put_object(&block_key, chunk, fencing_token)
                        .await
                });

                futures.push(task);
                offset_cursor = end;
                block_count += 1;
            }

            for f in futures {
                f.await.map_err(|e| {
                    SqueezefsError::Io(std::io::Error::other(format!(
                        "Stripe upload task failed: {:?}",
                        e
                    )))
                })??;
            }

            let mut pipe = redis::pipe();
            pipe.hset(&meta_key, "size", new_size)
                .hset(&meta_key, "type", "striped")
                .hset(&meta_key, "block_prefix", format!("blocks/{}", file_uuid))
                .hset(&meta_key, "num_blocks", block_count)
                .hset(&meta_key, "fencing_token", fencing_token);

            if file_type.as_deref() == Some("inline") {
                let inline_key = format!("inline_data:{}", file_path);
                pipe.del(&inline_key);
            }

            let old_file_id: Option<String> = if file_type.as_deref() == Some("staged") {
                pipe.hdel(&meta_key, "file_id");
                con.hget(&meta_key, "file_id").await?
            } else {
                None
            };

            let _: () = pipe.query_async(&mut con).await?;

            if let Some(old_id) = old_file_id {
                let old_data_path = self
                    .cache
                    .nvme
                    .get_staged_path(&old_id)
                    .join(format!("{}.data", old_id));
                let old_meta_path = self
                    .cache
                    .nvme
                    .get_staged_path(&old_id)
                    .join(format!("{}.meta", old_id));
                let _ = tokio::fs::remove_file(old_data_path).await;
                let _ = tokio::fs::remove_file(old_meta_path).await;
                let mapping_key = format!("mapping:{}", old_id);
                let _: () = con.del(&mapping_key).await.unwrap_or(());
            }

            self.cache.lru.put(file_path, existing_data);
        }

        Ok(())
    }

    /// Perform a highly efficient block-by-block offset write to a striped file, avoiding loading the entire file.
    async fn write_striped(
        &self,
        file_path: &str,
        meta_key: &str,
        offset: u64,
        data: &[u8],
        fencing_token: u64,
        con: &mut crate::dlm::MetaConnection,
    ) -> Result<()> {
        let block_prefix_opt: Option<String> = con.hget(meta_key, "block_prefix").await?;
        let block_prefix = block_prefix_opt.ok_or_else(|| {
            SqueezefsError::InvalidOperation("Missing block_prefix for striped file".to_string())
        })?;

        let num_blocks_opt: Option<u32> = con.hget(meta_key, "num_blocks").await?;
        let num_blocks = num_blocks_opt.unwrap_or(0);

        let size_opt: Option<u64> = con.hget(meta_key, "size").await?;
        let existing_size = size_opt.unwrap_or(0);

        let block_size = 4 * 1024 * 1024; // 4MB
        let end_pos = offset + data.len() as u64;

        let start_block = (offset / block_size) as u32;
        let end_block = if data.is_empty() {
            start_block
        } else {
            ((end_pos - 1) / block_size) as u32
        };

        if data.is_empty() {
            return Ok(());
        }

        // 1. Fill any block gaps if writing far past existing blocks
        for b in num_blocks..start_block {
            let gap_key = format!("{}/part_{}", block_prefix, b);
            let gap_data = vec![0; block_size as usize];
            self.backend
                .put_object(&gap_key, gap_data, fencing_token)
                .await?;
        }

        // 2. Modify only affected blocks
        for b in start_block..=end_block {
            let block_start_file_offset = b as u64 * block_size;
            let block_end_file_offset = block_start_file_offset + block_size;

            let overlap_start = std::cmp::max(block_start_file_offset, offset);
            let overlap_end = std::cmp::min(block_end_file_offset, end_pos);

            let rel_start = (overlap_start - block_start_file_offset) as usize;
            let rel_end = (overlap_end - block_start_file_offset) as usize;

            let data_slice =
                &data[(overlap_start - offset) as usize..(overlap_end - offset) as usize];
            let block_key = format!("{}/part_{}", block_prefix, b);

            let mut block_data = if b < num_blocks {
                self.backend.get_object(&block_key).await?
            } else {
                vec![0; rel_end]
            };

            if block_data.len() < rel_end {
                block_data.resize(rel_end, 0);
            }

            block_data[rel_start..rel_end].copy_from_slice(data_slice);
            self.backend
                .put_object(&block_key, block_data, fencing_token)
                .await?;
        }

        let new_num_blocks = std::cmp::max(num_blocks, end_block + 1);
        let new_size = std::cmp::max(existing_size, end_pos);

        let _: () = redis::pipe()
            .hset(meta_key, "size", new_size)
            .hset(meta_key, "num_blocks", new_num_blocks)
            .hset(meta_key, "fencing_token", fencing_token)
            .query_async(con)
            .await?;

        // If file data is fully cached in unified RAM cache, patch it there too
        if let Some(mut cached_data) = self.cache.lru.get(file_path) {
            let end_offset = end_pos as usize;
            if cached_data.len() < end_offset {
                cached_data.resize(end_offset, 0);
            }
            cached_data[offset as usize..end_offset].copy_from_slice(data);
            self.cache.lru.put(file_path, cached_data);
        }

        Ok(())
    }

    /// Read file data, attempting to satisfy the read via the fastest cache tier.
    pub async fn read_file(&self, file_path: &str) -> Result<Vec<u8>> {
        // Tier 2 check: System RAM LRU Cache
        if let Some(cached_data) = self.cache.lru.get(file_path) {
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            debug!(
                "Routing: Cache hit (Tier 2 - Unified System RAM) for '{}'",
                file_path
            );
            return Ok(cached_data);
        }
        METRICS.cache_misses.fetch_add(1, Ordering::Relaxed);

        // Fetch file metadata from Garnet
        let mut con = self.dlm.get_connection().await?;
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
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
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
        let mut con = self.dlm.get_connection().await?;
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

    pub fn backend(&self) -> &RustFsClient {
        &self.backend
    }
}
