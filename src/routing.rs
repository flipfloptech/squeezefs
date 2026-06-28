use crate::cache::{PooledBuf, TieredCache, BUFFER_POOL};
use crate::dlm::DlmClient;
use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use log::debug;
use redis::AsyncCommands;
use std::sync::atomic::Ordering;

use std::time::{Duration, SystemTime};
use uuid::Uuid;

pub fn parse_inode_from_path(path: &str) -> u64 {
    if path.starts_with("inode_") {
        path.strip_prefix("inode_")
            .unwrap()
            .parse::<u64>()
            .unwrap_or(0)
    } else {
        0
    }
}

#[derive(Clone)]
pub struct CachedMetadata {
    pub file_type: String,
    pub size: u64,
    pub block_map_id: Option<String>,
    pub block_prefix: Option<String>,
    pub file_id: Option<String>,
    pub cached_at: std::time::Instant,
    pub data_key: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct DataRouter {
    pub dlm: DlmClient,
    pub cache: TieredCache,
    pub block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
    pub nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    pub block_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub metadata_cache: moka::sync::Cache<String, CachedMetadata>,
    pub block_map_cache: moka::sync::Cache<(String, u32), (Option<String>, std::time::Instant)>,
    inflight_block_reads: std::sync::Arc<
        dashmap::DashMap<String, tokio::sync::broadcast::Sender<()>, ahash::RandomState>,
    >,
    sequential_read_state:
        std::sync::Arc<dashmap::DashMap<String, (u32, std::time::Instant), ahash::RandomState>>,
    pub crypto:
        std::sync::Arc<once_cell::sync::OnceCell<crate::crypto_compress::CryptoCompressState>>,
    pub prefetcher: std::sync::Arc<IoUringPrefetcher>,
}

struct InflightBlockReadGuard {
    key: String,
    inflight_block_reads: std::sync::Arc<
        dashmap::DashMap<String, tokio::sync::broadcast::Sender<()>, ahash::RandomState>,
    >,
    tx: tokio::sync::broadcast::Sender<()>,
}

impl Drop for InflightBlockReadGuard {
    fn drop(&mut self) {
        // If the sender in the map is still ours, remove it
        self.inflight_block_reads
            .remove_if(&self.key, |_, current| current.same_channel(&self.tx));
        // Notify all waiters by sending a message, ignore errors if no one is listening
        let _ = self.tx.send(());
    }
}

impl DataRouter {
    pub fn new(
        dlm: DlmClient,
        cache: TieredCache,
        block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
        nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    ) -> Self {
        Self {
            dlm,
            cache,
            block_allocator,
            nvme_writer,
            block_size: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(4 * 1024 * 1024)),
            metadata_cache: moka::sync::Cache::builder()
                .max_capacity(100_000)
                .time_to_live(std::time::Duration::from_secs(60))
                .build(),
            block_map_cache: moka::sync::Cache::builder()
                .max_capacity(500_000)
                .time_to_live(std::time::Duration::from_secs(60))
                .build(),
            inflight_block_reads: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            sequential_read_state: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            crypto: std::sync::Arc::new(once_cell::sync::OnceCell::new()),
            prefetcher: std::sync::Arc::new(IoUringPrefetcher::new()),
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

    pub async fn read_nvme_block(&self, offset_str: &str) -> Result<Vec<u8>> {
        let offset = offset_str.parse::<u64>().map_err(|_| {
            crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid block offset",
            ))
        })?;
        let size = self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
        self.nvme_writer.read_block(offset, size).await
    }

    pub fn set_block_size(&self, block_size: u64) {
        self.block_size
            .store(block_size, std::sync::atomic::Ordering::Relaxed);
    }

    async fn fetch_block_from_remote(&self, block_key: &str) -> Result<PooledBuf> {
        let raw = if let Some(dht) = self.cache.nvme.dht_node.get() {
            let client = crate::p2p::P2pClient::new();
            if let Ok(data) = client.download_block_from_peer(dht, block_key).await {
                data
            } else {
                self.read_nvme_block(block_key).await?
            }
        } else {
            self.read_nvme_block(block_key).await?
        };

        let decompressed = self.get_crypto().process_read(&raw)?;
        let mut pooled = BUFFER_POOL.alloc();
        pooled.resize(decompressed.len(), 0);
        pooled.copy_from_slice(&decompressed);
        Ok(pooled)
    }

