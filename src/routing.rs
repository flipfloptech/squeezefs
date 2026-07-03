use crate::cache::{TieredCache, BUFFER_POOL};
use crate::dlm::DlmClient;
use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use log::debug;
use redis::AsyncCommands;
use std::sync::atomic::Ordering;

use std::time::{Duration, SystemTime};
use uuid::Uuid;

/// Build a Redis pipeline wrapped in MULTI/EXEC for critical metadata mutations (P0-6).
/// Non-atomic pipelines can leave partial key updates if the connection dies mid-batch.
#[inline]
fn atomic_meta_pipe() -> redis::Pipeline {
    let mut pipe = redis::pipe();
    pipe.atomic();
    pipe
}

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

pub struct StorageBackend {
    pub device: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    pub block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
}

#[derive(Clone)]
pub struct BackendRouter {
    pub default_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
    pub default_device: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    pub backends: std::sync::Arc<
        dashmap::DashMap<String, std::sync::Arc<StorageBackend>, ahash::RandomState>,
    >,
    /// P1-11: lock-free active backend id (hot path read).
    pub active_write_backend: std::sync::Arc<arc_swap::ArcSwap<String>>,
    pub block_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[cold]
#[inline(never)]
fn err_invalid_offset() -> crate::error::SqueezefsError {
    crate::error::SqueezefsError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "Invalid block offset",
    ))
}

#[cold]
#[inline(never)]
fn err_backend_not_found(be_id: &str) -> crate::error::SqueezefsError {
    crate::error::SqueezefsError::InvalidOperation(format!("Storage backend '{}' not found", be_id))
}

#[cold]
#[inline(never)]
fn err_active_backend_not_found(be_id: &str) -> crate::error::SqueezefsError {
    crate::error::SqueezefsError::InvalidOperation(format!(
        "Active write backend '{}' not found",
        be_id
    ))
}

impl BackendRouter {
    pub fn new(
        default_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
        default_device: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
        block_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            default_allocator,
            default_device,
            backends: std::sync::Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new())),
            active_write_backend: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                "backend_0".to_string(),
            )),
            block_size,
        }
    }

    pub fn get_active_backend(
        &self,
    ) -> Result<(
        String,
        std::sync::Arc<crate::block_allocator::BlockAllocator>,
        std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    )> {
        let active_be_id = self.active_write_backend.load_full();
        if active_be_id.as_str() == "backend_0" {
            Ok((
                "backend_0".to_string(),
                self.default_allocator.clone(),
                self.default_device.clone(),
            ))
        } else if let Some(be) = self.backends.get(active_be_id.as_str()) {
            Ok((
                (*active_be_id).clone(),
                be.block_allocator.clone(),
                be.device.clone(),
            ))
        } else {
            Err(err_active_backend_not_found(active_be_id.as_str()))
        }
    }

    pub fn get_backend(
        &self,
        be_id: &str,
    ) -> Result<(
        std::sync::Arc<crate::block_allocator::BlockAllocator>,
        std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    )> {
        if be_id == "backend_0" {
            Ok((self.default_allocator.clone(), self.default_device.clone()))
        } else if let Some(be) = self.backends.get(be_id) {
            Ok((be.block_allocator.clone(), be.device.clone()))
        } else {
            Err(err_backend_not_found(be_id))
        }
    }

    pub async fn read_block(&self, block_key: &str, size: usize) -> Result<bytes::Bytes> {
        let parts: Vec<&str> = block_key.split("://").collect();
        let (be_id, offset_str) = if parts.len() > 1 {
            (parts[0], parts[1])
        } else {
            ("backend_0", block_key)
        };

        let offset = offset_str
            .parse::<u64>()
            .map_err(|_| err_invalid_offset())?;

        if be_id == "backend_0" {
            self.default_device.read_block(offset, size).await
        } else if let Some(be) = self.backends.get(be_id) {
            be.device.read_block(offset, size).await
        } else {
            Err(err_backend_not_found(be_id))
        }
    }

    pub async fn free_block(&self, block_key: &str) -> Result<()> {
        let parts: Vec<&str> = block_key.split("://").collect();
        let (be_id, offset_str) = if parts.len() > 1 {
            (parts[0], parts[1])
        } else {
            ("backend_0", block_key)
        };

        let offset = offset_str
            .parse::<u64>()
            .map_err(|_| err_invalid_offset())?;

        if be_id == "backend_0" {
            let _ = self.default_allocator.free_block(offset).await;
        } else if let Some(be) = self.backends.get(be_id) {
            let _ = be.block_allocator.free_block(offset).await;
        }
        Ok(())
    }

    pub async fn free_blocks(&self, block_keys: &[&str]) -> Result<()> {
        if block_keys.is_empty() {
            return Ok(());
        }
        let mut backend_groups: std::collections::HashMap<&str, Vec<u64>> =
            std::collections::HashMap::new();
        for &block_key in block_keys {
            let parts: Vec<&str> = block_key.split("://").collect();
            let (be_id, offset_str) = if parts.len() > 1 {
                (parts[0], parts[1])
            } else {
                ("backend_0", block_key)
            };
            if let Ok(offset) = offset_str.parse::<u64>() {
                backend_groups.entry(be_id).or_default().push(offset);
            }
        }

        for (be_id, offsets) in backend_groups {
            if be_id == "backend_0" {
                let _ = self.default_allocator.free_blocks(&offsets).await;
            } else if let Some(be) = self.backends.get(be_id) {
                let _ = be.block_allocator.free_blocks(&offsets).await;
            }
        }
        Ok(())
    }
}

pub struct DataRouterInner {
    pub dlm: DlmClient,
    pub cache: TieredCache,
    pub block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
    pub nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    pub backend_router: std::sync::Arc<BackendRouter>,
    pub block_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub metadata_cache: moka::sync::Cache<String, CachedMetadata>,
    pub block_map_cache: moka::sync::Cache<(String, u32), (Option<String>, std::time::Instant)>,
    pub(crate) inflight_block_reads:
        std::sync::Arc<scc::HashIndex<String, tokio::sync::broadcast::Sender<()>>>,
    pub(crate) sequential_read_state: moka::sync::Cache<String, (u32, std::time::Instant)>,
    pub crypto:
        std::sync::Arc<once_cell::sync::OnceCell<crate::crypto_compress::CryptoCompressState>>,
    pub prefetcher: std::sync::Arc<IoUringPrefetcher>,
    pub stripe_write_semaphore: std::sync::Arc<tokio::sync::Semaphore>,
}

#[derive(Clone)]
pub struct DataRouter {
    inner: std::sync::Arc<DataRouterInner>,
}

impl std::ops::Deref for DataRouter {
    type Target = DataRouterInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

struct InflightBlockReadGuard {
    key: String,
    inflight_block_reads:
        std::sync::Arc<scc::HashIndex<String, tokio::sync::broadcast::Sender<()>>>,
    tx: tokio::sync::broadcast::Sender<()>,
}

impl Drop for InflightBlockReadGuard {
    fn drop(&mut self) {
        self.inflight_block_reads
            .remove_if_sync(&self.key, |current| current.same_channel(&self.tx));
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
        let block_size = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(4 * 1024 * 1024));
        let backend_router = std::sync::Arc::new(BackendRouter::new(
            block_allocator.clone(),
            nvme_writer.clone(),
            block_size.clone(),
        ));
        cache.set_backend_router(backend_router.clone());

        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let total_memory = sys.total_memory();
        let metadata_capacity = std::cmp::max(10_000, total_memory / 200_000);
        let block_map_capacity = std::cmp::max(50_000, total_memory / 50_000);

        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(16);
        let stripe_permits = cores * 4;
        let stripe_write_semaphore =
            std::sync::Arc::new(tokio::sync::Semaphore::new(stripe_permits));

