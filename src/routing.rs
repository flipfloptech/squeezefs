use crate::backend::{parse_backend_and_key, MultiBackendClient};
use crate::cache::TieredCache;
use crate::dlm::DlmClient;
use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use log::debug;
use redis::AsyncCommands;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use uuid::Uuid;

#[derive(Clone)]
pub struct CachedMetadata {
    pub file_type: String,
    pub size: u64,
    pub block_map_id: Option<String>,
    pub block_prefix: Option<String>,
    pub file_id: Option<String>,
    pub cached_at: std::time::Instant,
}

#[derive(Clone)]
pub struct DataRouter {
    pub dlm: DlmClient,
    pub backend: MultiBackendClient,
    pub cache: TieredCache,
    pub block_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub metadata_cache: std::sync::Arc<dashmap::DashMap<String, CachedMetadata>>,
    pub block_map_cache:
        std::sync::Arc<dashmap::DashMap<(String, u32), (String, std::time::Instant)>>,
    pub crypto:
        std::sync::Arc<once_cell::sync::OnceCell<crate::crypto_compress::CryptoCompressState>>,
}

impl DataRouter {
    pub fn new(dlm: DlmClient, backend: impl Into<MultiBackendClient>, cache: TieredCache) -> Self {
        Self {
            dlm,
            backend: backend.into(),
            cache,
            block_size: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(4 * 1024 * 1024)),
            metadata_cache: std::sync::Arc::new(dashmap::DashMap::new()),
            block_map_cache: std::sync::Arc::new(dashmap::DashMap::new()),
            crypto: std::sync::Arc::new(once_cell::sync::OnceCell::new()),
        }
    }

    pub fn set_crypto(&self, crypto: crate::crypto_compress::CryptoCompressState) {
        let _ = self.crypto.set(crypto);
    }

    pub fn get_crypto(&self) -> &crate::crypto_compress::CryptoCompressState {
        static DEFAULT_CRYPTO: once_cell::sync::Lazy<crate::crypto_compress::CryptoCompressState> =
            once_cell::sync::Lazy::new(|| {
                crate::crypto_compress::CryptoCompressState::new(
                    "none".to_string(),
                    "none".to_string(),
                    None,
                )
            });
        self.crypto.get().unwrap_or(&*DEFAULT_CRYPTO)
    }

    pub fn set_block_size(&self, block_size: u64) {
        self.block_size
            .store(block_size, std::sync::atomic::Ordering::Relaxed);
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
                if let Some(b) = bytes {
                    self.get_crypto().process_read(&b)?
                } else {
                    Vec::new()
                }
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
                            let (be_id, real_key) = parse_backend_and_key(&bk);
                            let raw = self
                                .backend
                                .get_object_range(&be_id, &real_key, off, off + sz)
                                .await?;
                            self.get_crypto().process_read(&raw)?
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
            let processed_data = self.get_crypto().process_write(&existing_data)?;
            let mut pipe = redis::pipe();
            pipe.set(&inline_key, &processed_data)
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
                let old_staged_path = self
                    .cache
                    .nvme
                    .get_staged_path(&old_id)
                    .join(format!("{}.staged", old_id));
                let _ = tokio::fs::remove_file(old_staged_path).await;
                let mapping_key = format!("mapping:{}", old_id);
                let _: () = con.del(&mapping_key).await.unwrap_or(());
            }

            self.cache.lru.put(file_path, Arc::new(existing_data));
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
                let old_staged_path = self
                    .cache
                    .nvme
                    .get_staged_path(&old_id)
                    .join(format!("{}.staged", old_id));
                let _ = tokio::fs::remove_file(old_staged_path).await;
                let mapping_key = format!("mapping:{}", old_id);
                let _: () = con.del(&mapping_key).await.unwrap_or(());
            }

            self.cache.lru.put(file_path, Arc::new(existing_data));
        } else {
            // Layout: striped
            let file_uuid = Uuid::new_v4().to_string();
            let block_map_id = Uuid::new_v4().to_string();
            let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
            let mut futures = Vec::new();
            let mut offset_cursor = 0;
            let mut block_count = 0;
            let mut block_mappings = Vec::new();

            while offset_cursor < new_size {
                let end = std::cmp::min(offset_cursor + block_size, new_size);
                let chunk = existing_data[offset_cursor..end].to_vec();
                let block_write_uuid = Uuid::new_v4().to_string();
                let block_key = format!(
                    "blocks/{}/block_{}_{}",
                    file_uuid, block_count, block_write_uuid
                );
                let active_be = self.backend.get_backend_for_key(&block_key);
                let stored_block_key = format!("{}:{}", active_be, block_key);

                block_mappings.push((block_count.to_string(), stored_block_key));

                let backend_clone = self.backend.clone();
                let crypto = self.get_crypto().clone();
                let task = tokio::spawn(async move {
                    let processed = crypto.process_write(&chunk)?;
                    backend_clone
                        .put_object(&block_key, processed, fencing_token)
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

            // Register block mappings and reference counts in Garnet
            let block_map_key = format!("block_map:{}", block_map_id);
            let refcounts_key = "squeezefs:block_refcounts";
            let mut pipe_map = redis::pipe();
            for (idx_str, key) in &block_mappings {
                pipe_map.hset(&block_map_key, idx_str, key);
                pipe_map.hset(refcounts_key, key, 1);
            }
            let _: () = pipe_map.query_async(&mut con).await?;

            let mut pipe = redis::pipe();
            pipe.hset(&meta_key, "size", new_size)
                .hset(&meta_key, "type", "striped")
                .hset(&meta_key, "block_prefix", format!("blocks/{}", file_uuid))
                .hset(&meta_key, "block_map_id", &block_map_id)
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
                let old_staged_path = self
                    .cache
                    .nvme
                    .get_staged_path(&old_id)
                    .join(format!("{}.staged", old_id));
                let _ = tokio::fs::remove_file(old_staged_path).await;
                let mapping_key = format!("mapping:{}", old_id);
                // Decrement refcount of old staged merged block if it exists
                let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                if let Some(bk) = block_key {
                    let current_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
                    if let Some(mut r) = current_ref {
                        r -= 1;
                        if r <= 0 {
                            let _: () = con.hdel(refcounts_key, &bk).await?;
                            let (be_id, real_key) = parse_backend_and_key(&bk);
                            let _ = self.backend.delete_object(&be_id, &real_key).await;
                        } else {
                            let _: () = con.hset(refcounts_key, &bk, r).await?;
                        }
                    } else {
                        let (be_id, real_key) = parse_backend_and_key(&bk);
                        let _ = self.backend.delete_object(&be_id, &real_key).await;
                    }
                }
                let _: () = con.del(&mapping_key).await.unwrap_or(());
            }

            self.cache.lru.put(file_path, Arc::new(existing_data));
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
        let block_map_id_opt: Option<String> = con.hget(meta_key, "block_map_id").await?;
        let block_map_id = match block_map_id_opt {
            Some(id) => id,
            None => {
                let block_prefix_opt: Option<String> = con.hget(meta_key, "block_prefix").await?;
                let block_prefix = block_prefix_opt.ok_or_else(|| {
                    SqueezefsError::InvalidOperation(
                        "Missing block_map_id and block_prefix for striped file".to_string(),
                    )
                })?;
                let num_blocks_opt: Option<u32> = con.hget(meta_key, "num_blocks").await?;
                let num_blocks = num_blocks_opt.unwrap_or(0);

                let new_id = Uuid::new_v4().to_string();
                let block_map_key = format!("block_map:{}", new_id);
                let refcounts_key = "squeezefs:block_refcounts";
                let mut pipe = redis::pipe();
                for i in 0..num_blocks {
                    let old_key = format!("{}/part_{}", block_prefix, i);
                    pipe.hset(&block_map_key, i.to_string(), &old_key);
                    pipe.hset(refcounts_key, &old_key, 1);
                }
                pipe.hset(meta_key, "block_map_id", &new_id);
                let _: () = pipe.query_async(con).await?;
                new_id
            }
        };

        let num_blocks_opt: Option<u32> = con.hget(meta_key, "num_blocks").await?;
        let num_blocks = num_blocks_opt.unwrap_or(0);

        let size_opt: Option<u64> = con.hget(meta_key, "size").await?;
        let existing_size = size_opt.unwrap_or(0);

        let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
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

        let block_map_key = format!("block_map:{}", block_map_id);
        let refcounts_key = "squeezefs:block_refcounts";

        // 1. Fill any block gaps if writing far past existing blocks
        for b in num_blocks..start_block {
            let gap_write_uuid = Uuid::new_v4().to_string();
            let file_uuid = Uuid::new_v4().to_string();
            let gap_key = format!("blocks/{}/block_{}_{}", file_uuid, b, gap_write_uuid);
            let gap_data = vec![0; block_size as usize];
            let processed_gap = self.get_crypto().process_write(&gap_data)?;
            self.backend
                .put_object(&gap_key, processed_gap, fencing_token)
                .await?;

            let _: () = redis::pipe()
                .hset(refcounts_key, &gap_key, 1)
                .hset(&block_map_key, b.to_string(), &gap_key)
                .query_async(con)
                .await?;
        }

        // 2. Fetch all old block keys in a single pipeline
        let mut pipe = redis::pipe();
        for b in start_block..=end_block {
            pipe.hget(&block_map_key, b.to_string());
        }
        let old_block_keys: Vec<Option<String>> = pipe.query_async(con).await?;

        // 3. Spawn tasks to modify affected blocks concurrently
        let mut tasks = Vec::new();
        for (idx, b) in (start_block..=end_block).enumerate() {
            let old_block_key = old_block_keys[idx].clone();

            let block_start_file_offset = b as u64 * block_size;
            let block_end_file_offset = block_start_file_offset + block_size;

            let overlap_start = std::cmp::max(block_start_file_offset, offset);
            let overlap_end = std::cmp::min(block_end_file_offset, end_pos);

            let rel_start = (overlap_start - block_start_file_offset) as usize;
            let rel_end = (overlap_end - block_start_file_offset) as usize;

            let data_slice =
                data[(overlap_start - offset) as usize..(overlap_end - offset) as usize].to_vec();

            let backend_clone = self.backend.clone();
            let crypto = self.get_crypto().clone();

            tasks.push(tokio::spawn(async move {
                let mut block_data = if let Some(ref bk) = old_block_key {
                    let (be_id, real_key) = parse_backend_and_key(bk);
                    let raw = backend_clone.get_object(&be_id, &real_key).await?;
                    crypto.process_read(&raw)?
                } else {
                    vec![0; rel_end]
                };

                if block_data.len() < rel_end {
                    block_data.resize(rel_end, 0);
                }

                block_data[rel_start..rel_end].copy_from_slice(&data_slice);

                let file_uuid = Uuid::new_v4().to_string();
                let block_write_uuid = Uuid::new_v4().to_string();
                let new_block_key =
                    format!("blocks/{}/block_{}_{}", file_uuid, b, block_write_uuid);

                let processed_block = crypto.process_write(&block_data)?;

                backend_clone
                    .put_object(&new_block_key, processed_block, fencing_token)
                    .await?;

                let active_be = backend_clone.get_backend_for_key(&new_block_key);
                let stored_new_block_key = format!("{}:{}", active_be, new_block_key);

                Ok::<_, SqueezefsError>((b, old_block_key, stored_new_block_key))
            }));
        }

        let results = futures::future::try_join_all(tasks).await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::other(format!(
                "Block write task panicked: {:?}",
                e
            )))
        })?;

        // 4. Build single Redis pipeline to update mappings
        let mut pipe_update = redis::pipe();
        let mut old_keys_to_clean = Vec::new();
        for res in results {
            let (b, old_block_key, new_block_key) = res?;
            pipe_update.hset(refcounts_key, &new_block_key, 1).hset(
                &block_map_key,
                b.to_string(),
                &new_block_key,
            );

            if let Some(bk) = old_block_key {
                old_keys_to_clean.push(bk);
            }
        }
        let _: () = pipe_update.query_async(con).await?;

        // Clean up old block keys
        for bk in old_keys_to_clean {
            let old_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
            if let Some(mut r) = old_ref {
                r -= 1;
                if r <= 0 {
                    let _: () = redis::pipe()
                        .hdel(refcounts_key, &bk)
                        .query_async(con)
                        .await?;
                    let (be_id, real_key) = parse_backend_and_key(&bk);
                    let _ = self.backend.delete_object(&be_id, &real_key).await;
                } else {
                    let _: () = con.hset(refcounts_key, &bk, r).await?;
                }
            } else {
                let (be_id, real_key) = parse_backend_and_key(&bk);
                let _ = self.backend.delete_object(&be_id, &real_key).await;
            }
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
            let data_vec = Arc::make_mut(&mut cached_data);
            if data_vec.len() < end_offset {
                data_vec.resize(end_offset, 0);
            }
            data_vec[offset as usize..end_offset].copy_from_slice(data);
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
            return Ok((*cached_data).clone());
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
                self.get_crypto().process_read(&bytes)?
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
                        let (be_id, real_key) = parse_backend_and_key(&bk);
                        let raw = self
                            .backend
                            .get_object_range(&be_id, &real_key, off, off + sz)
                            .await?;
                        self.get_crypto().process_read(&raw)?
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
                let num_blocks_opt: Option<u32> = con.hget(&meta_key, "num_blocks").await?;
                let num_blocks = num_blocks_opt.ok_or_else(|| {
                    SqueezefsError::InvalidOperation(
                        "Missing num_blocks for striped file".to_string(),
                    )
                })?;

                let block_map_id_opt: Option<String> = con.hget(&meta_key, "block_map_id").await?;

                let block_keys = if let Some(block_map_id) = block_map_id_opt {
                    let block_map_key = format!("block_map:{}", block_map_id);
                    let mut keys = Vec::new();
                    let mut pipe = redis::pipe();
                    for i in 0..num_blocks {
                        pipe.hget(&block_map_key, i.to_string());
                    }
                    let res: Vec<Option<String>> = pipe.query_async(&mut con).await?;
                    for (i, key_opt) in res.into_iter().enumerate() {
                        let bk = key_opt.ok_or_else(|| {
                            SqueezefsError::Io(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                format!("Block {} mapping not found in Garnet", i),
                            ))
                        })?;
                        keys.push(bk);
                    }
                    keys
                } else {
                    let block_prefix_opt: Option<String> =
                        con.hget(&meta_key, "block_prefix").await?;
                    let block_prefix = block_prefix_opt.ok_or_else(|| {
                        SqueezefsError::InvalidOperation(
                            "Missing block_prefix for striped file".to_string(),
                        )
                    })?;
                    let mut keys = Vec::new();
                    for i in 0..num_blocks {
                        keys.push(format!("{}/part_{}", block_prefix, i));
                    }
                    keys
                };

                let mut futures = Vec::new();
                let crypto = self.get_crypto().clone();
                for block_key in block_keys {
                    let backend_clone = self.backend.clone();
                    let crypto_clone = crypto.clone();
                    let task = tokio::spawn(async move {
                        let (be_id, real_key) = parse_backend_and_key(&block_key);
                        let raw = backend_clone.get_object(&be_id, &real_key).await?;
                        crypto_clone.process_read(&raw)
                    });
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
        self.cache.lru.put(file_path, Arc::new(data.clone()));

        Ok(data)
    }

    /// Read a specific byte range of a file, downloading only the required 4MB blocks.
    pub async fn read_file_range(
        &self,
        file_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        // Tier 2 check: System RAM LRU Cache
        if let Some(cached_data) = self.cache.lru.get(file_path) {
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            let start = std::cmp::min(offset as usize, cached_data.len());
            let end = std::cmp::min((offset + size as u64) as usize, cached_data.len());
            return Ok(cached_data[start..end].to_vec());
        }

        // Fetch file metadata from local cache or Garnet
        let cached_meta = if let Some(entry) = self.metadata_cache.get(file_path) {
            if entry.cached_at.elapsed() < Duration::from_secs(1) {
                Some(entry.clone())
            } else {
                None
            }
        } else {
            None
        };

        let meta = match cached_meta {
            Some(m) => m,
            None => {
                let mut con = self.dlm.get_connection().await?;
                let meta_key = format!("metadata:{}", file_path);
                let fields: std::collections::HashMap<String, String> =
                    con.hgetall(&meta_key).await?;

                let file_type = fields.get("type").cloned().ok_or_else(|| {
                    SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("File not found: {}", file_path),
                    ))
                })?;
                let size_val = fields
                    .get("size")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let block_map_id = fields
                    .get("block_map_id")
                    .filter(|s| !s.is_empty())
                    .cloned();
                let block_prefix = fields
                    .get("block_prefix")
                    .filter(|s| !s.is_empty())
                    .cloned();
                let file_id = fields.get("file_id").filter(|s| !s.is_empty()).cloned();

                let m = CachedMetadata {
                    file_type,
                    size: size_val,
                    block_map_id,
                    block_prefix,
                    file_id,
                    cached_at: std::time::Instant::now(),
                };
                self.metadata_cache.insert(file_path.to_string(), m.clone());
                m
            }
        };

        match meta.file_type.as_str() {
            "inline" => {
                let mut con = self.dlm.get_connection().await?;
                let inline_key = format!("inline_data:{}", file_path);
                let bytes: Vec<u8> = con.get(&inline_key).await?;
                let decompressed = self.get_crypto().process_read(&bytes)?;
                let start = std::cmp::min(offset as usize, decompressed.len());
                let end = std::cmp::min((offset + size as u64) as usize, decompressed.len());
                Ok(decompressed[start..end].to_vec())
            }
            "staged" => {
                let file_id = meta.file_id.ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Missing file_id for staged file".to_string())
                })?;

                if let Some(staged_data) = self.cache.nvme.read_staged(&file_id) {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    let start = std::cmp::min(offset as usize, staged_data.len());
                    let end = std::cmp::min((offset + size as u64) as usize, staged_data.len());
                    Ok(staged_data[start..end].to_vec())
                } else {
                    let mapping_key = format!("mapping:{}", file_id);
                    let mut con = self.dlm.get_connection().await?;
                    let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                    let off_opt: Option<u64> = con.hget(&mapping_key, "offset").await?;
                    let sz_opt: Option<u64> = con.hget(&mapping_key, "size").await?;

                    if let (Some(bk), Some(off), Some(sz)) = (block_key, off_opt, sz_opt) {
                        let (be_id, real_key) = parse_backend_and_key(&bk);
                        let packed_bytes = self
                            .backend
                            .get_object_range(&be_id, &real_key, off, off + sz)
                            .await?;
                        let decompressed = self.get_crypto().process_read(&packed_bytes)?;
                        if offset >= decompressed.len() as u64 {
                            return Ok(Vec::new());
                        }
                        let start = offset as usize;
                        let end =
                            std::cmp::min((offset + size as u64) as usize, decompressed.len());
                        Ok(decompressed[start..end].to_vec())
                    } else {
                        Err(SqueezefsError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Staged file ID {} mapping not found in Garnet", file_id),
                        )))
                    }
                }
            }
            "striped" => {
                let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
                let file_size = meta.size;

                if offset >= file_size {
                    return Ok(Vec::new());
                }

                let end_offset = std::cmp::min(offset + size as u64, file_size);
                if offset >= end_offset {
                    return Ok(Vec::new());
                }

                let start_block = (offset / block_size) as u32;
                let end_block = ((end_offset - 1) / block_size) as u32;

                let mut block_keys = Vec::new();
                if let Some(block_map_id) = &meta.block_map_id {
                    let block_map_key = format!("block_map:{}", block_map_id);

                    let mut blocks_to_query = Vec::new();
                    for b in start_block..=end_block {
                        let cache_key = (block_map_id.clone(), b);
                        if let Some(entry) = self.block_map_cache.get(&cache_key) {
                            let (bk, cached_at) = entry.value();
                            if cached_at.elapsed() < Duration::from_secs(1) {
                                block_keys.push((b, bk.clone()));
                                continue;
                            }
                        }
                        blocks_to_query.push(b);
                    }

                    if !blocks_to_query.is_empty() {
                        let mut pipe = redis::pipe();
                        for &b in &blocks_to_query {
                            pipe.hget(&block_map_key, b.to_string());
                        }
                        let mut con = self.dlm.get_connection().await?;
                        let res: Vec<Option<String>> = pipe.query_async(&mut con).await?;
                        for (idx, key_opt) in res.into_iter().enumerate() {
                            let b = blocks_to_query[idx];
                            let bk = key_opt.ok_or_else(|| {
                                SqueezefsError::Io(std::io::Error::new(
                                    std::io::ErrorKind::NotFound,
                                    format!("Block {} mapping not found in Garnet", b),
                                ))
                            })?;
                            self.block_map_cache.insert(
                                (block_map_id.clone(), b),
                                (bk.clone(), std::time::Instant::now()),
                            );
                            block_keys.push((b, bk));
                        }
                    }
                } else if let Some(block_prefix) = &meta.block_prefix {
                    for b in start_block..=end_block {
                        block_keys.push((b, format!("{}/part_{}", block_prefix, b)));
                    }
                } else {
                    return Err(SqueezefsError::InvalidOperation(
                        "Missing block_map_id and block_prefix for striped file".to_string(),
                    ));
                }

                // Pipelined discovery of block peers for cache misses
                let mut cache_misses = Vec::new();
                for (_, b_key) in &block_keys {
                    let safe_name = b_key.replace(['/', ':'], "_");
                    let exists = self
                        .cache
                        .nvme
                        .staging_dirs()
                        .iter()
                        .any(|dir| dir.join(format!("{}.block", safe_name)).exists());
                    if !exists {
                        cache_misses.push(b_key.clone());
                    }
                }

                let mut peer_mappings = std::collections::HashMap::new();
                if !cache_misses.is_empty() {
                    if let Ok(mut con) = self.dlm.get_connection().await {
                        let mut pipe = redis::pipe();
                        for key in &cache_misses {
                            let safe_name = key.replace(['/', ':'], "_");
                            pipe.smembers(format!("block_peers:{}", safe_name));
                        }
                        if let Ok(peer_lists) =
                            pipe.query_async::<_, Vec<Vec<String>>>(&mut con).await
                        {
                            for (idx, list) in peer_lists.into_iter().enumerate() {
                                peer_mappings.insert(cache_misses[idx].clone(), list);
                            }
                        }
                    }
                }

                // Spawn concurrent tasks to download block data in parallel
                let mut futures = Vec::new();
                let crypto = self.get_crypto().clone();
                for (b_idx, b_key) in block_keys {
                    let cache_ref = self.cache.nvme.clone();
                    let backend_ref = self.backend.clone();
                    let peers = peer_mappings.get(&b_key).cloned().unwrap_or_default();
                    let own_p2p_addr = self.cache.nvme.p2p_addr.clone();

                    let b_start_offset = b_idx as u64 * block_size;
                    let b_end_offset = b_start_offset + block_size;
                    let slice_start = std::cmp::max(offset, b_start_offset) - b_start_offset;
                    let slice_end = std::cmp::min(end_offset, b_end_offset);
                    let rel_end = slice_end - b_start_offset;
                    let slice_len = (rel_end - slice_start) as u32;

                    let crypto_clone = crypto.clone();
                    futures.push(tokio::spawn(async move {
                        let block_data = if let Some(cached_range) =
                            cache_ref.get_cached_read_block_range(&b_key, slice_start, slice_len)
                        {
                            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                            cached_range
                        } else {
                            METRICS.cache_misses.fetch_add(1, Ordering::Relaxed);

                            // Try P2P download
                            let mut downloaded_data = None;
                            let client = crate::p2p::P2pClient::new();
                            for peer in &peers {
                                if Some(peer) == own_p2p_addr.get() {
                                    continue;
                                }
                                if let Ok(data) =
                                    client.download_block_from_peer(peer, &b_key).await
                                {
                                    downloaded_data = Some(data);
                                    break;
                                }
                            }

                            let downloaded = match downloaded_data {
                                Some(data) => {
                                    let _ = cache_ref.cache_read_block(&b_key, &data);
                                    data
                                }
                                None => {
                                    // Fallback to S3
                                    let (be_id, real_key) = parse_backend_and_key(&b_key);
                                    let data = backend_ref.get_object(&be_id, &real_key).await?;
                                    let decompressed = crypto_clone.process_read(&data)?;
                                    let _ = cache_ref.cache_read_block(&b_key, &decompressed);
                                    decompressed
                                }
                            };

                            let start = std::cmp::min(slice_start as usize, downloaded.len());
                            let end = std::cmp::min(
                                (slice_start + slice_len as u64) as usize,
                                downloaded.len(),
                            );
                            downloaded[start..end].to_vec()
                        };
                        Ok::<_, SqueezefsError>((b_idx, block_data))
                    }));
                }

                let results = futures::future::try_join_all(futures).await.map_err(|e| {
                    SqueezefsError::Io(std::io::Error::other(format!(
                        "Parallel block download task panicked: {:?}",
                        e
                    )))
                })?;

                let mut results_sorted = Vec::new();
                for res in results {
                    results_sorted.push(res?);
                }
                results_sorted.sort_by_key(|r| r.0);

                let mut range_data = Vec::new();
                for (_, block_data) in results_sorted {
                    range_data.extend_from_slice(&block_data);
                }

                Ok(range_data)
            }
            _ => Err(SqueezefsError::InvalidOperation(format!(
                "Unknown file type: {}",
                meta.file_type
            ))),
        }
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

    pub fn backend(&self) -> &MultiBackendClient {
        &self.backend
    }

    /// Clone a file metadata-only. If it's inline, copy the inline data.
    /// If it's staged, copy the staging folder/files and mapping.
    /// If it's striped, copy the block map and increment all block reference counts.
    pub async fn clone_file(&self, src: &str, dest: &str) -> Result<()> {
        let _src_lock = self
            .dlm
            .acquire_lock(src, None, std::time::Duration::from_secs(5))
            .await?;
        let dest_lock = self
            .dlm
            .acquire_lock(dest, None, std::time::Duration::from_secs(5))
            .await?;

        let mut con = self.dlm.get_connection().await?;
        let src_meta_key = format!("metadata:{}", src);
        let dest_meta_key = format!("metadata:{}", dest);

        let exists_src: bool = con.exists(&src_meta_key).await?;
        if !exists_src {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Source file not found: {}", src),
            )));
        }

        let exists_dest: bool = con.exists(&dest_meta_key).await?;
        if exists_dest {
            let dest_size_opt: Option<u64> = con.hget(&dest_meta_key, "size").await?;
            let dest_size = dest_size_opt.unwrap_or(0);
            if dest_size > 0 {
                return Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("Destination file already exists and is not empty: {}", dest),
                )));
            }
            let _: () = con.del(&dest_meta_key).await?;
        }

        let file_type: Option<String> = con.hget(&src_meta_key, "type").await?;
        let file_type = file_type
            .ok_or_else(|| SqueezefsError::InvalidOperation("Missing file type".to_string()))?;

        if file_type == "inline" {
            let inline_src_key = format!("inline_data:{}", src);
            let inline_dest_key = format!("inline_data:{}", dest);

            let inline_data: Option<Vec<u8>> = con.get(&inline_src_key).await?;
            let inline_data = inline_data.unwrap_or_default();
            let size_opt: Option<u64> = con.hget(&src_meta_key, "size").await?;
            let size = size_opt.unwrap_or(0);

            let mut pipe = redis::pipe();
            pipe.set(&inline_dest_key, &inline_data)
                .hset(&dest_meta_key, "size", size)
                .hset(&dest_meta_key, "type", "inline")
                .hset(&dest_meta_key, "fencing_token", dest_lock.fencing_token());
            let _: () = pipe.query_async(&mut con).await?;

            if let Some(cached) = self.cache.lru.get(src) {
                self.cache.lru.put(dest, cached);
            }
        } else if file_type == "staged" {
            let src_file_id_opt: Option<String> = con.hget(&src_meta_key, "file_id").await?;
            let src_file_id = src_file_id_opt.ok_or_else(|| {
                SqueezefsError::InvalidOperation("Missing file_id for staged file".to_string())
            })?;

            let size_opt: Option<u64> = con.hget(&src_meta_key, "size").await?;
            let size = size_opt.unwrap_or(0);
            let new_file_id = Uuid::new_v4().to_string();

            let src_staged_path = self
                .cache
                .nvme
                .get_staged_path(&src_file_id)
                .join(format!("{}.staged", src_file_id));

            let dest_dir = self.cache.nvme.get_staged_path(&new_file_id);
            tokio::fs::create_dir_all(&dest_dir).await.map_err(|e| {
                SqueezefsError::Io(std::io::Error::other(format!(
                    "Failed to create stage dir for clone: {:?}",
                    e
                )))
            })?;

            let dest_staged_path = dest_dir.join(format!("{}.staged", new_file_id));

            if tokio::fs::metadata(&src_staged_path).await.is_ok() {
                tokio::fs::copy(&src_staged_path, &dest_staged_path)
                    .await
                    .map_err(|e| {
                        SqueezefsError::Io(std::io::Error::other(format!(
                            "Failed to copy stage data: {:?}",
                            e
                        )))
                    })?;
            }

            let mapping_src_key = format!("mapping:{}", src_file_id);
            let mapping_dest_key = format!("mapping:{}", new_file_id);
            let block: Option<String> = con.hget(&mapping_src_key, "block").await?;
            let offset: Option<u64> = con.hget(&mapping_src_key, "offset").await?;
            let sz: Option<u64> = con.hget(&mapping_src_key, "size").await?;

            let mut pipe = redis::pipe();
            if let (Some(ref bk), Some(off), Some(s)) = (&block, offset, sz) {
                pipe.hset(&mapping_dest_key, "block", bk)
                    .hset(&mapping_dest_key, "offset", off)
                    .hset(&mapping_dest_key, "size", s);

                let refcounts_key = "squeezefs:block_refcounts";
                let current_ref: Option<i32> = con.hget(refcounts_key, bk).await?;
                let new_ref = current_ref.unwrap_or(1) + 1;
                pipe.hset(refcounts_key, bk, new_ref);
            }

            pipe.hset(&dest_meta_key, "size", size)
                .hset(&dest_meta_key, "type", "staged")
                .hset(&dest_meta_key, "file_id", &new_file_id)
                .hset(&dest_meta_key, "fencing_token", dest_lock.fencing_token());

            let _: () = pipe.query_async(&mut con).await?;

            if let Some(cached) = self.cache.lru.get(src) {
                self.cache.lru.put(dest, cached);
            }
        } else if file_type == "striped" {
            let src_block_map_id_opt: Option<String> =
                con.hget(&src_meta_key, "block_map_id").await?;
            let src_block_map_id = match src_block_map_id_opt {
                Some(id) => id,
                None => {
                    let block_prefix_opt: Option<String> =
                        con.hget(&src_meta_key, "block_prefix").await?;
                    let block_prefix = block_prefix_opt.ok_or_else(|| {
                        SqueezefsError::InvalidOperation(
                            "Missing block_map_id and block_prefix for striped file".to_string(),
                        )
                    })?;
                    let num_blocks_opt: Option<u32> = con.hget(&src_meta_key, "num_blocks").await?;
                    let num_blocks = num_blocks_opt.unwrap_or(0);

                    let new_id = Uuid::new_v4().to_string();
                    let block_map_key = format!("block_map:{}", new_id);
                    let refcounts_key = "squeezefs:block_refcounts";
                    let mut pipe = redis::pipe();
                    for i in 0..num_blocks {
                        let old_key = format!("{}/part_{}", block_prefix, i);
                        pipe.hset(&block_map_key, i.to_string(), &old_key);
                        pipe.hset(refcounts_key, &old_key, 1);
                    }
                    pipe.hset(&src_meta_key, "block_map_id", &new_id);
                    let _: () = pipe.query_async(&mut con).await?;
                    new_id
                }
            };

            let size_opt: Option<u64> = con.hget(&src_meta_key, "size").await?;
            let size = size_opt.unwrap_or(0);
            let num_blocks_opt: Option<u32> = con.hget(&src_meta_key, "num_blocks").await?;
            let num_blocks = num_blocks_opt.unwrap_or(0);

            let dest_block_map_id = Uuid::new_v4().to_string();
            let src_block_map_key = format!("block_map:{}", src_block_map_id);
            let dest_block_map_key = format!("block_map:{}", dest_block_map_id);
            let refcounts_key = "squeezefs:block_refcounts";

            let block_mappings: std::collections::HashMap<String, String> =
                con.hgetall(&src_block_map_key).await?;

            let mut pipe = redis::pipe();
            for (idx_str, bk) in &block_mappings {
                pipe.hset(&dest_block_map_key, idx_str, bk);
            }
            let _: () = pipe.query_async(&mut con).await?;

            let mut pipe_fetch = redis::pipe();
            for bk in block_mappings.values() {
                pipe_fetch.hget(refcounts_key, bk);
            }
            let current_refs: Vec<Option<i32>> = pipe_fetch.query_async(&mut con).await?;

            let mut pipe_set = redis::pipe();
            for (bk, ref_opt) in block_mappings.values().zip(current_refs) {
                let new_ref = ref_opt.unwrap_or(1) + 1;
                pipe_set.hset(refcounts_key, bk, new_ref);
            }
            let _: () = pipe_set.query_async(&mut con).await?;

            let mut pipe_meta = redis::pipe();
            pipe_meta
                .hset(&dest_meta_key, "size", size)
                .hset(&dest_meta_key, "type", "striped")
                .hset(&dest_meta_key, "block_map_id", &dest_block_map_id)
                .hset(&dest_meta_key, "num_blocks", num_blocks)
                .hset(&dest_meta_key, "fencing_token", dest_lock.fencing_token());
            let _: () = pipe_meta.query_async(&mut con).await?;

            if let Some(cached) = self.cache.lru.get(src) {
                self.cache.lru.put(dest, cached);
            }
        }

        Ok(())
    }

    /// Safely delete all underlying storage files/blocks associated with the file.
    pub async fn delete_file(
        &self,
        file_path: &str,
        con: &mut crate::dlm::MetaConnection,
    ) -> Result<()> {
        let meta_key = format!("metadata:{}", file_path);
        let file_type: Option<String> = con.hget(&meta_key, "type").await?;
        if let Some(t) = file_type {
            if t == "striped" {
                let block_map_id_opt: Option<String> = con.hget(&meta_key, "block_map_id").await?;
                if let Some(block_map_id) = block_map_id_opt {
                    let block_map_key = format!("block_map:{}", block_map_id);
                    let refcounts_key = "squeezefs:block_refcounts";

                    let block_mappings: std::collections::HashMap<String, String> =
                        con.hgetall(&block_map_key).await?;

                    for (_, bk) in block_mappings {
                        let current_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
                        if let Some(mut r) = current_ref {
                            r -= 1;
                            if r <= 0 {
                                let _: () = con.hdel(refcounts_key, &bk).await?;
                                let (be_id, real_key) = parse_backend_and_key(&bk);
                                let _ = self.backend.delete_object(&be_id, &real_key).await;
                            } else {
                                let _: () = con.hset(refcounts_key, &bk, r).await?;
                            }
                        } else {
                            let (be_id, real_key) = parse_backend_and_key(&bk);
                            let _ = self.backend.delete_object(&be_id, &real_key).await;
                        }
                    }
                    let _: () = con.del(&block_map_key).await?;
                } else {
                    let block_prefix_opt: Option<String> =
                        con.hget(&meta_key, "block_prefix").await?;
                    let num_blocks_opt: Option<u32> = con.hget(&meta_key, "num_blocks").await?;
                    if let (Some(bp), Some(nb)) = (block_prefix_opt, num_blocks_opt) {
                        for i in 0..nb {
                            let block_key = format!("{}/part_{}", bp, i);
                            let _ = self.backend.delete_object("backend_0", &block_key).await;
                        }
                    }
                }
            } else if t == "staged" {
                let file_id_opt: Option<String> = con.hget(&meta_key, "file_id").await?;
                if let Some(fid) = file_id_opt {
                    let mapping_key = format!("mapping:{}", fid);
                    let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                    if let Some(bk) = block_key {
                        let refcounts_key = "squeezefs:block_refcounts";
                        let current_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
                        if let Some(mut r) = current_ref {
                            r -= 1;
                            if r <= 0 {
                                let _: () = con.hdel(refcounts_key, &bk).await?;
                                let (be_id, real_key) = parse_backend_and_key(&bk);
                                let _ = self.backend.delete_object(&be_id, &real_key).await;
                            } else {
                                let _: () = con.hset(refcounts_key, &bk, r).await?;
                            }
                        } else {
                            let (be_id, real_key) = parse_backend_and_key(&bk);
                            let _ = self.backend.delete_object(&be_id, &real_key).await;
                        }
                    }
                    let _: () = con.del(&mapping_key).await?;

                    let old_staged_path = self
                        .cache
                        .nvme
                        .get_staged_path(&fid)
                        .join(format!("{}.staged", fid));
                    let _ = tokio::fs::remove_file(old_staged_path).await;
                }
            }
        }
        self.cache.lru.remove(file_path);
        Ok(())
    }

    /// Resolve a logical filesystem path (e.g., "/dir1/file.txt") to its FUSE inode number.
    pub async fn resolve_path_to_inode(&self, path: &str) -> Result<u64> {
        let mut con = self.dlm.get_connection().await?;
        let mut current_ino = 1u64; // Root inode

        for part in path.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            let dir_key = format!("squeezefs:dir:{}", current_ino);
            let next_ino_opt: Option<u64> = con.hget(&dir_key, part).await?;
            match next_ino_opt {
                Some(next_ino) => {
                    current_ino = next_ino;
                }
                None => {
                    return Err(SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!(
                            "Path component '{}' not found in inode {}",
                            part, current_ino
                        ),
                    )));
                }
            }
        }

        Ok(current_ino)
    }

    /// Clone a path to another path metadata-only.
    pub async fn clone_path(&self, src_path: &str, dest_path: &str) -> Result<()> {
        // Resolve source path to inode
        let src_ino = self.resolve_path_to_inode(src_path).await?;

        // Parse dest path into parent path and file name
        let dest_p = std::path::Path::new(dest_path);
        let parent_str = dest_p.parent().and_then(|p| p.to_str()).unwrap_or("");
        let file_name = dest_p.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            SqueezefsError::InvalidOperation("Invalid destination filename".to_string())
        })?;

        // Resolve parent directory to inode
        let parent_ino = self.resolve_path_to_inode(parent_str).await?;

        let mut con = self.dlm.get_connection().await?;
        let parent_dir_key = format!("squeezefs:dir:{}", parent_ino);

        // Check if destination already exists
        let exists: bool = con.hexists(&parent_dir_key, file_name).await?;
        if exists {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("Destination file '{}' already exists", dest_path),
            )));
        }

        // Generate new inode number
        let dest_ino: u64 = con.incr("squeezefs:inode_counter", 1).await?;

        // Retrieve attributes of source inode
        let src_attr_key = format!("squeezefs:attr:{}", src_ino);
        let size_opt: Option<u64> = con.hget(&src_attr_key, "size").await?;
        let size = size_opt.unwrap_or(0);
        let mode_opt: Option<u32> = con.hget(&src_attr_key, "mode").await?;
        let mode = mode_opt.unwrap_or(0o644);
        let uid_opt: Option<u32> = con.hget(&src_attr_key, "uid").await?;
        let uid = uid_opt.unwrap_or(1000);
        let gid_opt: Option<u32> = con.hget(&src_attr_key, "gid").await?;
        let gid = gid_opt.unwrap_or(1000);
        let kind_opt: Option<u8> = con.hget(&src_attr_key, "kind").await?;
        let kind = kind_opt.unwrap_or(1); // 1 = Regular file

        // Set attributes of destination inode
        let dest_attr_key = format!("squeezefs:attr:{}", dest_ino);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        let mut pipe = redis::pipe();
        pipe.hset(&dest_attr_key, "ino", dest_ino)
            .hset(&dest_attr_key, "size", size)
            .hset(&dest_attr_key, "blocks", size.div_ceil(512))
            .hset(&dest_attr_key, "atime_sec", sec)
            .hset(&dest_attr_key, "atime_nsec", nsec)
            .hset(&dest_attr_key, "mtime_sec", sec)
            .hset(&dest_attr_key, "mtime_nsec", nsec)
            .hset(&dest_attr_key, "ctime_sec", sec)
            .hset(&dest_attr_key, "ctime_nsec", nsec)
            .hset(&dest_attr_key, "kind", kind)
            .hset(&dest_attr_key, "mode", mode)
            .hset(&dest_attr_key, "nlink", 1)
            .hset(&dest_attr_key, "uid", uid)
            .hset(&dest_attr_key, "gid", gid)
            .hset(&dest_attr_key, "rdev", 0)
            .hset(&parent_dir_key, file_name, dest_ino);
        let _: () = pipe.query_async(&mut con).await?;

        // Clone the underlying data blocks/metadata
        self.clone_file(
            &format!("inode_{}", src_ino),
            &format!("inode_{}", dest_ino),
        )
        .await?;

        Ok(())
    }
}