    pub async fn get_cached_or_fetch_block(&self, block_key: &str) -> Result<PooledBuf> {
        if let Some(cached_block) = self.cache.read_lru.get(block_key) {
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            let mut pooled = BUFFER_POOL.alloc();
            pooled.resize(cached_block.len(), 0);
            pooled.copy_from_slice(&cached_block);
            return Ok(pooled);
        }

        if let Some(cached_block) = self.cache.nvme.read_cached_block(block_key) {
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            self.cache
                .read_lru
                .put(block_key, bytes::Bytes::from(cached_block.clone()));
            let mut pooled = BUFFER_POOL.alloc();
            pooled.resize(cached_block.len(), 0);
            pooled.copy_from_slice(&cached_block);
            return Ok(pooled);
        }

        let (tx, _rx) = tokio::sync::broadcast::channel(1);
        match self.inflight_block_reads.entry(block_key.to_string()) {
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                METRICS.cache_misses.fetch_add(1, Ordering::Relaxed);
                entry.insert(tx.clone());
                let _guard = InflightBlockReadGuard {
                    key: block_key.to_string(),
                    inflight_block_reads: self.inflight_block_reads.clone(),
                    tx,
                };

                let downloaded = self.fetch_block_from_remote(block_key).await?;
                let nvme_clone = self.cache.nvme.clone();
                let bk_clone = block_key.to_string();
                let dl_clone = downloaded.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = nvme_clone.cache_read_block(&bk_clone, &dl_clone);
                });
                self.cache
                    .read_lru
                    .put(block_key, bytes::Bytes::copy_from_slice(&downloaded));
                Ok(downloaded)
            }
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                let tx = entry.get().clone();
                let mut rx = tx.subscribe();
                drop(entry); // Drop the dashmap lock before awaiting!
                let _ = rx.recv().await;
                if let Some(cached_block) = self.cache.read_lru.get(block_key) {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    let mut pooled = BUFFER_POOL.alloc();
                    pooled.resize(cached_block.len(), 0);
                    pooled.copy_from_slice(&cached_block);
                    return Ok(pooled);
                }
                if let Some(cached_block) = self.cache.nvme.read_cached_block(block_key) {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    self.cache
                        .read_lru
                        .put(block_key, bytes::Bytes::from(cached_block.clone()));
                    let mut pooled = BUFFER_POOL.alloc();
                    pooled.resize(cached_block.len(), 0);
                    pooled.copy_from_slice(&cached_block);
                    return Ok(pooled);
                }
                Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Block fetch failed by the primary fetcher task",
                )))
            }
        }
    }

    pub async fn fetch_metadata(&self, file_path: &str) -> Result<CachedMetadata> {
        use std::time::Duration;
        if let Some(entry) = self.metadata_cache.get(file_path) {
            if entry.cached_at.elapsed() < Duration::from_secs(1) {
                return Ok(entry.clone());
            }
        }
        let mut con = self
            .dlm
            .get_connection_for_inode(parse_inode_from_path(file_path))
            .await?;
        let meta_key = format!("metadata:{}", file_path);
        let fields: std::collections::HashMap<String, String> =
            tokio::time::timeout(std::time::Duration::from_secs(2), con.hgetall(&meta_key))
                .await
                .map_err(|_| {
                    SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Redis query timed out",
                    ))
                })??;

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
            data_key: None,
        };
        self.metadata_cache.insert(file_path.to_string(), m.clone());
        Ok(m)
    }

    pub async fn load_striped_block_keys(
        &self,
        file_path: &str,
        meta: &CachedMetadata,
        start_block: u32,
        end_block: u32,
    ) -> Result<Vec<(u32, Option<String>)>> {
        let mut block_keys = Vec::new();

        if let Some(block_map_id) = &meta.block_map_id {
            let block_map_key = format!("block_map:{}", block_map_id);

            let mut blocks_to_query = Vec::new();
            for b in start_block..=end_block {
                let cache_key = (block_map_id.clone(), b);
                if let Some(entry) = self.block_map_cache.get(&cache_key) {
                    let (bk, cached_at) = &entry;
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
                let mut con = self
                    .dlm
                    .get_connection_for_inode(parse_inode_from_path(file_path))
                    .await?;
                let res: Vec<Option<String>> = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    pipe.query_async(&mut con),
                )
                .await
                .map_err(|_| {
                    SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Redis query timed out",
                    ))
                })??;
                for (idx, key_opt) in res.into_iter().enumerate() {
                    let b = blocks_to_query[idx];
                    self.block_map_cache.insert(
                        (block_map_id.clone(), b),
                        (key_opt.clone(), std::time::Instant::now()),
                    );
                    block_keys.push((b, key_opt));
                }
            }
        } else if let Some(block_prefix) = &meta.block_prefix {
            for b in start_block..=end_block {
                block_keys.push((b, Some(format!("{}/part_{}", block_prefix, b))));
            }
        } else {
            return Err(SqueezefsError::InvalidOperation(
                "Missing block_map_id and block_prefix for striped file".to_string(),
            ));
        }

        block_keys.sort_by_key(|(block_idx, _)| *block_idx);
        Ok(block_keys)
    }

    fn should_prefetch_after_striped_read(
        &self,
        file_path: &str,
        start_block: u32,
        end_block: u32,
    ) -> bool {
        let now = std::time::Instant::now();
        let mut should_prefetch = end_block > start_block;

        if !should_prefetch {
            if let Some(previous) = self.sequential_read_state.get(file_path) {
                let (prev_end_block, prev_seen_at) = *previous.value();
                should_prefetch = prev_seen_at.elapsed() < Duration::from_secs(2)
                    && start_block == prev_end_block.saturating_add(1);
            }
        }

        self.sequential_read_state
            .insert(file_path.to_string(), (end_block, now));
        should_prefetch
    }

    fn schedule_striped_prefetch(
        &self,
        file_path: String,
        meta: CachedMetadata,
        next_block: u32,
        block_size: u64,
    ) {
        const PREFETCH_BLOCK_COUNT: u32 = 9;

        let total_blocks = meta.size.div_ceil(block_size) as u32;
        if next_block >= total_blocks {
            return;
        }

        let prefetch_end = std::cmp::min(
            next_block.saturating_add(PREFETCH_BLOCK_COUNT - 1),
            total_blocks.saturating_sub(1),
        );
        let router = self.clone();

        tokio::spawn(async move {
            let block_keys = match router
                .load_striped_block_keys(&file_path, &meta, next_block, prefetch_end)
                .await
            {
                Ok(block_keys) => block_keys,
                Err(err) => {
                    debug!("Prefetch: Failed to resolve striped block keys: {:?}", err);
                    return;
                }
            };

            let mut tasks = Vec::new();
            for (_, block_key_opt) in block_keys {
                if let Some(block_key) = block_key_opt {
                    let router_clone = router.clone();

                    if router_clone.cache.gds.is_available() {
                        if let Some(local_path) = router_clone.cache.gds.get_gds_path(&block_key) {
                            if !local_path.exists() {
                                debug!(
                                    "Prefetch (GDS): Scheduling download for block {}",
                                    block_key
                                );
                                let local_path_clone = local_path.clone();
                                let block_key_clone = block_key.clone();
                                tasks.push(tokio::spawn(async move {
                                    if let Ok(downloaded) =
                                        router_clone.fetch_block_from_remote(&block_key_clone).await
                                    {
                                        if let Err(e) =
                                            tokio::fs::write(&local_path_clone, &*downloaded).await
                                        {
                                            debug!(
                                                "Prefetch (GDS): Failed to write block {}: {:?}",
                                                block_key_clone, e
                                            );
                                        } else {
                                            debug!(
                                                "Prefetch (GDS): Successfully cached block {}",
                                                block_key_clone
                                            );
                                        }
                                    }
                                }));
                                continue;
                            }
                        }
                    }

                    if let Some(guard) = router_clone
                        .cache
                        .nvme
                        .get_cached_read_block_range_zero_copy(&block_key, 0, u32::MAX)
                    {
                        debug!(
                            "Prefetch (io_uring): Scheduling page prefetch for block {}",
                            block_key
                        );
                        let addr = guard.as_ptr() as u64;
                        let len = guard.len();
                        router_clone.prefetcher.prefetch(addr, len);
                    } else {
                        debug!("Prefetch: Scheduling remote fetch for block {}", block_key);
                        let block_key_clone = block_key.clone();
                        tasks.push(tokio::spawn(async move {
                            if let Err(err) = router_clone
                                .get_cached_or_fetch_block(&block_key_clone)
                                .await
                            {
                                debug!(
                                    "Prefetch: Failed to fetch block {}: {:?}",
                                    block_key_clone, err
                                );
                            }
                        }));
                    }
                }
            }
            for task in tasks {
                let _ = task.await;
            }
        });
    }

    async fn decrement_staged_block_refcount(
        &self,
        old_id: &str,
        con: &mut crate::dlm::MetaConnection,
    ) -> Result<()> {
        let mapping_key = format!("mapping:{}", old_id);
        let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
        if let Some(bk) = block_key {
            let refcounts_key_str = crate::fs_key!("block_refcounts");
            let refcounts_key = &refcounts_key_str;
            let current_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
            if let Some(mut r) = current_ref {
                r -= 1;
                if r <= 0 {
                    let _: () = redis::pipe()
                        .hdel(refcounts_key, &bk)
                        .hdel(crate::fs_key!("block_sizes"), &bk)
                        .query_async(con)
                        .await?;
                    if let Ok(offset_u64) = bk.parse::<u64>() {
                        let _ = self.block_allocator.free_block(offset_u64).await;
                    }
                } else {
                    let _: () = con.hset(refcounts_key, &bk, r).await?;
                }
            } else {
                let _: () = con
                    .hdel(crate::fs_key!("block_sizes"), &bk)
                    .await
                    .unwrap_or(());
                if let Ok(offset_u64) = bk.parse::<u64>() {
                    let _ = self.block_allocator.free_block(offset_u64).await;
                }
            }
        }
        Ok(())
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

        let mut con = self
            .dlm
            .get_connection_for_inode(parse_inode_from_path(file_path))
            .await?;
        let meta_key = format!("metadata:{}", file_path);

        let file_type: Option<String> = con.hget(&meta_key, "type").await?;

        // 1. If file is already striped, perform RMW block-by-block without loading the whole file
        if file_type.as_deref() == Some("striped") {
            self.write_striped(
                file_path,
                &meta_key,
                offset,
                bytes::Bytes::copy_from_slice(data),
                fencing_token,
                &mut con,
            )
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
                            let offset_u64 = bk.parse::<u64>().map_err(|_| {
                                crate::error::SqueezefsError::Io(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "Invalid block offset",
                                ))
                            })?;
                            let raw = self
                                .nvme_writer
                                .read_block(offset_u64 + off, sz as usize)
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

        if end_offset > 4 * 1024 * 1024 && file_type.as_deref() != Some("striped") {
            // Transition the existing data (which is at most 4MB) to striped layout
            let file_uuid = Uuid::new_v4().to_string();
            let block_map_id = Uuid::new_v4().to_string();
            let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
            let mut sizes_to_register = Vec::new();
            let mut offset_cursor = 0;
            let mut block_count = 0;
            let mut block_mappings = Vec::new();
            let existing_bytes = bytes::Bytes::from(existing_data);
            let existing_size = existing_bytes.len();
            while offset_cursor < existing_size {
                let end = std::cmp::min(offset_cursor + block_size, existing_size);
                let chunk = existing_bytes.slice(offset_cursor..end);
                let offset = self.block_allocator.allocate_block().await?;
                let stored_block_key = offset.to_string();
                let stored_block_key_clone = stored_block_key.clone();

                block_mappings.push((block_count.to_string(), stored_block_key.clone()));

                let chunk_len = chunk.len();
                let processed = match self.get_crypto().process_write(chunk.clone()) {
                    Ok(p) => p,
                    Err(e) => return Err(e),
                };
                let processed_len = processed.len();
                sizes_to_register.push((stored_block_key_clone.clone(), chunk_len, processed_len));

                let nvme_writer = self.nvme_writer.clone();
                let read_lru = self.cache.read_lru.clone();
                let chunk_clone = chunk.clone();
                tokio::spawn(async move {
                    if let Err(e) = nvme_writer.write_block(offset, &processed).await {
                        log::error!(
                            "Background Stripe upload task failed for block {}: {:?}",
                            stored_block_key,
                            e
                        );
                    }
                    // Cache the newly written block in RAM - dehydrated to NVMe on eviction
                    read_lru.put(&stored_block_key_clone, chunk_clone);
                });

                offset_cursor = end;
                block_count += 1;
            }

            // Register block mappings and reference counts in Garnet
            let block_map_key = format!("block_map:{}", block_map_id);
            let refcounts_key_str = crate::fs_key!("block_refcounts");
            let refcounts_key = &refcounts_key_str;
            let mut pipe_map = redis::pipe();
            for (idx_str, key) in &block_mappings {
                pipe_map.hset(&block_map_key, idx_str, key);
                pipe_map.hset(refcounts_key, key, 1);
            }
            for (key, logical, physical) in &sizes_to_register {
                pipe_map.hset(
                    crate::fs_key!("block_sizes"),
                    key,
                    format!("{}:{}", logical, physical),
                );
            }
            let _: () = pipe_map.query_async(&mut con).await?;

            let mut pipe = redis::pipe();
            pipe.hset(&meta_key, "size", existing_size)
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

            let _: () = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                pipe.query_async(&mut con),
            )
            .await
            .map_err(|_| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Redis query timed out",
                ))
            })??;

            if let Some(old_id) = old_file_id {
                self.cache.nvme.remove_staged(&old_id);
                let _ = self
                    .decrement_staged_block_refcount(&old_id, &mut con)
                    .await;
                let mapping_key = format!("mapping:{}", old_id);
                let _: () = con.del(&mapping_key).await.unwrap_or(());
            }

            // Now that layout is transitioned to "striped", perform the write block-by-block
            self.write_striped(
                file_path,
                &meta_key,
                offset,
                bytes::Bytes::copy_from_slice(data),
                fencing_token,
                &mut con,
            )
            .await?;
            return Ok(());
        }

        if existing_data.len() < end_offset {
            existing_data.resize(end_offset, 0);
        }
        existing_data[offset as usize..end_offset].copy_from_slice(data);
        let new_size = existing_data.len();

        // 4. Save back with appropriate layout routing
        if new_size < 64 * 1024 {
            // Layout: inline
            let inline_key = format!("inline_data:{}", file_path);
            let shared_data = bytes::Bytes::from(existing_data);
            let processed_data = self.get_crypto().process_write(shared_data.clone())?;
            let mut pipe = redis::pipe();
            pipe.set(&inline_key, &processed_data[..])
                .hset(&meta_key, "size", new_size)
                .hset(&meta_key, "type", "inline")
                .hset(&meta_key, "fencing_token", fencing_token);

            let old_file_id: Option<String> = if file_type.as_deref() == Some("staged") {
                pipe.hdel(&meta_key, "file_id");
                con.hget(&meta_key, "file_id").await?
            } else {
                None
            };

            let _: () = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                pipe.query_async(&mut con),
            )
            .await
            .map_err(|_| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Redis query timed out",
                ))
            })??;

            if let Some(old_id) = old_file_id {
                self.cache.nvme.remove_staged(&old_id);
                let _ = self
                    .decrement_staged_block_refcount(&old_id, &mut con)
                    .await;
                let mapping_key = format!("mapping:{}", old_id);
                let _: () = con.del(&mapping_key).await.unwrap_or(());
            }

            self.cache.write_lru.put(file_path, shared_data.clone());
            self.cache.read_lru.put(file_path, shared_data);
        } else if new_size <= 4 * 1024 * 1024 {
            // Layout: staged
            let new_file_id = Uuid::new_v4().to_string();

            let old_file_id: Option<String> = if file_type.as_deref() == Some("staged") {
                con.hget(&meta_key, "file_id").await?
            } else {
                None
            };

            // Stage write locally (fallback to direct backend write if staging is full)
            let stage_res = self
                .cache
                .nvme
                .stage_write(file_path, &new_file_id, &existing_data, fencing_token)
                .await;

            let shared_data = bytes::Bytes::from(existing_data);

            match stage_res {
                Ok(_) => {
                    let mut pipe = redis::pipe();
                    pipe.hset(&meta_key, "size", new_size)
                        .hset(&meta_key, "type", "staged")
                        .hset(&meta_key, "file_id", &new_file_id)
                        .hset(&meta_key, "fencing_token", fencing_token);

                    if file_type.as_deref() == Some("inline") {
                        let inline_key = format!("inline_data:{}", file_path);
                        pipe.del(&inline_key);
                    }
                    let _: () = tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        pipe.query_async(&mut con),
                    )
                    .await
                    .map_err(|_| {
                        SqueezefsError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Redis query timed out",
                        ))
                    })??;

                    if let Some(old_id) = old_file_id {
                        self.cache.nvme.remove_staged(&old_id);
                        let _ = self
                            .decrement_staged_block_refcount(&old_id, &mut con)
                            .await;
                        let mapping_key = format!("mapping:{}", old_id);
                        let _: () = con.del(&mapping_key).await.unwrap_or(());
                    }
                }
                Err(SqueezefsError::Io(ref e)) if e.kind() == std::io::ErrorKind::StorageFull => {
                    log::warn!("NVMe write staging cache full. Falling back to direct synchronous backend block write for: {}", file_path);

                    // 1. Process data (encryption and compression)
                    let processed_data = self.get_crypto().process_write(shared_data.clone())?;

                    // 2. Write block directly to backing device
                    let offset = self.block_allocator.allocate_block().await?;
                    let stored_block_key = offset.to_string();

                    self.nvme_writer
                        .write_block(offset, &processed_data)
                        .await?;

                    // 3. Register type as staged, file_id, and mapping in Garnet
                    let mut pipe = redis::pipe();
                    pipe.hset(&meta_key, "size", new_size)
                        .hset(&meta_key, "type", "staged")
                        .hset(&meta_key, "file_id", &new_file_id)
                        .hset(&meta_key, "fencing_token", fencing_token);

                    if file_type.as_deref() == Some("inline") {
                        let inline_key = format!("inline_data:{}", file_path);
                        pipe.del(&inline_key);
                    }
                    let _: () = tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        pipe.query_async(&mut con),
                    )
                    .await
                    .map_err(|_| {
                        SqueezefsError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Redis query timed out",
                        ))
                    })??;

                    let mapping_key = format!("mapping:{}", new_file_id);
                    let size = processed_data.len() as u64;
                    let _: () = redis::pipe()
                        .hset(&mapping_key, "block", &stored_block_key)
                        .hset(&mapping_key, "offset", 0u64)
                        .hset(&mapping_key, "size", size)
                        .hset(crate::fs_key!("block_refcounts"), &stored_block_key, 1)
                        .hset(
                            crate::fs_key!("block_sizes"),
                            &stored_block_key,
                            format!("{}:{}", shared_data.len(), processed_data.len()),
                        )
                        .query_async(&mut con)
                        .await?;

                    // 4. Remove old staged files if any
                    if let Some(old_id) = old_file_id {
                        self.cache.nvme.remove_staged(&old_id);
                        let _ = self
                            .decrement_staged_block_refcount(&old_id, &mut con)
                            .await;
                        let old_mapping_key = format!("mapping:{}", old_id);
                        let _: () = con.del(&old_mapping_key).await.unwrap_or(());
                    }
                }
                Err(e) => return Err(e),
            }

            self.cache.write_lru.put(file_path, shared_data.clone());
            self.cache.read_lru.put(file_path, shared_data);
        } else {
            // Layout: striped
            let file_uuid = Uuid::new_v4().to_string();
            let block_map_id = Uuid::new_v4().to_string();
            let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
            let mut sizes_to_register = Vec::new();
            let mut offset_cursor = 0;
            let mut block_count = 0;
            let mut block_mappings = Vec::new();
            let existing_bytes = bytes::Bytes::from(existing_data);
            let new_size = existing_bytes.len();
            while offset_cursor < new_size {
                let end = std::cmp::min(offset_cursor + block_size, new_size);
                let chunk = existing_bytes.slice(offset_cursor..end);
                let offset = self.block_allocator.allocate_block().await?;
                let stored_block_key = offset.to_string();
                let stored_block_key_clone = stored_block_key.clone();

                block_mappings.push((block_count.to_string(), stored_block_key.clone()));

                let chunk_len = chunk.len();
                let processed = match self.get_crypto().process_write(chunk.clone()) {
                    Ok(p) => p,
                    Err(e) => return Err(e),
                };
                let processed_len = processed.len();
                sizes_to_register.push((stored_block_key_clone.clone(), chunk_len, processed_len));

                let nvme_writer = self.nvme_writer.clone();
                let read_lru = self.cache.read_lru.clone();
                let chunk_clone = chunk.clone();
                tokio::spawn(async move {
                    if let Err(e) = nvme_writer.write_block(offset, &processed).await {
                        log::error!(
                            "Background Stripe upload task failed for block {}: {:?}",
                            stored_block_key,
                            e
                        );
                    }
                    // Cache the newly written block in RAM - dehydrated to NVMe on eviction
                    read_lru.put(&stored_block_key_clone, chunk_clone);
                });

                offset_cursor = end;
                block_count += 1;
            }

            // Register block mappings and reference counts in Garnet
            let block_map_key = format!("block_map:{}", block_map_id);
            let refcounts_key_str = crate::fs_key!("block_refcounts");
            let refcounts_key = &refcounts_key_str;
            let mut pipe_map = redis::pipe();
            for (idx_str, key) in &block_mappings {
                pipe_map.hset(&block_map_key, idx_str, key);
                pipe_map.hset(refcounts_key, key, 1);
            }
            for (key, logical, physical) in &sizes_to_register {
                pipe_map.hset(
                    crate::fs_key!("block_sizes"),
                    key,
                    format!("{}:{}", logical, physical),
                );
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

            let _: () = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                pipe.query_async(&mut con),
            )
            .await
            .map_err(|_| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Redis query timed out",
                ))
            })??;

            if let Some(old_id) = old_file_id {
                self.cache.nvme.remove_staged(&old_id);
                let mapping_key = format!("mapping:{}", old_id);
                // Decrement refcount of old staged merged block if it exists
                let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                if let Some(bk) = block_key {
                    let current_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
                    if let Some(mut r) = current_ref {
                        r -= 1;
                        if r <= 0 {
                            let _: () = redis::pipe()
                                .hdel(refcounts_key, &bk)
                                .hdel(crate::fs_key!("block_sizes"), &bk)
                                .query_async(&mut con)
                                .await?;
                            if let Ok(offset_u64) = bk.parse::<u64>() {
                                let _ = self.block_allocator.free_block(offset_u64).await;
                            }
                        } else {
                            let _: () = con.hset(refcounts_key, &bk, r).await?;
                        }
                    } else {
                        let _: () = con
                            .hdel(crate::fs_key!("block_sizes"), &bk)
                            .await
                            .unwrap_or(());
                        if let Ok(offset_u64) = bk.parse::<u64>() {
                            let _ = self.block_allocator.free_block(offset_u64).await;
                        }
                    }
                }
                let _: () = con.del(&mapping_key).await.unwrap_or(());
            }

            self.cache.write_lru.remove(file_path);
            self.cache.read_lru.remove(file_path);
        }

        self.metadata_cache.invalidate(file_path);

        Ok(())
    }

    async fn write_striped(
        &self,
        file_path: &str,
        meta_key: &str,
        offset: u64,
        data: bytes::Bytes,
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
                let refcounts_key_str = crate::fs_key!("block_refcounts");
                let refcounts_key = &refcounts_key_str;
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
        let refcounts_key_str = crate::fs_key!("block_refcounts");
        let refcounts_key = &refcounts_key_str;

        // 1. Fill any block gaps: gaps are now supported natively as sparse blocks
        // (i.e. not written to backing device and mapped to None in block map), so no action is required here.

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
                data.slice((overlap_start - offset) as usize..(overlap_end - offset) as usize);

            let router_clone = self.clone();
            let crypto = self.get_crypto().clone();
            let read_lru = self.cache.read_lru.clone();

            let needs_existing = {
                let existing_block_end = std::cmp::min(existing_size, block_end_file_offset);
                existing_block_end > block_start_file_offset
                    && (overlap_start > block_start_file_offset || overlap_end < existing_block_end)
            };

            tasks.push(tokio::spawn(async move {
                let mut block_data = if needs_existing {
                    if let Some(ref bk) = old_block_key {
                        router_clone.get_cached_or_fetch_block(bk).await?
                    } else {
                        let mut pooled = BUFFER_POOL.alloc();
                        pooled.resize(rel_end, 0);
                        pooled
                    }
                } else {
                    let mut pooled = BUFFER_POOL.alloc();
                    pooled.resize(rel_end, 0);
                    pooled
                };

                if block_data.len() < rel_end {
                    block_data.resize(rel_end, 0);
                }

                block_data[rel_start..rel_end].copy_from_slice(&data_slice);

                let file_uuid = Uuid::new_v4().to_string();
                let block_write_uuid = Uuid::new_v4().to_string();
                let new_block_key =
                    format!("blocks/{}/block_{}_{}", file_uuid, b, block_write_uuid);

                let offset = router_clone.block_allocator.allocate_block().await?;
                let stored_new_block_key = offset.to_string();

                let block_bytes = bytes::Bytes::copy_from_slice(&block_data);

                // Cache newly written block in RAM - dehydrated to NVMe on eviction
                read_lru.put(&stored_new_block_key, block_bytes.clone());

                let logical_size = block_bytes.len();
                let processed_block = crypto.process_write(block_bytes)?;
                let physical_size = processed_block.len();
                router_clone
                    .nvme_writer
                    .write_block(offset, &processed_block)
                    .await?;
                debug!(
                    "Writeback: Successfully wrote block {} to backing device",
                    new_block_key
                );

                Ok::<_, SqueezefsError>((
                    b,
                    old_block_key,
                    stored_new_block_key,
                    logical_size,
                    physical_size,
                ))
            }));
        }

        let mut results = Vec::new();
        for task in tasks {
            let res = task.await.map_err(|e| {
                SqueezefsError::Io(std::io::Error::other(format!(
                    "Block write task panicked: {:?}",
                    e
                )))
            })??;
            results.push(res);
        }

        // 4. Build single Redis pipeline to update mappings
        let mut pipe_update = redis::pipe();
        let mut old_keys_to_clean = Vec::new();
        for res in results {
            let (b, old_block_key, new_block_key, logical_size, physical_size) = res;
            pipe_update
                .hset(refcounts_key, &new_block_key, 1)
                .hset(&block_map_key, b.to_string(), &new_block_key)
                .hset(
                    crate::fs_key!("block_sizes"),
                    &new_block_key,
                    format!("{}:{}", logical_size, physical_size),
                );

            if let Some(bk) = old_block_key {
                old_keys_to_clean.push(bk);
            }
        }
        let _: () = pipe_update.query_async(con).await?;

        // Clean up old block keys
        for bk in old_keys_to_clean {
            self.cache.read_lru.remove(&bk);
            let old_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
            if let Some(mut r) = old_ref {
                r -= 1;
                if r <= 0 {
                    let _: () = redis::pipe()
                        .hdel(refcounts_key, &bk)
                        .hdel(crate::fs_key!("block_sizes"), &bk)
                        .query_async(con)
                        .await?;
                    if let Ok(offset_u64) = bk.parse::<u64>() {
                        let _ = self.block_allocator.free_block(offset_u64).await;
                    }
                } else {
                    let _: () = con.hset(refcounts_key, &bk, r).await?;
                }
            } else {
                let _: () = con
                    .hdel(crate::fs_key!("block_sizes"), &bk)
                    .await
                    .unwrap_or(());
                if let Ok(offset_u64) = bk.parse::<u64>() {
                    let _ = self.block_allocator.free_block(offset_u64).await;
                }
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

        // If file data is fully cached in RAM, update/invalidate
        let mut found_data = self.cache.write_lru.get(file_path);
        if found_data.is_none() {
            found_data = self.cache.read_lru.get(file_path);
        }
        if let Some(cached_data) = found_data {
            let end_offset = end_pos as usize;
            let mut data_vec = cached_data.to_vec();
            if data_vec.len() < end_offset {
                data_vec.resize(end_offset, 0);
            }
            data_vec[offset as usize..end_offset].copy_from_slice(&data);
            self.cache
                .write_lru
                .put(file_path, bytes::Bytes::from(data_vec));
            self.cache.read_lru.remove(file_path);
        }

        self.metadata_cache.invalidate(file_path);
        for b in start_block..=end_block {
            self.block_map_cache.invalidate(&(block_map_id.clone(), b));
        }

        Ok(())
    }

    /// Read file data, attempting to satisfy the read via the fastest cache tier.
    pub async fn read_file(&self, file_path: &str) -> Result<Vec<u8>> {
        // Tier 2 check: System RAM LRU Caches
        let mut cached_opt = self.cache.write_lru.get(file_path);
        if cached_opt.is_none() {
            cached_opt = self.cache.read_lru.get(file_path);
        }
        if let Some(cached_data) = cached_opt {
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            debug!(
                "Routing: Cache hit (Tier 2 - System RAM) for '{}'",
                file_path
            );
            return Ok(cached_data.to_vec());
        }
        METRICS.cache_misses.fetch_add(1, Ordering::Relaxed);

        // Fetch file metadata from Garnet
        let mut con = self
            .dlm
            .get_connection_for_inode(parse_inode_from_path(file_path))
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
                        let offset_u64 = bk.parse::<u64>().map_err(|_| {
                            crate::error::SqueezefsError::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "Invalid block offset",
                            ))
                        })?;
                        let raw = self
                            .nvme_writer
                            .read_block(offset_u64 + off, sz as usize)
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
                let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
                let num_blocks_opt: Option<u32> = con.hget(&meta_key, "num_blocks").await?;
                let num_blocks = num_blocks_opt.ok_or_else(|| {
                    SqueezefsError::InvalidOperation(
                        "Missing num_blocks for striped file".to_string(),
                    )
                })?;

                let size_opt: Option<u64> = con.hget(&meta_key, "size").await?;
                let meta_size = size_opt.unwrap_or(0);

                let block_map_id_opt: Option<String> = con.hget(&meta_key, "block_map_id").await?;

                let block_keys = if let Some(block_map_id) = block_map_id_opt {
                    let block_map_key = format!("block_map:{}", block_map_id);
                    let mut keys = Vec::new();
                    let mut pipe = redis::pipe();
                    for i in 0..num_blocks {
                        pipe.hget(&block_map_key, i.to_string());
                    }
                    let res: Vec<Option<String>> = tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        pipe.query_async(&mut con),
                    )
                    .await
                    .map_err(|_| {
                        SqueezefsError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Redis query timed out",
                        ))
                    })??;
                    for key_opt in res {
                        keys.push(key_opt);
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
                        keys.push(Some(format!("{}/part_{}", block_prefix, i)));
                    }
                    keys
                };

                let mut futures = Vec::new();
                for block_key_opt in block_keys {
                    let router = self.clone();
                    let task = tokio::spawn(async move {
                        if let Some(block_key) = block_key_opt {
                            router.get_cached_or_fetch_block(&block_key).await
                        } else {
                            let mut buf = BUFFER_POOL.alloc();
                            buf.resize(block_size as usize, 0);
                            Ok(buf)
                        }
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
                if file_data.len() > meta_size as usize {
                    file_data.truncate(meta_size as usize);
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

        // Cache in Tier 2: System RAM (Read Cache)
        self.cache
            .read_lru
            .put(file_path, bytes::Bytes::from(data.clone()));

        Ok(data)
    }

    /// Read a specific byte range of a file, downloading only the required 4MB blocks.
    pub async fn read_file_range(
        &self,
        file_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        // Tier 2 check: System RAM LRU Caches
        let mut cached_opt = self.cache.write_lru.get(file_path);
        if cached_opt.is_none() {
            cached_opt = self.cache.read_lru.get(file_path);
        }
        if let Some(cached_data) = cached_opt {
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
                let mut con = self
                    .dlm
                    .get_connection_for_inode(parse_inode_from_path(file_path))
                    .await?;
                let meta_key = format!("metadata:{}", file_path);
                let fields: std::collections::HashMap<String, String> =
                    tokio::time::timeout(std::time::Duration::from_secs(2), con.hgetall(&meta_key))
                        .await
                        .map_err(|_| {
                            SqueezefsError::Io(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "Redis query timed out",
                            ))
                        })??;

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
                    data_key: None,
                };
                self.metadata_cache.insert(file_path.to_string(), m.clone());
                m
            }
        };

        match meta.file_type.as_str() {
            "inline" => {
                let mut con = self
                    .dlm
                    .get_connection_for_inode(parse_inode_from_path(file_path))
                    .await?;
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
                    let mut con = self
                        .dlm
                        .get_connection_for_inode(parse_inode_from_path(file_path))
                        .await?;
                    let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                    let off_opt: Option<u64> = con.hget(&mapping_key, "offset").await?;
                    let sz_opt: Option<u64> = con.hget(&mapping_key, "size").await?;

                    if let (Some(bk), Some(off), Some(sz)) = (block_key, off_opt, sz_opt) {
                        let offset_u64 = bk.parse::<u64>().map_err(|_| {
                            crate::error::SqueezefsError::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "Invalid block offset",
                            ))
                        })?;
                        let packed_bytes = self
                            .nvme_writer
                            .read_block(offset_u64 + off, sz as usize)
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

                let block_keys = self
                    .load_striped_block_keys(file_path, &meta, start_block, end_block)
                    .await?;

                // Spawn concurrent tasks to download block data in parallel
                let mut futures = Vec::new();
                for (b_idx, b_key_opt) in block_keys {
                    let router = self.clone();
                    let b_start_offset = b_idx as u64 * block_size;
                    let b_end_offset = b_start_offset + block_size;
                    let slice_start = std::cmp::max(offset, b_start_offset) - b_start_offset;
                    let slice_end = std::cmp::min(end_offset, b_end_offset);
                    let rel_end = slice_end - b_start_offset;
                    let slice_len = (rel_end - slice_start) as u32;
                    let file_path_clone = file_path.to_string();
                    futures.push(tokio::spawn(async move {
                        let cache_key = format!("active_block:{}:block_{}", file_path_clone, b_idx);
                        let block_data = if let Some(active_data) =
                            router.cache.nvme.read_staged(&cache_key)
                        {
                            let start = std::cmp::min(slice_start as usize, active_data.len());
                            let end = std::cmp::min(
                                (slice_start + slice_len as u64) as usize,
                                active_data.len(),
                            );
                            let mut sliced_pooled = BUFFER_POOL.alloc();
                            sliced_pooled.resize(slice_len as usize, 0);
                            sliced_pooled[0..end - start].copy_from_slice(&active_data[start..end]);
                            sliced_pooled
                        } else if let Some(ref b_key) = b_key_opt {
                            if let Some(cached_block) = router.cache.read_lru.get(b_key) {
                                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                                let start = std::cmp::min(slice_start as usize, cached_block.len());
                                let end = std::cmp::min(
                                    (slice_start + slice_len as u64) as usize,
                                    cached_block.len(),
                                );
                                let mut sliced_pooled = BUFFER_POOL.alloc();
                                sliced_pooled.resize(slice_len as usize, 0);
                                sliced_pooled[0..end - start]
                                    .copy_from_slice(&cached_block[start..end]);
                                sliced_pooled
                            } else {
                                let downloaded = router.get_cached_or_fetch_block(b_key).await?;
                                let start = std::cmp::min(slice_start as usize, downloaded.len());
                                let end = std::cmp::min(
                                    (slice_start + slice_len as u64) as usize,
                                    downloaded.len(),
                                );
                                let mut sliced_pooled = BUFFER_POOL.alloc();
                                sliced_pooled.resize(slice_len as usize, 0);
                                sliced_pooled[0..end - start]
                                    .copy_from_slice(&downloaded[start..end]);
                                sliced_pooled
                            }
                        } else {
                            // Hole support: return zero-filled block
                            let mut hole_pooled = BUFFER_POOL.alloc();
                            hole_pooled.resize(slice_len as usize, 0);
                            hole_pooled
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

                if self.should_prefetch_after_striped_read(file_path, start_block, end_block) {
                    self.schedule_striped_prefetch(
                        file_path.to_string(),
                        meta.clone(),
                        end_block.saturating_add(1),
                        block_size,
                    );
                }

                Ok(range_data)
            }
            _ => Err(SqueezefsError::InvalidOperation(format!(
                "Unknown file type: {}",
                meta.file_type
            ))),
        }
    }

    pub async fn read_file_range_zero_copy(
        &self,
        file_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<(
        bytes::Bytes,
        Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    )> {
        // Tier 2 check: System RAM LRU Caches
        let mut cached_opt = self.cache.write_lru.get(file_path);
        if cached_opt.is_none() {
            cached_opt = self.cache.read_lru.get(file_path);
        }
        if let Some(cached_data) = cached_opt {
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            let start = std::cmp::min(offset as usize, cached_data.len());
            let end = std::cmp::min((offset + size as u64) as usize, cached_data.len());
            let data = cached_data.slice(start..end);
            return Ok((data, None));
        }

        // Fetch file metadata from local cache or Garnet
        let meta = self.fetch_metadata(file_path).await?;

        match meta.file_type.as_str() {
            "inline" => {
                let mut con = self
                    .dlm
                    .get_connection_for_inode(parse_inode_from_path(file_path))
                    .await?;
                let inline_key = format!("inline_data:{}", file_path);
                let bytes: Vec<u8> = con.get(&inline_key).await?;
                let decompressed = self.get_crypto().process_read(&bytes)?;
                let start = std::cmp::min(offset as usize, decompressed.len());
                let end = std::cmp::min((offset + size as u64) as usize, decompressed.len());
                let data = bytes::Bytes::from(decompressed[start..end].to_vec());
                Ok((data, None))
            }
            "staged" => {
                let file_id = meta.file_id.as_ref().ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Missing file_id for staged file".to_string())
                })?;

                if let Some(guard) = self.cache.nvme.read_staged_zero_copy(file_id) {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    let start = std::cmp::min(offset as usize, guard.len);
                    let end = std::cmp::min((offset + size as u64) as usize, guard.len);
                    let mut sliced_guard = guard;
                    sliced_guard.offset += start;
                    sliced_guard.len = end - start;
                    let slice: &[u8] = &sliced_guard;
                    let data = unsafe {
                        bytes::Bytes::from_static(std::mem::transmute::<&[u8], &'static [u8]>(
                            slice,
                        ))
                    };
                    Ok((data, Some(std::sync::Arc::new(sliced_guard))))
                } else {
                    let mapping_key = format!("mapping:{}", file_id);
                    let mut con = self
                        .dlm
                        .get_connection_for_inode(parse_inode_from_path(file_path))
                        .await?;
                    let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                    let off_opt: Option<u64> = con.hget(&mapping_key, "offset").await?;
                    let sz_opt: Option<u64> = con.hget(&mapping_key, "size").await?;

                    if let (Some(bk), Some(off), Some(sz)) = (block_key, off_opt, sz_opt) {
                        let offset_u64 = bk.parse::<u64>().map_err(|_| {
                            crate::error::SqueezefsError::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "Invalid block offset",
                            ))
                        })?;
                        let packed_bytes = self
                            .nvme_writer
                            .read_block(offset_u64 + off, sz as usize)
                            .await?;
                        let decompressed = self.get_crypto().process_read(&packed_bytes)?;
                        if offset >= decompressed.len() as u64 {
                            return Ok((bytes::Bytes::new(), None));
                        }
                        let start = offset as usize;
                        let end =
                            std::cmp::min((offset + size as u64) as usize, decompressed.len());
                        let data = bytes::Bytes::from(decompressed[start..end].to_vec());
                        Ok((data, None))
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
                    return Ok((bytes::Bytes::new(), None));
                }

                let end_offset = std::cmp::min(offset + size as u64, file_size);
                if offset >= end_offset {
                    return Ok((bytes::Bytes::new(), None));
                }

                let start_block = (offset / block_size) as u32;
                let end_block = ((end_offset - 1) / block_size) as u32;

                // 1. Single block read optimization: check cache and staging zero-copy
                if start_block == end_block {
                    let b_idx = start_block;
                    let b_start_offset = b_idx as u64 * block_size;
                    let slice_start = offset - b_start_offset;
                    let slice_len = (end_offset - offset) as u32;
                    let cache_key = format!("active_block:{}:block_{}", file_path, b_idx);

                    // Check active block staging first
                    if let Some(guard) = self.cache.nvme.read_staged_zero_copy(&cache_key) {
                        let start = std::cmp::min(slice_start as usize, guard.len);
                        let end =
                            std::cmp::min((slice_start + slice_len as u64) as usize, guard.len);
                        let mut sliced_guard = guard;
                        sliced_guard.offset += start;
                        sliced_guard.len = end - start;
                        let slice: &[u8] = &sliced_guard;
                        let data = unsafe {
                            bytes::Bytes::from_static(std::mem::transmute::<&[u8], &'static [u8]>(
                                slice,
                            ))
                        };
                        return Ok((data, Some(std::sync::Arc::new(sliced_guard))));
                    }

                    // Check NVMe read block cache next
                    let block_keys = self
                        .load_striped_block_keys(file_path, &meta, start_block, end_block)
                        .await?;
                    if let Some((_, Some(ref b_key))) = block_keys.first() {
                        if let Some(guard) = self.cache.nvme.get_cached_read_block_range_zero_copy(
                            b_key,
                            slice_start,
                            slice_len,
                        ) {
                            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                            let slice: &[u8] = &guard;
                            let data = unsafe {
                                bytes::Bytes::from_static(
                                    std::mem::transmute::<&[u8], &'static [u8]>(slice),
                                )
                            };
                            return Ok((data, Some(std::sync::Arc::new(guard))));
                        }
                    }
                }

                // 2. Multi-block or cache miss: load and assemble using pooled buffer
                let block_keys = self
                    .load_striped_block_keys(file_path, &meta, start_block, end_block)
                    .await?;

                // Spawn concurrent tasks to download block data in parallel
                let mut futures = Vec::new();
                for (b_idx, b_key_opt) in block_keys {
                    let router = self.clone();
                    let b_start_offset = b_idx as u64 * block_size;
                    let b_end_offset = b_start_offset + block_size;
                    let slice_start = std::cmp::max(offset, b_start_offset) - b_start_offset;
                    let slice_end = std::cmp::min(end_offset, b_end_offset);
                    let rel_end = slice_end - b_start_offset;
                    let slice_len = (rel_end - slice_start) as u32;
                    let file_path_clone = file_path.to_string();
                    futures.push(tokio::spawn(async move {
                        let cache_key = format!("active_block:{}:block_{}", file_path_clone, b_idx);
                        let block_data = if let Some(active_data) =
                            router.cache.nvme.read_staged(&cache_key)
                        {
                            let start = std::cmp::min(slice_start as usize, active_data.len());
                            let end = std::cmp::min(
                                (slice_start + slice_len as u64) as usize,
                                active_data.len(),
                            );
                            let mut sliced_pooled = BUFFER_POOL.alloc();
                            sliced_pooled.resize(slice_len as usize, 0);
                            sliced_pooled[0..end - start].copy_from_slice(&active_data[start..end]);
                            sliced_pooled
                        } else if let Some(ref b_key) = b_key_opt {
                            if let Some(cached_block) = router.cache.read_lru.get(b_key) {
                                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                                let start = std::cmp::min(slice_start as usize, cached_block.len());
                                let end = std::cmp::min(
                                    (slice_start + slice_len as u64) as usize,
                                    cached_block.len(),
                                );
                                let mut sliced_pooled = BUFFER_POOL.alloc();
                                sliced_pooled.resize(slice_len as usize, 0);
                                sliced_pooled[0..end - start]
                                    .copy_from_slice(&cached_block[start..end]);
                                sliced_pooled
                            } else {
                                let downloaded = router.get_cached_or_fetch_block(b_key).await?;
                                let start = std::cmp::min(slice_start as usize, downloaded.len());
                                let end = std::cmp::min(
                                    (slice_start + slice_len as u64) as usize,
                                    downloaded.len(),
                                );
                                let mut sliced_pooled = BUFFER_POOL.alloc();
                                sliced_pooled.resize(slice_len as usize, 0);
                                sliced_pooled[0..end - start]
                                    .copy_from_slice(&downloaded[start..end]);
                                sliced_pooled
                            }
                        } else {
                            // Hole support: return zero-filled block
                            let mut hole_pooled = BUFFER_POOL.alloc();
                            hole_pooled.resize(slice_len as usize, 0);
                            hole_pooled
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

                let mut final_buf = BUFFER_POOL.alloc();
                final_buf.resize((end_offset - offset) as usize, 0);
                for (b_idx, block_data) in results_sorted {
                    let b_start_offset = b_idx as u64 * block_size;
                    let b_end_offset = b_start_offset + block_size;
                    let slice_start = std::cmp::max(offset, b_start_offset);
                    let slice_end = std::cmp::min(end_offset, b_end_offset);
                    let dest_start = (slice_start - offset) as usize;
                    let dest_end = (slice_end - offset) as usize;
                    let copy_len = dest_end - dest_start;
                    let src_len = std::cmp::min(copy_len, block_data.len());
                    final_buf[dest_start..dest_start + src_len]
                        .copy_from_slice(&block_data[..src_len]);
                }

                if self.should_prefetch_after_striped_read(file_path, start_block, end_block) {
                    self.schedule_striped_prefetch(
                        file_path.to_string(),
                        meta.clone(),
                        end_block.saturating_add(1),
                        block_size,
                    );
                }

                let slice: &[u8] = &final_buf[..(end_offset - offset) as usize];
                let data = unsafe {
                    bytes::Bytes::from_static(std::mem::transmute::<&[u8], &'static [u8]>(slice))
                };
                Ok((data, Some(std::sync::Arc::new(final_buf))))
            }
            _ => Err(SqueezefsError::InvalidOperation(format!(
                "Unknown file type: {}",
                meta.file_type
            ))),
        }
    }

    /// Retrieve the file size from metadata.
    pub async fn get_file_size(&self, file_path: &str) -> Result<u64> {
        let mut con = self
            .dlm
            .get_connection_for_inode(parse_inode_from_path(file_path))
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

        let mut src_con =
            if let Some(src_ino) = crate::dlm::parse_inode_from_key(&format!("metadata:{}", src)) {
                self.dlm.get_connection_for_inode(src_ino).await?
            } else {
                self.dlm.get_connection().await?
            };
        let mut dest_con = if let Some(dest_ino) =
            crate::dlm::parse_inode_from_key(&format!("metadata:{}", dest))
        {
            self.dlm.get_connection_for_inode(dest_ino).await?
        } else {
            self.dlm.get_connection().await?
        };
        let mut con = self.dlm.get_connection().await?;

        let src_meta_key = format!("metadata:{}", src);
        let dest_meta_key = format!("metadata:{}", dest);

        let exists_src: bool = src_con.exists(&src_meta_key).await?;
        if !exists_src {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Source file not found: {}", src),
            )));
        }

        let exists_dest: bool = dest_con.exists(&dest_meta_key).await?;
        if exists_dest {
            let dest_size_opt: Option<u64> = dest_con.hget(&dest_meta_key, "size").await?;
            let dest_size = dest_size_opt.unwrap_or(0);
            if dest_size > 0 {
                return Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("Destination file already exists and is not empty: {}", dest),
                )));
            }
            let _: () = dest_con.del(&dest_meta_key).await?;
        }

        let file_type: Option<String> = src_con.hget(&src_meta_key, "type").await?;
        let file_type = file_type
            .ok_or_else(|| SqueezefsError::InvalidOperation("Missing file type".to_string()))?;

        if file_type == "inline" {
            let inline_src_key = format!("inline_data:{}", src);
            let inline_dest_key = format!("inline_data:{}", dest);

            let inline_data: Option<Vec<u8>> = src_con.get(&inline_src_key).await?;
            let inline_data = inline_data.unwrap_or_default();
            let size_opt: Option<u64> = src_con.hget(&src_meta_key, "size").await?;
            let size = size_opt.unwrap_or(0);

            let mut pipe = redis::pipe();
            pipe.set(&inline_dest_key, &inline_data)
                .hset(&dest_meta_key, "size", size)
                .hset(&dest_meta_key, "type", "inline")
                .hset(&dest_meta_key, "fencing_token", dest_lock.fencing_token());
            let _: () = pipe.query_async(&mut dest_con).await?;

            let mut cached_opt = self.cache.write_lru.get(src);
            if cached_opt.is_none() {
                cached_opt = self.cache.read_lru.get(src);
            }
            if let Some(cached) = cached_opt {
                self.cache.write_lru.put(dest, cached);
            }
        } else if file_type == "staged" {
            let src_file_id_opt: Option<String> = src_con.hget(&src_meta_key, "file_id").await?;
            let src_file_id = src_file_id_opt.ok_or_else(|| {
                SqueezefsError::InvalidOperation("Missing file_id for staged file".to_string())
            })?;

            let size_opt: Option<u64> = src_con.hget(&src_meta_key, "size").await?;
            let size = size_opt.unwrap_or(0);
            let new_file_id = Uuid::new_v4().to_string();

            // Clone staging data if it's still in the local NVMe cache
            if let Some(data) = self.cache.nvme.read_staged(&src_file_id) {
                self.cache
                    .nvme
                    .stage_write(dest, &new_file_id, &data, dest_lock.fencing_token())
                    .await?;
            }

            let mapping_src_key = format!("mapping:{}", src_file_id);
            let mapping_dest_key = format!("mapping:{}", new_file_id);
            let block: Option<String> = src_con.hget(&mapping_src_key, "block").await?;
            let offset: Option<u64> = src_con.hget(&mapping_src_key, "offset").await?;
            let sz: Option<u64> = src_con.hget(&mapping_src_key, "size").await?;

            if let (Some(ref bk), Some(off), Some(s)) = (&block, offset, sz) {
                let mut map_pipe = redis::pipe();
                map_pipe
                    .hset(&mapping_dest_key, "block", bk)
                    .hset(&mapping_dest_key, "offset", off)
                    .hset(&mapping_dest_key, "size", s);
                let _: () = map_pipe.query_async(&mut dest_con).await?;

                let refcounts_key_str = crate::fs_key!("block_refcounts");
                let refcounts_key = &refcounts_key_str;
                let current_ref: Option<i32> = con.hget(refcounts_key, bk).await?;
                let new_ref = current_ref.unwrap_or(1) + 1;
                let _: () = con.hset(refcounts_key, bk, new_ref).await?;
            }

            let mut dest_pipe = redis::pipe();
            dest_pipe
                .hset(&dest_meta_key, "size", size)
                .hset(&dest_meta_key, "type", "staged")
                .hset(&dest_meta_key, "file_id", &new_file_id)
                .hset(&dest_meta_key, "fencing_token", dest_lock.fencing_token());
            let _: () = dest_pipe.query_async(&mut dest_con).await?;

            let mut cached_opt = self.cache.write_lru.get(src);
            if cached_opt.is_none() {
                cached_opt = self.cache.read_lru.get(src);
            }
            if let Some(cached) = cached_opt {
                self.cache.write_lru.put(dest, cached);
            }
        } else if file_type == "striped" {
            let src_block_map_id_opt: Option<String> =
                src_con.hget(&src_meta_key, "block_map_id").await?;
            let src_block_map_id = match src_block_map_id_opt {
                Some(id) => id,
                None => {
                    let block_prefix_opt: Option<String> =
                        src_con.hget(&src_meta_key, "block_prefix").await?;
                    let block_prefix = block_prefix_opt.ok_or_else(|| {
                        SqueezefsError::InvalidOperation(
                            "Missing block_map_id and block_prefix for striped file".to_string(),
                        )
                    })?;
                    let num_blocks_opt: Option<u32> =
                        src_con.hget(&src_meta_key, "num_blocks").await?;
                    let num_blocks = num_blocks_opt.unwrap_or(0);

                    let new_id = Uuid::new_v4().to_string();
                    let block_map_key = format!("block_map:{}", new_id);
                    let refcounts_key_str = crate::fs_key!("block_refcounts");
                    let refcounts_key = &refcounts_key_str;

                    let mut map_pipe = redis::pipe();
                    for i in 0..num_blocks {
                        let old_key = format!("{}/part_{}", block_prefix, i);
                        map_pipe.hset(&block_map_key, i.to_string(), &old_key);
                    }
                    let _: () = map_pipe.query_async(&mut src_con).await?;

                    let mut ref_pipe = redis::pipe();
                    for i in 0..num_blocks {
                        let old_key = format!("{}/part_{}", block_prefix, i);
                        ref_pipe.hset(refcounts_key, &old_key, 1);
                    }
                    let _: () = ref_pipe.query_async(&mut con).await?;

                    let _: () = src_con.hset(&src_meta_key, "block_map_id", &new_id).await?;
                    new_id
                }
            };

            let size_opt: Option<u64> = src_con.hget(&src_meta_key, "size").await?;
            let size = size_opt.unwrap_or(0);
            let num_blocks_opt: Option<u32> = src_con.hget(&src_meta_key, "num_blocks").await?;
            let num_blocks = num_blocks_opt.unwrap_or(0);

            let dest_block_map_id = Uuid::new_v4().to_string();
            let src_block_map_key = format!("block_map:{}", src_block_map_id);
            let dest_block_map_key = format!("block_map:{}", dest_block_map_id);
            let refcounts_key_str = crate::fs_key!("block_refcounts");
            let refcounts_key = &refcounts_key_str;

            let block_mappings: std::collections::HashMap<String, String> =
                src_con.hgetall(&src_block_map_key).await?;

            let mut pipe = redis::pipe();
            for (idx_str, bk) in &block_mappings {
                pipe.hset(&dest_block_map_key, idx_str, bk);
            }
            let _: () = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                pipe.query_async(&mut dest_con),
            )
            .await
            .map_err(|_| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Redis query timed out",
                ))
            })??;

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
            let _: () = pipe_meta.query_async(&mut dest_con).await?;

            let mut cached_opt = self.cache.write_lru.get(src);
            if cached_opt.is_none() {
                cached_opt = self.cache.read_lru.get(src);
            }
            if let Some(cached) = cached_opt {
                self.cache.write_lru.put(dest, cached);
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
                    let refcounts_key_str = crate::fs_key!("block_refcounts");
                    let refcounts_key = &refcounts_key_str;

                    let block_mappings: std::collections::HashMap<String, String> =
                        con.hgetall(&block_map_key).await?;

                    for (idx_str, bk) in block_mappings {
                        if let Ok(idx) = idx_str.parse::<u32>() {
                            self.block_map_cache
                                .invalidate(&(block_map_id.clone(), idx));
                        }
                        let current_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
                        if let Some(mut r) = current_ref {
                            r -= 1;
                            if r <= 0 {
                                let _: () = redis::pipe()
                                    .hdel(refcounts_key, &bk)
                                    .hdel(crate::fs_key!("block_sizes"), &bk)
                                    .query_async(con)
                                    .await?;
                                if let Ok(offset_u64) = bk.parse::<u64>() {
                                    let _ = self.block_allocator.free_block(offset_u64).await;
                                }
                            } else {
                                let _: () = con.hset(refcounts_key, &bk, r).await?;
                            }
                        } else {
                            let _: () = con
                                .hdel(crate::fs_key!("block_sizes"), &bk)
                                .await
                                .unwrap_or(());
                            if let Ok(offset_u64) = bk.parse::<u64>() {
                                let _ = self.block_allocator.free_block(offset_u64).await;
                            }
                        }
                    }
                    let _: () = con.del(&block_map_key).await?;
                }
            } else if t == "staged" {
                let file_id_opt: Option<String> = con.hget(&meta_key, "file_id").await?;
                if let Some(fid) = file_id_opt {
                    let mapping_key = format!("mapping:{}", fid);
                    let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                    if let Some(bk) = block_key {
                        let refcounts_key_str = crate::fs_key!("block_refcounts");
                        let refcounts_key = &refcounts_key_str;
                        let current_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
                        if let Some(mut r) = current_ref {
                            r -= 1;
                            if r <= 0 {
                                let _: () = redis::pipe()
                                    .hdel(refcounts_key, &bk)
                                    .hdel(crate::fs_key!("block_sizes"), &bk)
                                    .query_async(con)
                                    .await?;
                                if let Ok(offset_u64) = bk.parse::<u64>() {
                                    let _ = self.block_allocator.free_block(offset_u64).await;
                                }
                            } else {
                                let _: () = con.hset(refcounts_key, &bk, r).await?;
                            }
                        } else {
                            let _: () = con
                                .hdel(crate::fs_key!("block_sizes"), &bk)
                                .await
                                .unwrap_or(());
                            if let Ok(offset_u64) = bk.parse::<u64>() {
                                let _ = self.block_allocator.free_block(offset_u64).await;
                            }
                        }
                    }
                    let _: () = con.del(&mapping_key).await?;
                    let nvme_clone = self.cache.nvme.clone();
                    let fid_clone = fid.clone();
                    tokio::task::spawn_blocking(move || {
                        nvme_clone.remove_staged(&fid_clone);
                    });
                }
            }
        }

        // Explicitly clean up stale fields from the metadata key in Redis
        let mut pipe = redis::pipe();
        pipe.hdel(&meta_key, "block_map_id")
            .hdel(&meta_key, "block_prefix")
            .hdel(&meta_key, "num_blocks")
            .hdel(&meta_key, "file_id");
        let _: () = pipe.query_async(con).await.unwrap_or(());

        // Remove any local active write blocks for this inode from the staging segment cache
        let active_block_prefix = format!("active_block:{}:", file_path);
        let keys_to_remove: Vec<String> = self
            .cache
            .nvme
            .list_staged_files()
            .into_iter()
            .filter(|k| k.starts_with(&active_block_prefix))
            .collect();
        for key in keys_to_remove {
            self.cache.nvme.remove_active_block(&key);
        }

        self.cache.write_lru.remove(file_path);
        self.cache.read_lru.remove(file_path);
        self.metadata_cache.invalidate(file_path);
        Ok(())
    }

    /// Resolve a logical filesystem path (e.g., "/dir1/file.txt") to its FUSE inode number.
    pub async fn resolve_path_to_inode(&self, path: &str) -> Result<u64> {
        let mut current_ino = 1u64; // Root inode

        for part in path.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            let mut con = self.dlm.get_connection_for_inode(current_ino).await?;
            let dir_key = format!("{}:dir:{}", crate::fs_prefix(), current_ino);
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

        let mut parent_con = self.dlm.get_connection_for_inode(parent_ino).await?;
        let parent_dir_key = format!("{}:dir:{}", crate::fs_prefix(), parent_ino);

        // Check if destination already exists
        let exists: bool = parent_con.hexists(&parent_dir_key, file_name).await?;
        if exists {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("Destination file '{}' already exists", dest_path),
            )));
        }

        // Generate new inode number on parent's shard connection using shard count step increment
        let shard_count = self.dlm.shard_count() as i64;
        let dest_ino: u64 = parent_con
            .incr(crate::fs_key!("inode_counter"), shard_count)
            .await?;

        // Retrieve attributes of source inode on its respective shard
        let mut src_con = self.dlm.get_connection_for_inode(src_ino).await?;
        let src_attr_key = format!("{}:attr:{}", crate::fs_prefix(), src_ino);
        let size_opt: Option<u64> = src_con.hget(&src_attr_key, "size").await?;
        let size = size_opt.unwrap_or(0);
        let mode_opt: Option<u32> = src_con.hget(&src_attr_key, "mode").await?;
        let mode = mode_opt.unwrap_or(0o644);
        let uid_opt: Option<u32> = src_con.hget(&src_attr_key, "uid").await?;
        let uid = uid_opt.unwrap_or(1000);
        let gid_opt: Option<u32> = src_con.hget(&src_attr_key, "gid").await?;
        let gid = gid_opt.unwrap_or(1000);
        let kind_opt: Option<u8> = src_con.hget(&src_attr_key, "kind").await?;
        let kind = kind_opt.unwrap_or(1); // 1 = Regular file

        // Set attributes of destination inode on parent_con (since dest_ino is on the same shard as parent_ino)
        let dest_attr_key = format!("{}:attr:{}", crate::fs_prefix(), dest_ino);
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
        let _: () = pipe.query_async(&mut parent_con).await?;

        // Clone the underlying data blocks/metadata
        self.clone_file(
            &format!("inode_{}", src_ino),
            &format!("inode_{}", dest_ino),
        )
        .await?;

        Ok(())
    }
}