        Self {
            inner: std::sync::Arc::new(DataRouterInner {
                dlm,
                cache,
                block_allocator,
                nvme_writer,
                backend_router,
                block_size,
                metadata_cache: moka::sync::Cache::builder()
                    .max_capacity(metadata_capacity)
                    .time_to_live(std::time::Duration::from_secs(300))
                    .build(),
                block_map_cache: moka::sync::Cache::builder()
                    .max_capacity(block_map_capacity)
                    .time_to_live(std::time::Duration::from_secs(300))
                    .build(),
                inflight_block_reads: std::sync::Arc::new(scc::HashIndex::new()),
                sequential_read_state: moka::sync::Cache::builder()
                    .max_capacity(100000)
                    .time_to_live(std::time::Duration::from_secs(5))
                    .build(),
                crypto: std::sync::Arc::new(once_cell::sync::OnceCell::new()),
                prefetcher: std::sync::Arc::new(IoUringPrefetcher::new()),
                stripe_write_semaphore,
            }),
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

    pub async fn read_nvme_block(&self, block_key: &str) -> Result<bytes::Bytes> {
        let size = self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
        self.backend_router.read_block(block_key, size).await
    }

    pub fn set_block_size(&self, block_size: u64) {
        self.block_size
            .store(block_size, std::sync::atomic::Ordering::Relaxed);
    }

    async fn fetch_block_from_remote(&self, block_key: &str) -> Result<bytes::Bytes> {
        let raw = if let Some(dht) = self.cache.nvme.dht_node.get() {
            let client = crate::p2p::P2pClient::new();
            if let Ok(data) = client.download_block_from_peer(dht, block_key).await {
                bytes::Bytes::from(data)
            } else {
                self.read_nvme_block(block_key).await?
            }
        } else {
            self.read_nvme_block(block_key).await?
        };

        let decompressed = self.get_crypto().process_read(&raw)?;
        match decompressed {
            std::borrow::Cow::Borrowed(slice) => Ok(bytes::Bytes::copy_from_slice(slice)),
            std::borrow::Cow::Owned(vec) => Ok(bytes::Bytes::from(vec)),
        }
    }

    pub async fn get_cached_or_fetch_block(
        &self,
        block_key: &str,
    ) -> Result<crate::cache::pool::ReadBlockValue> {
        if let Some(cached_block) = self.cache.read_lru.get(block_key) {
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(crate::cache::pool::ReadBlockValue::Bytes(cached_block));
        }

        if let Some(cached_block) = self.cache.nvme.read_cached_block(block_key) {
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            let bytes = bytes::Bytes::from(cached_block);
            self.cache.read_lru.put(block_key, bytes.clone());
            return Ok(crate::cache::pool::ReadBlockValue::Bytes(bytes));
        }

        let (tx, _rx) = tokio::sync::broadcast::channel(1);
        loop {
            if let Some(entry) = self.inflight_block_reads.get_sync(block_key) {
                let tx = entry.get().clone();
                drop(entry);
                let mut rx = tx.subscribe();
                let _ = rx.recv().await;
                if let Some(cached_block) = self.cache.read_lru.get(block_key) {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(crate::cache::pool::ReadBlockValue::Bytes(cached_block));
                }
                if let Some(cached_block) = self.cache.nvme.read_cached_block(block_key) {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    let bytes = bytes::Bytes::from(cached_block);
                    self.cache.read_lru.put(block_key, bytes.clone());
                    return Ok(crate::cache::pool::ReadBlockValue::Bytes(bytes));
                }
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Block fetch failed by the primary fetcher task",
                )));
            }

            match self
                .inflight_block_reads
                .insert_sync(block_key.to_string(), tx.clone())
            {
                Ok(_) => {
                    METRICS.cache_misses.fetch_add(1, Ordering::Relaxed);
                    let _guard = InflightBlockReadGuard {
                        key: block_key.to_string(),
                        inflight_block_reads: self.inflight_block_reads.clone(),
                        tx,
                    };

                    let downloaded = self.fetch_block_from_remote(block_key).await?;
                    let downloaded_bytes = downloaded;
                    if downloaded_bytes.len() < 64 * 1024 {
                        let _ = self
                            .cache
                            .nvme
                            .cache_read_block(block_key, downloaded_bytes.clone());
                    } else {
                        let nvme_clone = self.cache.nvme.clone();
                        let bk_clone = block_key.to_string();
                        let dl_clone = downloaded_bytes.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = nvme_clone.cache_read_block(&bk_clone, dl_clone);
                        });
                    }
                    self.cache.read_lru.put(block_key, downloaded_bytes.clone());
                    return Ok(crate::cache::pool::ReadBlockValue::Bytes(downloaded_bytes));
                }
                Err(_) => {
                    continue;
                }
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
        let meta_key = crate::keys::metadata_for_path(file_path);
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
            let block_map_key = crate::keys::block_map(&block_map_id);

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
                let (prev_end_block, prev_seen_at) = previous;
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