pub struct IoUringPrefetcher {
    #[cfg(target_os = "linux")]
    tx: tokio::sync::mpsc::Sender<(u64, usize)>,
    pub prefetch_count: std::sync::atomic::AtomicUsize,
}

impl IoUringPrefetcher {
    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<(u64, usize)>(8192);
            std::thread::spawn(move || {
                use io_uring::{opcode, IoUring};
                let mut ring = match IoUring::new(512) {
                    Ok(r) => r,
                    Err(e) => {
                        log::error!("Failed to initialize prefetcher io_uring: {:?}", e);
                        return;
                    }
                };
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    while let Some((addr, len)) = rx.recv().await {
                        let prefetch_e = opcode::Madvise::new(
                            addr as *mut std::ffi::c_void,
                            len as i64,
                            libc::MADV_WILLNEED,
                        )
                        .build()
                        .user_data(0x99);
                        unsafe {
                            if let Err(e) = ring.submission().push(&prefetch_e) {
                                log::debug!("Prefetcher: Failed to push to io_uring: {:?}", e);
                                continue;
                            }
                        }
                        if let Err(e) = ring.submit() {
                            log::debug!("Prefetcher: io_uring submit failed: {:?}", e);
                        }
                        // Reap completions
                        let mut cq = ring.completion();
                        cq.sync();
                        for _ in cq {}
                    }
                });
            });
            Self {
                tx,
                prefetch_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {
                prefetch_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    pub fn prefetch(&self, _addr: u64, _len: usize) {
        self.prefetch_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(target_os = "linux")]
        {
            let _ = self.tx.try_send((_addr, _len));
        }
    }
}

impl Default for IoUringPrefetcher {
    fn default() -> Self {
        Self::new()
    }
}