        // P1-5: single admitted outer job; inner fetches limited by buffer_unordered.
        crate::bg_admit::spawn_bg(async move {
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

            use futures::stream::{self, StreamExt};
            let fetches = stream::iter(block_keys.into_iter().filter_map(|(_, block_key_opt)| {
                block_key_opt.map(|block_key| {
                    let router_clone = router.clone();
                    async move {
                        if router_clone.cache.gds.is_available() {
                            if let Some(local_path) =
                                router_clone.cache.gds.get_gds_path(&block_key)
                            {
                                if !local_path.exists() {
                                    debug!(
                                        "Prefetch (GDS): Scheduling download for block {}",
                                        block_key
                                    );
                                    if let Ok(downloaded) =
                                        router_clone.fetch_block_from_remote(&block_key).await
                                    {
                                        if let Err(e) =
                                            tokio::fs::write(&local_path, &*downloaded).await
                                        {
                                            debug!(
                                                "Prefetch (GDS): Failed to write block {}: {:?}",
                                                block_key, e
                                            );
                                        }
                                    }
                                    return;
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
                            if let Err(err) =
                                router_clone.get_cached_or_fetch_block(&block_key).await
                            {
                                debug!("Prefetch: Failed to fetch block {}: {:?}", block_key, err);
                            }
                        }
                    }
                })
            }))
            .buffer_unordered(crate::bg_admit::PREFETCH_BLOCK_CONCURRENCY);

            fetches.for_each(|_| async {}).await;
        });
    }

    async fn decrement_staged_block_refcount(
        &self,
        old_id: &str,
        con: &mut crate::dlm::MetaConnection,
    ) -> Result<()> {
        let mapping_key = crate::keys::mapping(&old_id);
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
                    let _ = self.backend_router.free_block(&bk).await;
                } else {
                    let _: () = con.hset(refcounts_key, &bk, r).await?;
                }
            } else {
                let _: () = con
                    .hdel(crate::fs_key!("block_sizes"), &bk)
                    .await
                    .unwrap_or_else(|e| {
                        log::debug!("non-fatal cleanup op failed: {:?}", e);
                    });
                let _ = self.backend_router.free_block(&bk).await;
            }
        }
        Ok(())
    }

    /// Write file data using progressive data layout routing with offset support (POSIX random-access RMW).
    /// Write path with phased meta connections (P1-10): Redis/Garnet work uses
    /// short-lived connections; durable NVMe / staging I/O never holds a pooled
    /// meta connection across the await.
    pub async fn write_file(
        &self,
        file_path: &str,
        offset: u64,
        data: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<()> {
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);

        let ino = parse_inode_from_path(file_path);
        let meta_key = crate::keys::metadata_for_path(file_path);

        // Deferred NVMe read of a staged payload (mapping captured under meta con).
        enum StagedBackendRead {
            None,
            Mapping {
                block_key: String,
                off: u64,
                sz: u64,
            },
        }

        // --- Meta prep: fencing, type, and cheap existing-data fetch ---
        let (file_type, mut existing_data, staged_backend) = {
            let mut con = self.dlm.get_connection_for_inode(ino).await?;

            let current_fencing: Option<u64> = con.hget(&meta_key, "fencing_token").await?;
            if let Some(cf) = current_fencing {
                if fencing_token < cf {
                    return Err(SqueezefsError::FencingTokenExpired {
                        token: fencing_token,
                        expected: cf,
                    });
                }
            }

            let file_type: Option<String> = con.hget(&meta_key, "type").await?;

            if file_type.as_deref() == Some("striped") {
                drop(con);
                self.write_striped(file_path, &meta_key, offset, data, fencing_token)
                    .await?;
                return Ok(());
            }

            let mut staged_backend = StagedBackendRead::None;
            let existing_data = match file_type.as_deref() {
                Some("inline") => {
                    let inline_key = crate::keys::inline_data(file_path);
                    let bytes: Option<Vec<u8>> = con.get(&inline_key).await?;
                    if let Some(b) = bytes {
                        self.get_crypto().process_read(&b)?.into_owned()
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
                            let mapping_key = crate::keys::mapping(&file_id);
                            let (block_key, off_val, sz_val): (
                                Option<String>,
                                Option<u64>,
                                Option<u64>,
                            ) = redis::cmd("HMGET")
                                .arg(&mapping_key)
                                .arg("block")
                                .arg("offset")
                                .arg("size")
                                .query_async(&mut con)
                                .await?;

                            if let (Some(bk), Some(off), Some(sz)) = (block_key, off_val, sz_val) {
                                staged_backend = StagedBackendRead::Mapping {
                                    block_key: bk,
                                    off,
                                    sz,
                                };
                            }
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            };
            (file_type, existing_data, staged_backend)
        }; // meta con dropped before any backend read

        if let StagedBackendRead::Mapping { block_key, off, sz } = staged_backend {
            let offset_u64 = block_key.parse::<u64>().map_err(|_| {
                crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid block offset",
                ))
            })?;
            let raw = self
                .nvme_writer
                .read_block(offset_u64 + off, sz as usize)
                .await?;
            existing_data = self.get_crypto().process_read(&raw)?.into_owned();
        }

        // Patch the in-memory buffer
        let end_offset = (offset as usize) + data.len();

        let stripe_threshold = if self.cache.nvme.staging_dirs().is_empty() {
            4 * 1024
        } else {
            4 * 1024 * 1024
        };

        if end_offset > stripe_threshold && file_type.as_deref() != Some("striped") {
            // Transition prior layout → striped. Durable I/O first (no meta con),
            // then short-lived con for the atomic type flip (P0-2).
            let file_uuid = Uuid::new_v4().to_string();
            let block_map_id = Uuid::new_v4().to_string();
            let existing_bytes = bytes::Bytes::from(existing_data);
            let existing_size = existing_bytes.len();

            if existing_size == 0 && offset == 0 {
                let (block_mappings, sizes_to_register, block_count) =
                    self.durable_write_stripe_payload(data.clone()).await?;
                let mut con = self.dlm.get_connection_for_inode(ino).await?;
                self.commit_striped_layout_meta(
                    file_path,
                    &meta_key,
                    file_type.as_deref(),
                    &block_map_id,
                    &file_uuid,
                    block_count,
                    data.len(),
                    fencing_token,
                    &block_mappings,
                    &sizes_to_register,
                    &mut con,
                )
                .await?;
                return Ok(());
            }

            let (block_mappings, sizes_to_register, block_count) =
                self.durable_write_stripe_payload(existing_bytes).await?;

            {
                let mut con = self.dlm.get_connection_for_inode(ino).await?;
                self.commit_striped_layout_meta(
                    file_path,
                    &meta_key,
                    file_type.as_deref(),
                    &block_map_id,
                    &file_uuid,
                    block_count,
                    existing_size,
                    fencing_token,
                    &block_mappings,
                    &sizes_to_register,
                    &mut con,
                )
                .await?;
            }

            // Apply the caller write on the now-striped layout (own phased cons).
            self.write_striped(file_path, &meta_key, offset, data.clone(), fencing_token)
                .await?;
            return Ok(());
        }

        if existing_data.len() < end_offset {
            existing_data.resize(end_offset, 0);
        }
        existing_data[offset as usize..end_offset].copy_from_slice(&data);
        let new_size = existing_data.len();

        // Save back with appropriate layout routing
        if new_size < 4 * 1024 {
            // Layout: inline (redis-only after crypto)
            let inline_key = crate::keys::inline_data(file_path);
            let shared_data = bytes::Bytes::from(existing_data);
            let processed_data = self.get_crypto().process_write(shared_data.clone())?;

            let mut con = self.dlm.get_connection_for_inode(ino).await?;
            let old_file_id: Option<String> = if file_type.as_deref() == Some("staged") {
                con.hget(&meta_key, "file_id").await?
            } else {
                None
            };

            let mut pipe = atomic_meta_pipe();
            pipe.set(&inline_key, &processed_data[..])
                .hset(&meta_key, "size", new_size)
                .hset(&meta_key, "type", "inline")
                .hset(&meta_key, "fencing_token", fencing_token);
            if file_type.as_deref() == Some("staged") {
                pipe.hdel(&meta_key, "file_id");
            }

            let _: () = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                pipe.query_async(&mut con),
            )
            .await
            .map_err(|_| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Redis atomic inline metadata commit timed out",
                ))
            })??;

            if let Some(old_id) = old_file_id {
                self.cache.nvme.remove_staged(&old_id);
                let _ = self
                    .decrement_staged_block_refcount(&old_id, &mut con)
                    .await;
                let mapping_key = crate::keys::mapping(&old_id);
                let _: () = con.del(&mapping_key).await.unwrap_or_else(|e| {
                    log::debug!("non-fatal cleanup op failed: {:?}", e);
                });
            }

            self.cache.write_lru.put(file_path, shared_data.clone());
            self.cache.read_lru.put(file_path, shared_data);
        } else if !self.cache.nvme.staging_dirs().is_empty() && new_size <= 4 * 1024 * 1024 {
            // Layout: staged — capture old id, drop con, stage (or backend write), re-acquire for meta.
            let new_file_id = Uuid::new_v4().to_string();

            let old_file_id: Option<String> = if file_type.as_deref() == Some("staged") {
                let mut con = self.dlm.get_connection_for_inode(ino).await?;
                con.hget(&meta_key, "file_id").await?
            } else {
                None
            };

            let stage_res = self
                .cache
                .nvme
                .stage_write(file_path, &new_file_id, &existing_data, fencing_token)
                .await;

            let shared_data = bytes::Bytes::from(existing_data);

            match stage_res {
                Ok(_) => {
                    let mut con = self.dlm.get_connection_for_inode(ino).await?;
                    let mut pipe = atomic_meta_pipe();
                    pipe.hset(&meta_key, "size", new_size)
                        .hset(&meta_key, "type", "staged")
                        .hset(&meta_key, "file_id", &new_file_id)
                        .hset(&meta_key, "fencing_token", fencing_token);

                    if file_type.as_deref() == Some("inline") {
                        let inline_key = crate::keys::inline_data(file_path);
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
                            "Redis atomic staged metadata commit timed out",
                        ))
                    })??;

                    if let Some(old_id) = old_file_id {
                        self.cache.nvme.remove_staged(&old_id);
                        let _ = self
                            .decrement_staged_block_refcount(&old_id, &mut con)
                            .await;
                        let mapping_key = crate::keys::mapping(&old_id);
                        let _: () = con.del(&mapping_key).await.unwrap_or_else(|e| {
                            log::debug!("non-fatal cleanup op failed: {:?}", e);
                        });
                    }
                }
                Err(SqueezefsError::Io(ref e)) if e.kind() == std::io::ErrorKind::StorageFull => {
                    log::warn!("NVMe write staging cache full. Falling back to direct synchronous backend block write for: {}", file_path);

                    let processed_data = self.get_crypto().process_write(shared_data.clone())?;

                    let (be_id, block_allocator, nvme_writer) =
                        self.backend_router.get_active_backend()?;
                    let be_offset = block_allocator.allocate_block().await?;
                    let stored_block_key = if be_id == "backend_0" {
                        be_offset.to_string()
                    } else {
                        format!("{}://{}", be_id, be_offset)
                    };

                    nvme_writer.write_block(be_offset, &processed_data).await?;

                    let mapping_key = crate::keys::mapping(&new_file_id);
                    let size = processed_data.len() as u64;
                    let mut con = self.dlm.get_connection_for_inode(ino).await?;
                    let mut pipe = atomic_meta_pipe();
                    pipe.hset(&meta_key, "size", new_size)
                        .hset(&meta_key, "type", "staged")
                        .hset(&meta_key, "file_id", &new_file_id)
                        .hset(&meta_key, "fencing_token", fencing_token);

                    if file_type.as_deref() == Some("inline") {
                        let inline_key = crate::keys::inline_data(file_path);
                        pipe.del(&inline_key);
                    }
                    pipe.hset(&mapping_key, "block", &stored_block_key)
                        .hset(&mapping_key, "offset", 0u64)
                        .hset(&mapping_key, "size", size)
                        .hset(crate::fs_key!("block_refcounts"), &stored_block_key, 1)
                        .hset(
                            crate::fs_key!("block_sizes"),
                            &stored_block_key,
                            format!("{}:{}", shared_data.len(), processed_data.len()),
                        );
                    let _: () = tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        pipe.query_async(&mut con),
                    )
                    .await
                    .map_err(|_| {
                        SqueezefsError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Redis atomic staged+mapping commit timed out",
                        ))
                    })??;

                    if let Some(old_id) = old_file_id {
                        self.cache.nvme.remove_staged(&old_id);
                        let _ = self
                            .decrement_staged_block_refcount(&old_id, &mut con)
                            .await;
                        let old_mapping_key = crate::keys::mapping(&old_id);
                        let _: () = con.del(&old_mapping_key).await.unwrap_or_else(|e| {
                            log::debug!("non-fatal cleanup op failed: {:?}", e);
                        });
                    }
                }
                Err(e) => return Err(e),
            }

            self.cache.write_lru.put(file_path, shared_data.clone());
            self.cache.read_lru.put(file_path, shared_data);
        } else {
            // First-time striped layout for a fully-buffered image.
            let file_uuid = Uuid::new_v4().to_string();
            let block_map_id = Uuid::new_v4().to_string();
            let existing_bytes = bytes::Bytes::from(existing_data);
            let new_size = existing_bytes.len();

            let (block_mappings, sizes_to_register, block_count) =
                self.durable_write_stripe_payload(existing_bytes).await?;

            let mut con = self.dlm.get_connection_for_inode(ino).await?;
            self.commit_striped_layout_meta(
                file_path,
                &meta_key,
                file_type.as_deref(),
                &block_map_id,
                &file_uuid,
                block_count,
                new_size,
                fencing_token,
                &block_mappings,
                &sizes_to_register,
                &mut con,
            )
            .await?;
        }

        self.metadata_cache.invalidate(file_path);

        Ok(())
    }

    /// Persist `payload` as contiguous stripe blocks on the active backend.
    ///
    /// Every block I/O is **awaited**. On any allocate/crypto/write failure, all
    /// blocks allocated in this call are freed and the error is returned so the
    /// caller can leave the prior layout (inline/staged) untouched.
    async fn durable_write_stripe_payload(
        &self,
        payload: bytes::Bytes,
    ) -> Result<(
        Vec<(String, String)>,       // (block_idx, block_key)
        Vec<(String, usize, usize)>, // (block_key, logical_len, physical_len)
        u32,                         // block_count
    )> {
        let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
        let mut block_mappings: Vec<(String, String)> = Vec::new();
        let mut sizes_to_register: Vec<(String, usize, usize)> = Vec::new();
        let mut allocated_keys: Vec<String> = Vec::new();
        let mut offset_cursor = 0usize;
        let mut block_count = 0u32;
        let payload_len = payload.len();

        while offset_cursor < payload_len {
            let end = std::cmp::min(offset_cursor + block_size, payload_len);
            // SAFETY: offset_cursor < payload_len, end clamped to payload_len
            let chunk = unsafe {
                let sub = payload.get_unchecked(offset_cursor..end);
                payload.slice_ref(sub)
            };

            let (be_id, block_allocator, nvme_writer) =
                match self.backend_router.get_active_backend() {
                    Ok(res) => res,
                    Err(e) => {
                        for k in &allocated_keys {
                            let _ = self.backend_router.free_block(k).await;
                        }
                        return Err(e);
                    }
                };

            let offset = match block_allocator.allocate_block().await {
                Ok(o) => o,
                Err(e) => {
                    for k in &allocated_keys {
                        let _ = self.backend_router.free_block(k).await;
                    }
                    return Err(e);
                }
            };
            let stored_block_key = if be_id == "backend_0" {
                offset.to_string()
            } else {
                format!("{}://{}", be_id, offset)
            };
            allocated_keys.push(stored_block_key.clone());

            let chunk_len = chunk.len();
            let processed = match self.get_crypto().process_write(chunk.clone()) {
                Ok(p) => p,
                Err(e) => {
                    for k in &allocated_keys {
                        let _ = self.backend_router.free_block(k).await;
                    }
                    return Err(e);
                }
            };
            let processed_len = processed.len();

            if let Err(e) = nvme_writer.write_block(offset, &processed).await {
                for k in &allocated_keys {
                    let _ = self.backend_router.free_block(k).await;
                }
                return Err(e);
            }

            // Cache plaintext block for subsequent reads (key = block key, not file path).
            self.cache.read_lru.put(&stored_block_key, chunk);

            block_mappings.push((block_count.to_string(), stored_block_key.clone()));
            sizes_to_register.push((stored_block_key, chunk_len, processed_len));
            offset_cursor = end;
            block_count += 1;
        }

        Ok((block_mappings, sizes_to_register, block_count))
    }

    /// Register a completed stripe layout in Garnet only after durable writes.
    /// Block-map, refcounts, sizes, and file meta type flip are applied in one
    /// MULTI/EXEC transaction so readers never observe a half-committed layout (P0-6).
    async fn commit_striped_layout_meta(
        &self,
        file_path: &str,
        meta_key: &str,
        file_type: Option<&str>,
        block_map_id: &str,
        file_uuid: &str,
        block_count: u32,
        size: usize,
        fencing_token: u64,
        block_mappings: &[(String, String)],
        sizes_to_register: &[(String, usize, usize)],
        con: &mut crate::dlm::MetaConnection,
    ) -> Result<()> {
        let block_map_key = crate::keys::block_map(&block_map_id);
        let refcounts_key_str = crate::fs_key!("block_refcounts");
        let refcounts_key = &refcounts_key_str;
        let block_sizes_key = crate::fs_key!("block_sizes");

        // Reads must happen outside MULTI (WATCHed differently); capture staged id first.
        let old_file_id: Option<String> = if file_type == Some("staged") {
            con.hget(meta_key, "file_id").await?
        } else {
            None
        };

        let mut pipe = atomic_meta_pipe();
        for (idx_str, key) in block_mappings {
            pipe.hset(&block_map_key, idx_str, key);
            pipe.hset(refcounts_key, key, 1);
        }
        for (key, logical, physical) in sizes_to_register {
            pipe.hset(&block_sizes_key, key, format!("{}:{}", logical, physical));
        }
        pipe.hset(meta_key, "size", size)
            .hset(meta_key, "type", "striped")
            .hset(meta_key, "block_prefix", format!("blocks/{}", file_uuid))
            .hset(meta_key, "block_map_id", block_map_id)
            .hset(meta_key, "num_blocks", block_count)
            .hset(meta_key, "fencing_token", fencing_token);

        if file_type == Some("inline") {
            let inline_key = crate::keys::inline_data(file_path);
            pipe.del(&inline_key);
        }
        if file_type == Some("staged") {
            pipe.hdel(meta_key, "file_id");
        }

        let _: () = tokio::time::timeout(std::time::Duration::from_secs(2), pipe.query_async(con))
            .await
            .map_err(|_| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Redis atomic metadata commit timed out",
                ))
            })??;

        if let Some(old_id) = old_file_id {
            self.cache.nvme.remove_staged(&old_id);
            let _ = self.decrement_staged_block_refcount(&old_id, con).await;
            let mapping_key = crate::keys::mapping(&old_id);
            let _: () = con.del(&mapping_key).await.unwrap_or_else(|e| {
                log::debug!("non-fatal cleanup op failed: {:?}", e);
            });
        }

        self.cache.write_lru.remove(file_path);
        self.cache.read_lru.remove(file_path);
        self.metadata_cache.invalidate(file_path);
        Ok(())
    }

    /// Striped RMW. P1-10: meta connections are phased — open for block-map
    /// reads, **dropped** before durable block I/O, re-acquired only for the
    /// atomic map/size commit and refcount cleanup (frees run after redis work).
    async fn write_striped(
        &self,
        file_path: &str,
        meta_key: &str,
        offset: u64,
        data: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<()> {
        let ino = parse_inode_from_path(file_path);
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

        // --- Meta prep (short-lived connection) ---
        let (block_map_id, num_blocks, existing_size, old_block_keys) = {
            let mut con = self.dlm.get_connection_for_inode(ino).await?;
            let (block_map_id_opt, num_blocks_opt, size_opt): (
                Option<String>,
                Option<u32>,
                Option<u64>,
            ) = redis::cmd("HMGET")
                .arg(meta_key)
                .arg("block_map_id")
                .arg("num_blocks")
                .arg("size")
                .query_async(&mut con)
                .await?;

            let block_map_id = match block_map_id_opt {
                Some(id) => id,
                None => {
                    let block_prefix_opt: Option<String> =
                        con.hget(meta_key, "block_prefix").await?;
                    let block_prefix = block_prefix_opt.ok_or_else(|| {
                        SqueezefsError::InvalidOperation(
                            "Missing block_map_id and block_prefix for striped file".to_string(),
                        )
                    })?;
                    let num_blocks = num_blocks_opt.unwrap_or(0);

                    let new_id = Uuid::new_v4().to_string();
                    let block_map_key = crate::keys::block_map(&new_id);
                    let refcounts_key_str = crate::fs_key!("block_refcounts");
                    let refcounts_key = &refcounts_key_str;
                    let mut pipe = redis::pipe();
                    for i in 0..num_blocks {
                        let old_key = format!("{}/part_{}", block_prefix, i);
                        pipe.hset(&block_map_key, i.to_string(), &old_key);
                        pipe.hset(refcounts_key, &old_key, 1);
                    }
                    pipe.hset(meta_key, "block_map_id", &new_id);
                    let _: () = pipe.query_async(&mut con).await?;
                    new_id
                }
            };

            let num_blocks = num_blocks_opt.unwrap_or(0);
            let existing_size = size_opt.unwrap_or(0);
            let block_map_key = crate::keys::block_map(&block_map_id);

            let mut pipe = redis::pipe();
            for b in start_block..=end_block {
                pipe.hget(&block_map_key, b.to_string());
            }
            let old_block_keys: Vec<Option<String>> = pipe.query_async(&mut con).await?;
            (block_map_id, num_blocks, existing_size, old_block_keys)
        }; // meta con dropped before block I/O

        let block_map_key = crate::keys::block_map(&block_map_id);
        let refcounts_key_str = crate::fs_key!("block_refcounts");
        let refcounts_key = &refcounts_key_str;

        // Pre-resolve cache hits on the main thread and spawn tasks to modify affected blocks concurrently
        use futures::stream::{FuturesUnordered, StreamExt};
        let tasks = FuturesUnordered::new();
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(16)); // Max 16 concurrent block writes

        let mut pre_resolved_blocks = Vec::with_capacity(old_block_keys.len());
        for (idx, b) in (start_block..=end_block).enumerate() {
            let block_start_file_offset = b as u64 * block_size;
            let block_end_file_offset = block_start_file_offset + block_size;
            let overlap_start = std::cmp::max(block_start_file_offset, offset);
            let overlap_end = std::cmp::min(block_end_file_offset, end_pos);
            let needs_existing = {
                let existing_block_end = std::cmp::min(existing_size, block_end_file_offset);
                existing_block_end > block_start_file_offset
                    && (overlap_start > block_start_file_offset || overlap_end < existing_block_end)
            };

            let mut resolved = None;
            if needs_existing {
                if let Some(ref bk) = old_block_keys[idx] {
                    if let Some(cached_block) = self.cache.read_lru.get(bk) {
                        resolved = Some(crate::cache::pool::ReadBlockValue::Bytes(cached_block));
                    } else if let Some(cached_block) = self.cache.nvme.read_cached_block(bk) {
                        let bytes = bytes::Bytes::from(cached_block);
                        self.cache.read_lru.put(bk, bytes.clone());
                        resolved = Some(crate::cache::pool::ReadBlockValue::Bytes(bytes));
                    }
                }
            }
            pre_resolved_blocks.push(resolved);
        }

        let mut old_keys_iter = old_block_keys.into_iter();
        let mut pre_resolved_iter = pre_resolved_blocks.into_iter();
        for b in start_block..=end_block {
            let old_block_key = old_keys_iter.next().unwrap();
            let pre_resolved = pre_resolved_iter.next().unwrap();

            let block_start_file_offset = b as u64 * block_size;
            let block_end_file_offset = block_start_file_offset + block_size;

            let overlap_start = std::cmp::max(block_start_file_offset, offset);
            let overlap_end = std::cmp::min(block_end_file_offset, end_pos);

            let rel_start = (overlap_start - block_start_file_offset) as usize;
            let rel_end = (overlap_end - block_start_file_offset) as usize;

            // SAFETY: (overlap_start - offset) and (overlap_end - offset) are in bounds by construction
            let data_slice = unsafe {
                let sub = data.get_unchecked(
                    (overlap_start - offset) as usize..(overlap_end - offset) as usize,
                );
                data.slice_ref(sub)
            };

            let router_clone = self.clone();
            let crypto = self.get_crypto().clone();
            let read_lru = self.cache.read_lru.clone();

            let needs_existing = {
                let existing_block_end = std::cmp::min(existing_size, block_end_file_offset);
                existing_block_end > block_start_file_offset
                    && (overlap_start > block_start_file_offset || overlap_end < existing_block_end)
            };

            let sem_clone = sem.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = sem_clone.acquire().await.map_err(|e| {
                    SqueezefsError::Io(std::io::Error::other(format!(
                        "Semaphore acquire error: {:?}",
                        e
                    )))
                })?;

                let mut block_data = if needs_existing {
                    if let Some(resolved) = pre_resolved {
                        match resolved {
                            crate::cache::pool::ReadBlockValue::Pooled(p) => p,
                            crate::cache::pool::ReadBlockValue::Bytes(b) => {
                                let mut pooled = BUFFER_POOL.alloc();
                                pooled.resize(b.len(), 0);
                                pooled.copy_from_slice(&b);
                                pooled
                            }
                        }
                    } else if let Some(ref bk) = old_block_key {
                        match router_clone.get_cached_or_fetch_block(bk).await? {
                            crate::cache::pool::ReadBlockValue::Pooled(p) => p,
                            crate::cache::pool::ReadBlockValue::Bytes(b) => {
                                let mut pooled = BUFFER_POOL.alloc();
                                pooled.resize(b.len(), 0);
                                pooled.copy_from_slice(&b);
                                pooled
                            }
                        }
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

                // SAFETY: We ensured block_data.len() >= rel_end on line 1419
                unsafe {
                    block_data
                        .get_unchecked_mut(rel_start..rel_end)
                        .copy_from_slice(&data_slice);
                }

                let (be_id, block_allocator, nvme_writer) =
                    router_clone.backend_router.get_active_backend()?;
                let offset = block_allocator.allocate_block().await?;
                let stored_new_block_key = if be_id == "backend_0" {
                    offset.to_string()
                } else {
                    format!("{}://{}", be_id, offset)
                };

                let block_bytes = bytes::Bytes::from(block_data.into_inner());

                // Cache newly written block in RAM - dehydrated to NVMe on eviction
                read_lru.put(&stored_new_block_key, block_bytes.clone());

                let logical_size = block_bytes.len();
                let processed_block = crypto.process_write(block_bytes)?;
                let physical_size = processed_block.len();
                nvme_writer.write_block(offset, &processed_block).await?;
                debug!(
                    "Writeback: Successfully wrote block {} to backing device",
                    stored_new_block_key
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
        let mut first_err: Option<SqueezefsError> = None;
        let mut tasks_stream = tasks;
        while let Some(task_res) = tasks_stream.next().await {
            match task_res.map_err(|e| {
                SqueezefsError::Io(std::io::Error::other(format!(
                    "Block write task panicked: {:?}",
                    e
                )))
            }) {
                Ok(Ok(res)) => results.push(res),
                Ok(Err(e)) | Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if let Some(e) = first_err {
            // Do not commit partial block-map updates; free blocks that did land.
            for (_b, _old, new_key, _logical, _physical) in &results {
                let _ = self.backend_router.free_block(new_key).await;
            }
            return Err(e);
        }

        // 4. Atomically update block map + file size/fence (P0-6). Fresh meta con
        // only for redis work; backend free runs after the connection is released.
        let mut old_keys_to_clean = Vec::new();
        let mut keys_to_free = Vec::new();
        {
            let mut con = self.dlm.get_connection_for_inode(ino).await?;
            let mut pipe_update = atomic_meta_pipe();
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

            let new_num_blocks = std::cmp::max(num_blocks, end_block + 1);
            let new_size = std::cmp::max(existing_size, end_pos);
            pipe_update
                .hset(meta_key, "size", new_size)
                .hset(meta_key, "num_blocks", new_num_blocks)
                .hset(meta_key, "fencing_token", fencing_token);

            let _: () = pipe_update.query_async(&mut con).await?;

            // Clean up old block keys (best-effort; primary mapping already committed)
            for bk in old_keys_to_clean {
                self.cache.read_lru.remove(&bk);
                let old_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
                if let Some(mut r) = old_ref {
                    r -= 1;
                    if r <= 0 {
                        let _: () = atomic_meta_pipe()
                            .hdel(refcounts_key, &bk)
                            .hdel(crate::fs_key!("block_sizes"), &bk)
                            .query_async(&mut con)
                            .await?;
                        keys_to_free.push(bk);
                    } else {
                        let _: () = con.hset(refcounts_key, &bk, r).await?;
                    }
                } else {
                    let _: () = con
                        .hdel(crate::fs_key!("block_sizes"), &bk)
                        .await
                        .unwrap_or_else(|e| {
                            log::debug!("non-fatal cleanup op failed: {:?}", e);
                        });
                    keys_to_free.push(bk);
                }
            }
        } // meta con dropped before backend free

        for bk in keys_to_free {
            let _ = self.backend_router.free_block(&bk).await;
        }

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
        let meta_key = crate::keys::metadata_for_path(file_path);

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
                let inline_key = crate::keys::inline_data(file_path);
                let bytes: Vec<u8> = con.get(&inline_key).await?;
                self.get_crypto().process_read(&bytes)?.into_owned()
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
                    // NVMe staging file was flushed/merged. Read the packed block from NVMe-oF backend
                    debug!(
                        "Routing: Staged file '{}' (ID: {}) already merged. Reading packed block.",
                        file_path, file_id
                    );
                    let mapping_key = crate::keys::mapping(&file_id);
                    let (block_key, offset, size): (Option<String>, Option<u64>, Option<u64>) =
                        redis::cmd("HMGET")
                            .arg(&mapping_key)
                            .arg("block")
                            .arg("offset")
                            .arg("size")
                            .query_async(&mut con)
                            .await?;

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
                        self.get_crypto().process_read(&raw)?.into_owned()
                    } else {
                        return Err(SqueezefsError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Staged file ID {} mapping not found in Garnet", file_id),
                        )));
                    }
                }
            }
            "striped" => {
                // Large File: fetch blocks from NVMe-oF backend in parallel
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
                    let block_map_key = crate::keys::block_map(&block_map_id);
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

                // P1-5: bounded concurrency (order preserved via index sort).
                use futures::stream::{self, StreamExt};
                let mut indexed: Vec<(usize, Result<crate::cache::pool::ReadBlockValue>)> =
                    stream::iter(
                        block_keys
                            .into_iter()
                            .enumerate()
                            .map(|(i, block_key_opt)| {
                                let router = self.clone();
                                async move {
                                    let res = if let Some(block_key) = block_key_opt {
                                        router.get_cached_or_fetch_block(&block_key).await
                                    } else {
                                        let mut buf = BUFFER_POOL.alloc();
                                        buf.resize(block_size as usize, 0);
                                        Ok(crate::cache::pool::ReadBlockValue::Pooled(buf))
                                    };
                                    (i, res)
                                }
                            }),
                    )
                    .buffer_unordered(crate::bg_admit::STRIPED_READ_CONCURRENCY)
                    .collect()
                    .await;
                indexed.sort_by_key(|(i, _)| *i);

                let mut file_data = Vec::new();
                for (_, block_data) in indexed {
                    file_data.extend_from_slice(&block_data?);
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
                let meta_key = crate::keys::metadata_for_path(file_path);
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
                let inline_key = crate::keys::inline_data(file_path);
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
                    let mapping_key = crate::keys::mapping(&file_id);
                    let mut con = self
                        .dlm
                        .get_connection_for_inode(parse_inode_from_path(file_path))
                        .await?;
                    let (block_key, off_opt, sz_opt): (Option<String>, Option<u64>, Option<u64>) =
                        redis::cmd("HMGET")
                            .arg(&mapping_key)
                            .arg("block")
                            .arg("offset")
                            .arg("size")
                            .query_async(&mut con)
                            .await?;

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

                // P1-5: admit each block task under STRIPED_IO_SEM (bounded concurrency).
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
                    let permit = crate::bg_admit::STRIPED_IO_SEM
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| {
                            SqueezefsError::InvalidOperation(
                                "striped read admission closed".to_string(),
                            )
                        })?;
                    futures.push(tokio::spawn(async move {
                        let _permit = permit;
                        let cache_key =
                            crate::keys::active_block_for_path(&file_path_clone, b_idx).to_string();
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
        let mut cached_opt = self.cache.read_lru.get(file_path);
        if cached_opt.is_none() {
            cached_opt = self.cache.write_lru.get(file_path);
        }
        if let Some(cached_data) = cached_opt {
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            let start = std::cmp::min(offset as usize, cached_data.len());
            let end = std::cmp::min((offset + size as u64) as usize, cached_data.len());
            // SAFETY: start and end are clamped to cached_data.len()
            let data = unsafe {
                let sub = cached_data.get_unchecked(start..end);
                cached_data.slice_ref(sub)
            };
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
                let inline_key = crate::keys::inline_data(file_path);
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
                    let data = bytes::Bytes::copy_from_slice(&sliced_guard);
                    Ok((data, Some(std::sync::Arc::new(sliced_guard))))
                } else {
                    let mapping_key = crate::keys::mapping(&file_id);
                    let mut con = self
                        .dlm
                        .get_connection_for_inode(parse_inode_from_path(file_path))
                        .await?;
                    let (block_key, off_opt, sz_opt): (Option<String>, Option<u64>, Option<u64>) =
                        redis::cmd("HMGET")
                            .arg(&mapping_key)
                            .arg("block")
                            .arg("offset")
                            .arg("size")
                            .query_async(&mut con)
                            .await?;

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
                    let cache_key =
                        crate::keys::active_block_for_path(file_path, b_idx).to_string();

                    // Check active block staging first
                    if let Some(guard) = self.cache.nvme.read_staged_zero_copy(&cache_key) {
                        let start = std::cmp::min(slice_start as usize, guard.len);
                        let end =
                            std::cmp::min((slice_start + slice_len as u64) as usize, guard.len);
                        let mut sliced_guard = guard;
                        sliced_guard.offset += start;
                        sliced_guard.len = end - start;
                        let data = bytes::Bytes::copy_from_slice(&sliced_guard);
                        return Ok((data, Some(std::sync::Arc::new(sliced_guard))));
                    }

                    // Check NVMe read block cache next
                    let block_keys = self
                        .load_striped_block_keys(file_path, &meta, start_block, end_block)
                        .await?;
                    if let Some((_, b_key_opt)) = block_keys.first() {
                        if let Some(ref b_key) = b_key_opt {
                            if let Some(guard) =
                                self.cache.nvme.get_cached_read_block_range_zero_copy(
                                    b_key,
                                    slice_start,
                                    slice_len,
                                )
                            {
                                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                                let data = bytes::Bytes::copy_from_slice(&guard);
                                return Ok((data, Some(std::sync::Arc::new(guard))));
                            } else {
                                // Single block cache miss: download directly in-line (zero-copy, no spawn)
                                let downloaded = self.get_cached_or_fetch_block(b_key).await?;
                                let start = std::cmp::min(slice_start as usize, downloaded.len());
                                let end = std::cmp::min(
                                    (slice_start + slice_len as u64) as usize,
                                    downloaded.len(),
                                );
                                if self.should_prefetch_after_striped_read(
                                    file_path,
                                    start_block,
                                    end_block,
                                ) {
                                    self.schedule_striped_prefetch(
                                        file_path.to_string(),
                                        meta.clone(),
                                        end_block.saturating_add(1),
                                        block_size,
                                    );
                                }
                                let slice: &[u8] = &downloaded[start..end];
                                let data = bytes::Bytes::copy_from_slice(slice);
                                return Ok((data, Some(std::sync::Arc::new(downloaded))));
                            }
                        } else {
                            // Hole support: return zero-filled slice
                            let mut hole_pooled = BUFFER_POOL.alloc();
                            hole_pooled.resize(slice_len as usize, 0);
                            let data = bytes::Bytes::copy_from_slice(&hole_pooled);
                            return Ok((data, Some(std::sync::Arc::new(hole_pooled))));
                        }
                    }
                }

                // 2. Multi-block or cache miss: load and assemble using pooled buffer
                let block_keys = self
                    .load_striped_block_keys(file_path, &meta, start_block, end_block)
                    .await?;

                let mut final_buf = BUFFER_POOL.alloc();
                let final_len = (end_offset - offset) as usize;
                final_buf.resize(final_len, 0);
                let raw_ptr = final_buf.as_mut_ptr() as usize;

                // Spawn concurrent tasks to download block data in parallel
                let mut futures = Vec::new();
                for (b_idx, b_key_opt) in block_keys {
                    let router = self.clone();
                    let b_start_offset = b_idx as u64 * block_size;
                    let b_end_offset = b_start_offset + block_size;
                    let slice_start = std::cmp::max(offset, b_start_offset);
                    let slice_end = std::cmp::min(end_offset, b_end_offset);
                    let dest_start = (slice_start - offset) as usize;
                    let copy_len = (slice_end - slice_start) as usize;

                    let rel_start = (slice_start - b_start_offset) as usize;
                    let file_path_clone = file_path.to_string();

                    let permit = crate::bg_admit::STRIPED_IO_SEM
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| {
                            SqueezefsError::InvalidOperation(
                                "striped read admission closed".to_string(),
                            )
                        })?;
                    futures.push(tokio::spawn(async move {
                        let _permit = permit;
                        let cache_key =
                            crate::keys::active_block_for_path(&file_path_clone, b_idx).to_string();
                        if let Some(active_data) = router.cache.nvme.read_staged(&cache_key) {
                            let start = std::cmp::min(rel_start, active_data.len());
                            let end = std::cmp::min(rel_start + copy_len, active_data.len());
                            let actual_copy = end - start;
                            if actual_copy > 0 {
                                unsafe {
                                    let dest = (raw_ptr + dest_start) as *mut u8;
                                    std::ptr::copy_nonoverlapping(
                                        active_data[start..end].as_ptr(),
                                        dest,
                                        actual_copy,
                                    );
                                }
                            }
                        } else if let Some(ref b_key) = b_key_opt {
                            if let Some(cached_block) = router.cache.read_lru.get(b_key) {
                                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                                let start = std::cmp::min(rel_start, cached_block.len());
                                let end = std::cmp::min(rel_start + copy_len, cached_block.len());
                                let actual_copy = end - start;
                                if actual_copy > 0 {
                                    unsafe {
                                        let dest = (raw_ptr + dest_start) as *mut u8;
                                        std::ptr::copy_nonoverlapping(
                                            cached_block[start..end].as_ptr(),
                                            dest,
                                            actual_copy,
                                        );
                                    }
                                }
                            } else {
                                let downloaded = router.get_cached_or_fetch_block(b_key).await?;
                                let start = std::cmp::min(rel_start, downloaded.len());
                                let end = std::cmp::min(rel_start + copy_len, downloaded.len());
                                let actual_copy = end - start;
                                if actual_copy > 0 {
                                    unsafe {
                                        let dest = (raw_ptr + dest_start) as *mut u8;
                                        std::ptr::copy_nonoverlapping(
                                            downloaded[start..end].as_ptr(),
                                            dest,
                                            actual_copy,
                                        );
                                    }
                                }
                            }
                        }
                        Ok::<(), SqueezefsError>(())
                    }));
                }

                let results = futures::future::try_join_all(futures).await.map_err(|e| {
                    SqueezefsError::Io(std::io::Error::other(format!(
                        "Parallel block download task panicked: {:?}",
                        e
                    )))
                })?;

                for res in results {
                    res?;
                }

                if self.should_prefetch_after_striped_read(file_path, start_block, end_block) {
                    self.schedule_striped_prefetch(
                        file_path.to_string(),
                        meta.clone(),
                        end_block.saturating_add(1),
                        block_size,
                    );
                }

                let data = bytes::Bytes::copy_from_slice(&final_buf[..final_len]);
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
        let meta_key = crate::keys::metadata_for_path(file_path);
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
        let (_src_lock, dest_lock) = if src < dest {
            let l1 = self
                .dlm
                .acquire_lock_with_retry(src, None, std::time::Duration::from_secs(5), 5)
                .await?;
            let l2 = self
                .dlm
                .acquire_lock_with_retry(dest, None, std::time::Duration::from_secs(5), 5)
                .await?;
            (l1, l2)
        } else {
            let l1 = self
                .dlm
                .acquire_lock_with_retry(dest, None, std::time::Duration::from_secs(5), 5)
                .await?;
            let l2 = self
                .dlm
                .acquire_lock_with_retry(src, None, std::time::Duration::from_secs(5), 5)
                .await?;
            (l2, l1)
        };

        let mut src_con = if let Some(src_ino) =
            crate::dlm::parse_inode_from_key(&crate::keys::metadata_for_path(src))
        {
            self.dlm.get_connection_for_inode(src_ino).await?
        } else {
            self.dlm.get_connection().await?
        };
        let mut dest_con = if let Some(dest_ino) =
            crate::dlm::parse_inode_from_key(&crate::keys::metadata_for_path(dest))
        {
            self.dlm.get_connection_for_inode(dest_ino).await?
        } else {
            self.dlm.get_connection().await?
        };
        let mut con = self.dlm.get_connection().await?;

        let src_meta_key = crate::keys::metadata_for_path(src);
        let dest_meta_key = crate::keys::metadata_for_path(dest);

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
            let inline_src_key = crate::keys::inline_data(src);
            let inline_dest_key = crate::keys::inline_data(dest);

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

            let mapping_src_key = crate::keys::mapping(&src_file_id);
            let mapping_dest_key = crate::keys::mapping(&new_file_id);
            let (block, offset, sz): (Option<String>, Option<u64>, Option<u64>) =
                redis::cmd("HMGET")
                    .arg(&mapping_src_key)
                    .arg("block")
                    .arg("offset")
                    .arg("size")
                    .query_async(&mut src_con)
                    .await?;

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
                    let block_map_key = crate::keys::block_map(&new_id);
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
            let src_block_map_key = crate::keys::block_map(&src_block_map_id);
            let dest_block_map_key = crate::keys::block_map(&dest_block_map_id);
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
        let meta_key = crate::keys::metadata_for_path(file_path);
        let file_type: Option<String> = con.hget(&meta_key, "type").await?;
        if let Some(t) = file_type {
            if t == "striped" {
                let block_map_id_opt: Option<String> = con.hget(&meta_key, "block_map_id").await?;
                if let Some(block_map_id) = block_map_id_opt {
                    let block_map_key = crate::keys::block_map(&block_map_id);
                    let refcounts_key_str = crate::fs_key!("block_refcounts");
                    let refcounts_key = &refcounts_key_str;
                    let blocks_to_free = self
                        .delete_file_striped_fallback(
                            &meta_key,
                            &block_map_id,
                            &block_map_key,
                            refcounts_key,
                            con,
                        )
                        .await?;

                    if !blocks_to_free.is_empty() {
                        let blocks_str: Vec<&str> =
                            blocks_to_free.iter().map(|s| s.as_str()).collect();
                        self.backend_router.free_blocks(&blocks_str).await?;
                    }
                    let _: () = con.del(&block_map_key).await.unwrap_or_default();
                }
            } else if t == "staged" {
                let file_id_opt: Option<String> = con.hget(&meta_key, "file_id").await?;
                if let Some(fid) = file_id_opt {
                    let mapping_key = crate::keys::mapping(&fid);
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
                                let _ = self.backend_router.free_block(&bk).await;
                            } else {
                                let _: () = con.hset(refcounts_key, &bk, r).await?;
                            }
                        } else {
                            let _: () = con
                                .hdel(crate::fs_key!("block_sizes"), &bk)
                                .await
                                .unwrap_or_else(|e| {
                                    log::debug!("non-fatal cleanup op failed: {:?}", e);
                                });
                            let _ = self.backend_router.free_block(&bk).await;
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
        let _: () = pipe.query_async(con).await.unwrap_or_else(|e| {
            log::debug!("non-fatal cleanup op failed: {:?}", e);
        });

        // Remove any local active write blocks for this inode from the staging segment cache
        let active_block_prefix = crate::keys::active_block_path_prefix(file_path);
        let keys_to_remove: Vec<String> = self
            .cache
            .nvme
            .list_staged_files()
            .into_iter()
            .filter(|k| k.starts_with(active_block_prefix.as_str()))
            .collect();
        for key in keys_to_remove {
            self.cache.nvme.remove_active_block(&key);
        }

        self.cache.write_lru.remove(file_path);
        self.cache.read_lru.remove(file_path);
        self.metadata_cache.invalidate(file_path);
        Ok(())
    }

    async fn delete_file_striped_fallback(
        &self,
        _meta_key: &str,
        block_map_id: &str,
        block_map_key: &str,
        refcounts_key: &str,
        con: &mut crate::dlm::MetaConnection,
    ) -> Result<Vec<String>> {
        let block_mappings: std::collections::HashMap<String, String> =
            con.hgetall(block_map_key).await?;

        let mut blocks_to_free = Vec::new();
        if !block_mappings.is_empty() {
            for idx_str in block_mappings.keys() {
                if let Ok(idx) = idx_str.parse::<u32>() {
                    self.block_map_cache
                        .invalidate(&(block_map_id.to_string(), idx));
                }
            }

            // 1. Pipeline query all refcounts
            let mut get_pipe = redis::pipe();
            for bk in block_mappings.values() {
                get_pipe.hget(refcounts_key, bk);
            }
            let refcounts: Vec<Option<i32>> = get_pipe.query_async(con).await?;

            // 2. Pipeline updates/deletes
            let mut update_pipe = redis::pipe();
            let mut has_updates = false;
            for ((_, bk), ref_opt) in block_mappings.iter().zip(refcounts) {
                if let Some(mut r) = ref_opt {
                    r -= 1;
                    if r <= 0 {
                        update_pipe
                            .hdel(refcounts_key, bk)
                            .hdel(crate::fs_key!("block_sizes"), bk);
                        blocks_to_free.push(bk.clone());
                        has_updates = true;
                    } else {
                        update_pipe.hset(refcounts_key, bk, r);
                        has_updates = true;
                    }
                } else {
                    update_pipe.hdel(crate::fs_key!("block_sizes"), bk);
                    blocks_to_free.push(bk.clone());
                    has_updates = true;
                }
            }
            if has_updates {
                let _: () = update_pipe.query_async(con).await?;
            }
        }
        Ok(blocks_to_free)
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
            crate::keys::inode_path(src_ino).as_str(),
            crate::keys::inode_path(dest_ino).as_str(),
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
