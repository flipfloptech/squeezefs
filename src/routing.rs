use crate::cache::{TieredCache, BUFFER_POOL};
use crate::dlm::DlmClient;

use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::meta_backend::Metadata;
use log::debug;
use std::sync::atomic::Ordering;

use std::time::Duration;
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

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct LayoutMetadata {
    pub file_type: String,
    pub size: u64,
    pub block_map_id: Option<String>,
    pub block_prefix: Option<String>,
    pub file_id: Option<String>,
    pub data_key: Option<Vec<u8>>,
    pub block_map: Option<std::collections::HashMap<u32, String>>,
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
    pub block_map: Option<std::collections::HashMap<u32, String>>,
}

impl Default for CachedMetadata {
    fn default() -> Self {
        Self {
            file_type: "inline".to_string(),
            size: 0,
            block_map_id: None,
            block_prefix: None,
            file_id: None,
            cached_at: std::time::Instant::now(),
            data_key: None,
            block_map: None,
        }
    }
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
    pub unhealthy_backends: std::sync::Arc<dashmap::DashMap<String, bool, ahash::RandomState>>,
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
    crate::error::SqueezefsError::InvalidOperation(format!(
        "Storage backend '{}' not found/offline",
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
            unhealthy_backends: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            active_write_backend: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                "backend_0".to_string(),
            )),
            block_size,
        }
    }

    pub fn is_backend_healthy(&self, be_id: &str) -> bool {
        if self.unhealthy_backends.contains_key(be_id) {
            false
        } else if be_id == "backend_0" {
            std::path::Path::new(&self.default_device.device_path).exists()
        } else if let Some(be) = self.backends.get(be_id) {
            std::path::Path::new(&be.device.device_path).exists()
        } else {
            false
        }
    }

    pub fn get_active_backend(
        &self,
    ) -> Result<(
        String,
        std::sync::Arc<crate::block_allocator::BlockAllocator>,
        std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    )> {
        let mut healthy_backends = Vec::new();

        if self.is_backend_healthy("backend_0") {
            healthy_backends.push((
                "backend_0".to_string(),
                self.default_allocator.clone(),
                self.default_device.clone(),
            ));
        }

        for entry in self.backends.iter() {
            let be_id = entry.key();
            if self.is_backend_healthy(be_id) {
                healthy_backends.push((
                    be_id.clone(),
                    entry.value().block_allocator.clone(),
                    entry.value().device.clone(),
                ));
            }
        }

        if healthy_backends.is_empty() {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "No healthy storage backends available for write",
            )));
        }

        let mut selected = &healthy_backends[0];
        let mut min_used = selected.1.get_used_blocks();

        for backend in &healthy_backends[1..] {
            let used = backend.1.get_used_blocks();
            if used < min_used {
                min_used = used;
                selected = backend;
            }
        }

        Ok((selected.0.clone(), selected.1.clone(), selected.2.clone()))
    }

    pub fn get_backend(
        &self,
        be_id: &str,
    ) -> Result<(
        std::sync::Arc<crate::block_allocator::BlockAllocator>,
        std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    )> {
        if !self.is_backend_healthy(be_id) {
            return Err(err_backend_not_found(be_id));
        }
        if be_id == "backend_0" {
            Ok((self.default_allocator.clone(), self.default_device.clone()))
        } else if let Some(be) = self.backends.get(be_id) {
            Ok((be.block_allocator.clone(), be.device.clone()))
        } else {
            Err(err_backend_not_found(be_id))
        }
    }

    pub fn parse_block_key(&self, block_key: &str) -> Result<(String, u64)> {
        let parts: Vec<&str> = block_key.split("://").collect();
        let (be_id, offset_str) = if parts.len() > 1 {
            (parts[0], parts[1])
        } else {
            ("backend_0", block_key)
        };
        let offset = offset_str
            .parse::<u64>()
            .map_err(|_| err_invalid_offset())?;
        Ok((be_id.to_string(), offset))
    }

    pub fn parse_block_offset(&self, block_key: &str) -> Result<u64> {
        let (_, offset) = self.parse_block_key(block_key)?;
        Ok(offset)
    }

    pub async fn read_block(&self, block_key: &str, size: usize) -> Result<bytes::Bytes> {
        self.read_block_with_dest(block_key, size, None).await
    }

    pub async fn read_block_with_dest(
        &self,
        block_key: &str,
        size: usize,
        dest_addr: Option<u64>,
    ) -> Result<bytes::Bytes> {
        let (be_id, offset) = self.parse_block_key(block_key)?;

        if be_id == "backend_0" {
            self.default_device
                .read_block_with_dest(offset, size, dest_addr)
                .await
        } else if let Some(be) = self.backends.get(&be_id) {
            be.device
                .read_block_with_dest(offset, size, dest_addr)
                .await
        } else {
            Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "Storage backend '{}' not found",
                be_id
            )))
        }
    }

    pub fn increment_refcount(&self, block_key: &str) {
        if let Ok((be_id, offset)) = self.parse_block_key(block_key) {
            if be_id == "backend_0" {
                self.default_allocator.increment_refcount(offset);
            } else if let Some(be) = self.backends.get(&be_id) {
                be.block_allocator.increment_refcount(offset);
            }
        }
    }

    pub async fn free_block(&self, block_key: &str) -> Result<()> {
        let (be_id, offset) = self.parse_block_key(block_key)?;

        if be_id == "backend_0" {
            let _ = self.default_allocator.free_block(offset).await;
        } else if let Some(be) = self.backends.get(&be_id) {
            let _ = be.block_allocator.free_block(offset).await;
        }
        Ok(())
    }

    pub async fn free_blocks(&self, block_keys: &[&str]) -> Result<()> {
        if block_keys.is_empty() {
            return Ok(());
        }
        let mut backend_groups: std::collections::HashMap<String, Vec<u64>> =
            std::collections::HashMap::new();
        for &block_key in block_keys {
            if let Ok((be_id, offset)) = self.parse_block_key(block_key) {
                backend_groups.entry(be_id).or_default().push(offset);
            }
        }

        for (be_id, offsets) in backend_groups {
            if be_id == "backend_0" {
                let _ = self.default_allocator.free_blocks(&offsets).await;
            } else if let Some(be) = self.backends.get(&be_id) {
                let _ = be.block_allocator.free_blocks(&offsets).await;
            }
        }
        Ok(())
    }

    pub fn start_health_check_worker(
        self: &std::sync::Arc<Self>,
        _redis_url: String,
        _fs_name: String,
    ) {
        let router = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;

                // 1. Health check default backend (backend_0)
                let default_healthy = perform_device_health_check(&router.default_device).await;
                if !default_healthy {
                    if !router.unhealthy_backends.contains_key("backend_0") {
                        log::error!("Backend health check: backend_0 (default) is UNHEALTHY!");
                        router
                            .unhealthy_backends
                            .insert("backend_0".to_string(), true);
                    }
                } else if router.unhealthy_backends.contains_key("backend_0") {
                    log::info!("Backend health check: backend_0 has recovered.");
                    router.unhealthy_backends.remove("backend_0");
                }

                // 2. Health check all registered secondary backends
                let mut failed_backends = Vec::new();
                let mut recovered_backends = Vec::new();

                for entry in router.backends.iter() {
                    let be_id = entry.key();
                    let dev = &entry.value().device;
                    let healthy = perform_device_health_check(dev).await;
                    if !healthy {
                        if !router.unhealthy_backends.contains_key(be_id) {
                            log::error!("Backend health check: backend '{}' is UNHEALTHY!", be_id);
                            failed_backends.push(be_id.clone());
                        }
                    } else if router.unhealthy_backends.contains_key(be_id) {
                        log::info!("Backend health check: backend '{}' has recovered.", be_id);
                        recovered_backends.push(be_id.clone());
                    }
                }

                for be_id in failed_backends {
                    router.unhealthy_backends.insert(be_id, true);
                }
                for be_id in recovered_backends {
                    router.unhealthy_backends.remove(&be_id);
                }

                // 3. Trigger failover if currently active write backend is unhealthy
                let active_be = (*router.active_write_backend.load_full()).clone();
                if !router.is_backend_healthy(&active_be) {
                    log::warn!(
                        "Active write backend '{}' is unhealthy! Initiating failover...",
                        active_be
                    );
                    let mut fallback_be = None;
                    if router.is_backend_healthy("backend_0") {
                        fallback_be = Some("backend_0".to_string());
                    } else {
                        for entry in router.backends.iter() {
                            let be_id = entry.key();
                            if router.is_backend_healthy(be_id) {
                                fallback_be = Some(be_id.clone());
                                break;
                            }
                        }
                    }

                    if let Some(healthy_be) = fallback_be {
                        log::info!(
                            "Failover: Switching active write backend from '{}' to '{}'",
                            active_be,
                            healthy_be
                        );
                        router
                            .active_write_backend
                            .store(std::sync::Arc::new(healthy_be.clone()));
                        // Switch is completed in-memory.
                    } else {
                        log::error!("Failover failed: No healthy storage backends available!");
                    }
                }
            }
        });
    }
}

async fn perform_device_health_check(dev: &crate::nvme_dev::NvmeBlockDev) -> bool {
    if !std::path::Path::new(&dev.device_path).exists() {
        return false;
    }
    match dev.read_block(0, 4096).await {
        Ok(_) => true,
        Err(e) => {
            log::warn!(
                "Device health check failed for path {}: {:?}",
                dev.device_path,
                e
            );
            false
        }
    }
}

pub struct DataRouterInner {
    pub dlm: DlmClient,
    pub meta_backend:
        once_cell::sync::OnceCell<std::sync::Arc<crate::meta_backend::RoutedMetaBackend>>,
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
    pub fn set_meta_backend(
        &self,
        meta_backend: std::sync::Arc<crate::meta_backend::RoutedMetaBackend>,
    ) {
        let _ = self.inner.meta_backend.set(meta_backend);
    }

    fn parse_block_mapping(&self, mapping_str: &str) -> Result<(u64, u64, usize)> {
        let default_size = self.block_size.load(Ordering::Acquire) as usize;
        let parts: Vec<&str> = mapping_str.split(':').collect();
        if parts.len() == 3 {
            let bk = self.backend_router.parse_block_offset(parts[0])?;
            let off = parts[1].parse::<u64>().unwrap_or(0);
            let sz = parts[2].parse::<usize>().unwrap_or(default_size);
            Ok((bk, off, sz))
        } else {
            let bk = self.backend_router.parse_block_offset(mapping_str)?;
            Ok((bk, 0, default_size))
        }
    }

    pub(crate) async fn fetch_metadata_from_backend(
        &self,
        ino: u64,
    ) -> Result<Option<CachedMetadata>> {
        if let Some(backend) = self.inner.meta_backend.get() {
            let xattr_res = backend.getxattr(ino, "layout").await?;
            if let Some(bytes) = xattr_res {
                if let Ok(layout) = serde_json::from_slice::<LayoutMetadata>(&bytes) {
                    return Ok(Some(CachedMetadata {
                        file_type: layout.file_type,
                        size: layout.size,
                        block_map_id: layout.block_map_id,
                        block_prefix: layout.block_prefix,
                        file_id: layout.file_id,
                        cached_at: std::time::Instant::now(),
                        data_key: layout.data_key,
                        block_map: layout.block_map,
                    }));
                }
            }
        }
        Ok(None)
    }

    pub(crate) async fn save_metadata_to_backend(
        &self,
        ino: u64,
        m: &CachedMetadata,
        _fencing_token: u64,
    ) -> Result<()> {
        let backend = self.inner.meta_backend.get().ok_or_else(|| {
            SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
        })?;

        let layout = LayoutMetadata {
            file_type: m.file_type.clone(),
            size: m.size,
            block_map_id: m.block_map.as_ref().map(|_| format!("block_map_{}", ino)),
            block_prefix: m.block_prefix.clone(),
            file_id: m.file_id.clone(),
            data_key: m.data_key.clone(),
            block_map: m.block_map.clone(),
        };
        let bytes = serde_json::to_vec(&layout).map_err(|e| {
            SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to serialize layout: {:?}", e),
            ))
        })?;
        backend.setxattr(ino, "layout", &bytes).await?;
        let _ = backend.setattr(ino, None, Some(m.size)).await;
        Ok(())
    }

    pub fn new(
        dlm: DlmClient,
        cache: TieredCache,
        block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
        nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    ) -> Self {
        let default_block_size = std::env::var("SQUEEZEFS_DEFAULT_BLOCK_SIZE")
            .ok()
            .and_then(|val| val.parse::<u64>().ok())
            .unwrap_or(4 * 1024 * 1024);
        let block_size = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(default_block_size));
        let backend_router = std::sync::Arc::new(BackendRouter::new(
            block_allocator.clone(),
            nvme_writer.clone(),
            block_size.clone(),
        ));
        cache.set_backend_router(backend_router.clone());

        let fs_name = crate::fs_prefix();
        let redis_url = dlm.redis_url().to_string();
        backend_router.start_health_check_worker(redis_url, fs_name.to_string());

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
                meta_backend: once_cell::sync::OnceCell::new(),
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

        let ino = parse_inode_from_path(file_path);
        if let Some(m) = self.fetch_metadata_from_backend(ino).await? {
            self.metadata_cache.insert(file_path.to_string(), m.clone());
            return Ok(m);
        }

        // If not found, return a default inline metadata (e.g. newly created file)
        let m = CachedMetadata {
            file_type: "inline".to_string(),
            size: 0,
            block_map_id: None,
            block_prefix: None,
            file_id: None,
            cached_at: std::time::Instant::now(),
            data_key: None,
            block_map: None,
        };
        self.metadata_cache.insert(file_path.to_string(), m.clone());
        Ok(m)
    }

    pub async fn load_striped_block_keys(
        &self,
        _file_path: &str,
        meta: &CachedMetadata,
        start_block: u32,
        end_block: u32,
    ) -> Result<Vec<(u32, Option<String>)>> {
        let mut block_keys = Vec::new();

        if let Some(block_map) = &meta.block_map {
            for b in start_block..=end_block {
                let key_opt = block_map.get(&b).cloned();
                block_keys.push((b, key_opt));
            }
        } else if let Some(block_map_id) = &meta.block_map_id {
            for b in start_block..=end_block {
                let cache_key = (block_map_id.clone(), b);
                if let Some(entry) = self.block_map_cache.get(&cache_key) {
                    let (bk, _) = &entry;
                    block_keys.push((b, bk.clone()));
                } else {
                    block_keys.push((b, None));
                }
            }
        } else if let Some(block_prefix) = &meta.block_prefix {
            for b in start_block..=end_block {
                block_keys.push((b, Some(format!("{}/part_{}", block_prefix, b))));
            }
        } else {
            return Err(SqueezefsError::InvalidOperation(
                "Missing block_map and block_prefix for striped file".to_string(),
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
                                        // P2-8: path-based cache write via process io_uring worker.
                                        if let Err(e) = crate::uring_fs::write_all(
                                            &local_path,
                                            downloaded.clone(),
                                        )
                                        .await
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

        let current_fencing = self.dlm.get_fencing_token(file_path);
        if fencing_token < current_fencing {
            return Err(crate::error::SqueezefsError::FencingTokenExpired {
                token: fencing_token,
                expected: current_fencing,
            });
        }

        let meta = self.fetch_metadata(file_path).await?;
        if meta.file_type == "striped" {
            self.write_striped(file_path, &meta_key, offset, data, fencing_token)
                .await?;
            crate::fuse_client::METRICS
                .layout_striped_writes
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        // Read existing data
        let mut existing_data = match meta.file_type.as_str() {
            "inline" => {
                if let Some(ref d) = meta.data_key {
                    d.clone()
                } else {
                    Vec::new()
                }
            }
            "staged" => {
                if let Some(ref file_id) = meta.file_id {
                    if let Some(staged_data) = self.cache.nvme.read_staged(file_id) {
                        staged_data
                    } else {
                        let mapping_opt =
                            meta.block_map.as_ref().and_then(|bm| bm.get(&0).cloned());
                        if let Some(mapping_str) = mapping_opt {
                            let (offset_u64, off, sz) = self.parse_block_mapping(&mapping_str)?;
                            let packed_bytes =
                                self.nvme_writer.read_block(offset_u64 + off, sz).await?;
                            self.get_crypto().process_read(&packed_bytes)?.into_owned()
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

        // Patch the data
        let end_offset = (offset as usize) + data.len();
        let stripe_threshold = if self.cache.nvme.staging_dirs().is_empty() {
            4 * 1024
        } else {
            self.block_size.load(Ordering::Acquire) as usize
        };

        if end_offset > stripe_threshold {
            // Transition layout → striped.
            if existing_data.len() < end_offset {
                existing_data.resize(end_offset, 0);
            }
            existing_data[offset as usize..end_offset].copy_from_slice(&data);

            let existing_bytes = bytes::Bytes::from(existing_data);
            let new_size = existing_bytes.len();

            let (block_mappings, _sizes, _block_count) =
                self.durable_write_stripe_payload(existing_bytes).await?;

            let mut block_map = std::collections::HashMap::new();
            for (idx_str, key) in block_mappings {
                if let Ok(idx) = idx_str.parse::<u32>() {
                    block_map.insert(idx, key);
                }
            }

            let mut updated_meta = meta.clone();
            updated_meta.file_type = "striped".to_string();
            updated_meta.size = new_size as u64;
            updated_meta.block_map = Some(block_map);
            self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                .await?;

            self.cache.write_lru.remove(file_path);
            self.cache.read_lru.remove(file_path);

            self.metadata_cache
                .insert(file_path.to_string(), updated_meta);
            crate::fuse_client::METRICS
                .layout_striped_writes
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        if existing_data.len() < end_offset {
            existing_data.resize(end_offset, 0);
        }
        existing_data[offset as usize..end_offset].copy_from_slice(&data);
        let new_size = existing_data.len();

        if new_size < 4 * 1024 {
            // Layout: inline
            crate::fuse_client::METRICS
                .layout_inline_writes
                .fetch_add(1, Ordering::Relaxed);
            let shared_data = bytes::Bytes::from(existing_data);

            let mut updated_meta = meta.clone();
            updated_meta.file_type = "inline".to_string();
            updated_meta.size = new_size as u64;
            updated_meta.data_key = Some(shared_data.to_vec());
            updated_meta.file_id = None;
            updated_meta.block_map = None;
            self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                .await?;

            self.cache.write_lru.put(file_path, shared_data.clone());
            self.cache.read_lru.put(file_path, shared_data);
            self.metadata_cache
                .insert(file_path.to_string(), updated_meta);
        } else if !self.cache.nvme.staging_dirs().is_empty()
            && (new_size as u64) <= self.block_size.load(Ordering::Acquire)
        {
            // Layout: staged
            crate::fuse_client::METRICS
                .layout_staged_writes
                .fetch_add(1, Ordering::Relaxed);
            let new_file_id = Uuid::new_v4().to_string();

            let stage_res = self
                .cache
                .nvme
                .stage_write(file_path, &new_file_id, &existing_data, fencing_token)
                .await;

            let shared_data = bytes::Bytes::from(existing_data);

            match stage_res {
                Ok(_) => {
                    let mut updated_meta = meta.clone();
                    updated_meta.file_type = "staged".to_string();
                    updated_meta.size = new_size as u64;
                    updated_meta.file_id = Some(new_file_id);
                    updated_meta.data_key = None;
                    updated_meta.block_map = None;
                    self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                        .await?;
                    self.metadata_cache
                        .insert(file_path.to_string(), updated_meta);
                }
                Err(SqueezefsError::Io(ref e)) if e.kind() == std::io::ErrorKind::StorageFull => {
                    log::warn!("NVMe write staging cache full. Falling back to direct synchronous backend block write for: {}", file_path);
                    let processed_data = self.get_crypto().process_write(shared_data.clone())?;

                    let (_be_id, block_allocator, nvme_writer) =
                        self.backend_router.get_active_backend()?;
                    let be_offset = block_allocator.allocate_block().await?;
                    let stored_block_key = be_offset.to_string();

                    nvme_writer.write_block(be_offset, &processed_data).await?;

                    let mut block_map = std::collections::HashMap::new();
                    block_map.insert(0, stored_block_key);

                    let mut updated_meta = meta.clone();
                    updated_meta.file_type = "staged".to_string();
                    updated_meta.size = new_size as u64;
                    updated_meta.file_id = Some(new_file_id);
                    updated_meta.data_key = None;
                    updated_meta.block_map = Some(block_map);
                    self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                        .await?;
                    self.metadata_cache
                        .insert(file_path.to_string(), updated_meta);
                }
                Err(e) => return Err(e),
            }

            self.cache.write_lru.put(file_path, shared_data.clone());
            self.cache.read_lru.put(file_path, shared_data);
        } else {
            // First-time striped layout
            let existing_bytes = bytes::Bytes::from(existing_data);
            let new_size = existing_bytes.len();

            let (block_mappings, _sizes, _block_count) =
                self.durable_write_stripe_payload(existing_bytes).await?;

            let mut block_map = std::collections::HashMap::new();
            for (idx_str, key) in block_mappings {
                if let Ok(idx) = idx_str.parse::<u32>() {
                    block_map.insert(idx, key);
                }
            }

            let mut updated_meta = meta.clone();
            updated_meta.file_type = "striped".to_string();
            updated_meta.size = new_size as u64;
            updated_meta.block_map = Some(block_map);
            self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                .await?;

            self.cache.write_lru.remove(file_path);
            self.cache.read_lru.remove(file_path);

            crate::fuse_client::METRICS
                .layout_striped_writes
                .fetch_add(1, Ordering::Relaxed);
            self.metadata_cache
                .insert(file_path.to_string(), updated_meta);
        }

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

            let (_be_id, block_allocator, nvme_writer) =
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
            let stored_block_key = offset.to_string();
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

    /// Striped RMW. P1-10: meta connections are phased — open for block-map
    /// reads, **dropped** before durable block I/O, re-acquired only for the
    /// atomic map/size commit and refcount cleanup (frees run after redis work).
    async fn write_striped(
        &self,
        file_path: &str,
        _meta_key: &str,
        offset: u64,
        data: bytes::Bytes,
        _fencing_token: u64,
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

        let meta = self.fetch_metadata(file_path).await?;
        let existing_size = meta.size;
        let block_map = meta.block_map.clone().unwrap_or_default();

        let mut old_block_keys = Vec::new();
        for b in start_block..=end_block {
            old_block_keys.push(block_map.get(&b).cloned());
        }

        // Pre-resolve cache hits on the main thread and spawn tasks to modify affected blocks concurrently
        use futures::stream::{FuturesUnordered, StreamExt};
        let tasks = FuturesUnordered::new();
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(
            crate::bg_admit::striped_block_concurrency(),
        ));

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

                unsafe {
                    block_data
                        .get_unchecked_mut(rel_start..rel_end)
                        .copy_from_slice(&data_slice);
                }

                let (_be_id, block_allocator, nvme_writer) =
                    router_clone.backend_router.get_active_backend()?;
                let offset = block_allocator.allocate_block().await?;
                let stored_new_block_key = offset.to_string();

                let block_bytes = bytes::Bytes::from(block_data.into_inner());

                read_lru.put(&stored_new_block_key, block_bytes.clone());

                let logical_size = block_bytes.len();
                let processed_block = crypto.process_write(block_bytes)?;
                let physical_size = processed_block.len();
                nvme_writer.write_block(offset, &processed_block).await?;

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
            for (_b, _old, new_key, _logical, _physical) in &results {
                let _ = self.backend_router.free_block(new_key).await;
            }
            return Err(e);
        }

        let mut updated_block_map = block_map;
        for res in &results {
            let (b, old_block_key, new_block_key, _, _) =
                (res.0, res.1.clone(), res.2.clone(), res.3, res.4);
            updated_block_map.insert(b, new_block_key);

            if let Some(bk) = old_block_key {
                self.cache.read_lru.remove(&bk);
                let _ = self.backend_router.free_block(&bk).await;
            }
        }

        let new_size = std::cmp::max(existing_size, end_pos);
        let mut updated_meta = meta.clone();
        updated_meta.size = new_size;
        updated_meta.block_map = Some(updated_block_map);
        self.save_metadata_to_backend(ino, &updated_meta, _fencing_token)
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

        self.metadata_cache
            .insert(file_path.to_string(), updated_meta);
        Ok(())
    }

    /// Read file data, attempting to satisfy the read via the fastest cache tier.
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

        // Fetch file metadata from local cache or metadata backend
        let meta = self.fetch_metadata(file_path).await?;

        let data = match meta.file_type.as_str() {
            "inline" => {
                if let Some(ref d) = meta.data_key {
                    d.clone()
                } else {
                    Vec::new()
                }
            }
            "staged" => {
                let file_id = meta.file_id.as_ref().ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Missing file_id for staged file".to_string())
                })?;

                if let Some(staged_data) = self.cache.nvme.read_staged(file_id) {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    staged_data
                } else {
                    let mapping_opt = meta.block_map.as_ref().and_then(|bm| bm.get(&0).cloned());
                    if let Some(mapping_str) = mapping_opt {
                        let (offset_u64, off, sz) = self.parse_block_mapping(&mapping_str)?;
                        let packed_bytes =
                            self.nvme_writer.read_block(offset_u64 + off, sz).await?;
                        self.get_crypto().process_read(&packed_bytes)?.into_owned()
                    } else {
                        return Err(SqueezefsError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Staged file ID {} mapping not found in metadata", file_id),
                        )));
                    }
                }
            }
            "striped" => {
                let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
                let meta_size = meta.size;
                let num_blocks = meta_size.div_ceil(block_size) as u32;

                let block_keys = self
                    .load_striped_block_keys(file_path, &meta, 0, num_blocks.saturating_sub(1))
                    .await?;

                // P1-5: bounded concurrency (order preserved via index sort).
                use futures::stream::{self, StreamExt};
                let mut indexed: Vec<(usize, Result<crate::cache::pool::ReadBlockValue>)> =
                    stream::iter(block_keys.into_iter().enumerate().map(
                        |(i, (_, block_key_opt))| {
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
                        },
                    ))
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
                    meta.file_type
                )));
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

        // Fetch file metadata from local cache or metadata backend
        let meta = self.fetch_metadata(file_path).await?;

        match meta.file_type.as_str() {
            "inline" => {
                let decompressed = if let Some(ref data) = meta.data_key {
                    data.clone()
                } else {
                    Vec::new()
                };
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
                    let mapping_opt = meta.block_map.as_ref().and_then(|bm| bm.get(&0).cloned());
                    if let Some(mapping_str) = mapping_opt {
                        let (offset_u64, off, sz) = self.parse_block_mapping(&mapping_str)?;
                        let packed_bytes =
                            self.nvme_writer.read_block(offset_u64 + off, sz).await?;
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
                            format!("Staged file ID {} mapping not found in metadata", file_id),
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
        dest_addr: Option<u64>,
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
            let len = end - start;
            let data = if let Some(dest) = dest_addr {
                let dest_ptr = dest as *mut u8;
                unsafe {
                    std::ptr::copy_nonoverlapping(cached_data[start..end].as_ptr(), dest_ptr, len);
                    bytes::Bytes::from_static(std::slice::from_raw_parts(dest_ptr, len))
                }
            } else {
                // SAFETY: start and end are clamped to cached_data.len()
                unsafe {
                    let sub = cached_data.get_unchecked(start..end);
                    cached_data.slice_ref(sub)
                }
            };
            return Ok((data, None));
        }

        // Fetch file metadata from local cache or Garnet
        let meta = self.fetch_metadata(file_path).await?;

        match meta.file_type.as_str() {
            "inline" => {
                let decompressed = if let Some(ref data) = meta.data_key {
                    data.clone()
                } else {
                    Vec::new()
                };
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
                    let len = end - start;
                    let (data, backing) = if let Some(dest) = dest_addr {
                        let dest_ptr = dest as *mut u8;
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                guard[start..end].as_ptr(),
                                dest_ptr,
                                len,
                            );
                            let d = bytes::Bytes::from_static(std::slice::from_raw_parts(
                                dest_ptr, len,
                            ));
                            (
                                d,
                                Some(std::sync::Arc::new(guard)
                                    as std::sync::Arc<dyn std::any::Any + Send + Sync>),
                            )
                        }
                    } else {
                        let mut sliced_guard = guard;
                        sliced_guard.offset += start;
                        sliced_guard.len = len;
                        let d = bytes::Bytes::copy_from_slice(&sliced_guard);
                        (
                            d,
                            Some(std::sync::Arc::new(sliced_guard)
                                as std::sync::Arc<dyn std::any::Any + Send + Sync>),
                        )
                    };
                    Ok((data, backing))
                } else {
                    let mapping_opt = meta.block_map.as_ref().and_then(|bm| bm.get(&0).cloned());
                    if let Some(bk) = mapping_opt {
                        let offset_u64 = self.backend_router.parse_block_offset(&bk)?;
                        let sz = self.block_size.load(Ordering::Acquire) as usize;
                        let packed_bytes = self.nvme_writer.read_block(offset_u64, sz).await?;
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
                            format!("Staged file ID {} mapping not found in metadata", file_id),
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
                        let len = end - start;
                        let (data, backing) = if let Some(dest) = dest_addr {
                            let dest_ptr = dest as *mut u8;
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    guard[start..end].as_ptr(),
                                    dest_ptr,
                                    len,
                                );
                                let d = bytes::Bytes::from_static(std::slice::from_raw_parts(
                                    dest_ptr, len,
                                ));
                                (
                                    d,
                                    Some(std::sync::Arc::new(guard)
                                        as std::sync::Arc<dyn std::any::Any + Send + Sync>),
                                )
                            }
                        } else {
                            let mut sliced_guard = guard;
                            sliced_guard.offset += start;
                            sliced_guard.len = len;
                            let d = bytes::Bytes::copy_from_slice(&sliced_guard);
                            (
                                d,
                                Some(std::sync::Arc::new(sliced_guard)
                                    as std::sync::Arc<dyn std::any::Any + Send + Sync>),
                            )
                        };
                        return Ok((data, backing));
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
                                let len = guard.len();
                                let data = if let Some(dest) = dest_addr {
                                    let dest_ptr = dest as *mut u8;
                                    unsafe {
                                        std::ptr::copy_nonoverlapping(
                                            guard.as_ptr(),
                                            dest_ptr,
                                            len,
                                        );
                                        bytes::Bytes::from_static(std::slice::from_raw_parts(
                                            dest_ptr, len,
                                        ))
                                    }
                                } else {
                                    bytes::Bytes::copy_from_slice(&guard)
                                };
                                return Ok((data, Some(std::sync::Arc::new(guard))));
                            } else {
                                // Single block cache miss: download directly in-line (zero-copy, no spawn)
                                let downloaded = if let Some(dest) = dest_addr {
                                    self.backend_router
                                        .read_block_with_dest(
                                            b_key,
                                            block_size as usize,
                                            Some(dest),
                                        )
                                        .await?;
                                    let len = block_size as usize;
                                    let dest_ptr = dest as *mut u8;
                                    let b = unsafe {
                                        bytes::Bytes::from_static(std::slice::from_raw_parts(
                                            dest_ptr, len,
                                        ))
                                    };
                                    crate::cache::pool::ReadBlockValue::Bytes(b)
                                } else {
                                    self.get_cached_or_fetch_block(b_key).await?
                                };
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
                                let data = if let Some(dest) = dest_addr {
                                    unsafe {
                                        bytes::Bytes::from_static(std::slice::from_raw_parts(
                                            dest as *mut u8,
                                            end - start,
                                        ))
                                    }
                                } else {
                                    let slice: &[u8] = &downloaded[start..end];
                                    bytes::Bytes::copy_from_slice(slice)
                                };
                                return Ok((data, Some(std::sync::Arc::new(downloaded))));
                            }
                        } else {
                            // Hole support: return zero-filled slice
                            let len = slice_len as usize;
                            let data = if let Some(dest) = dest_addr {
                                let dest_ptr = dest as *mut u8;
                                unsafe {
                                    std::ptr::write_bytes(dest_ptr, 0, len);
                                    bytes::Bytes::from_static(std::slice::from_raw_parts(
                                        dest_ptr, len,
                                    ))
                                }
                            } else {
                                let mut hole_pooled = BUFFER_POOL.alloc();
                                hole_pooled.resize(len, 0);
                                bytes::Bytes::copy_from_slice(&hole_pooled)
                            };
                            return Ok((data, None));
                        }
                    }
                }

                // 2. Multi-block or cache miss: load and assemble using pooled buffer
                let block_keys = self
                    .load_striped_block_keys(file_path, &meta, start_block, end_block)
                    .await?;

                let final_len = (end_offset - offset) as usize;
                let (raw_ptr, final_buf_opt) = if let Some(dest) = dest_addr {
                    (dest as usize, None)
                } else {
                    let mut final_buf = BUFFER_POOL.alloc();
                    final_buf.resize(final_len, 0);
                    let ptr = final_buf.as_mut_ptr() as usize;
                    (ptr, Some(final_buf))
                };

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

                if let Some(final_buf) = final_buf_opt {
                    let data = bytes::Bytes::copy_from_slice(&final_buf[..final_len]);
                    Ok((data, Some(std::sync::Arc::new(final_buf))))
                } else {
                    let data = unsafe {
                        bytes::Bytes::from_static(std::slice::from_raw_parts(
                            dest_addr.unwrap() as *mut u8,
                            final_len,
                        ))
                    };
                    Ok((data, None))
                }
            }
            _ => Err(SqueezefsError::InvalidOperation(format!(
                "Unknown file type: {}",
                meta.file_type
            ))),
        }
    }

    /// Retrieve the file size from metadata.
    pub async fn get_file_size(&self, file_path: &str) -> Result<u64> {
        let meta = self.fetch_metadata(file_path).await?;
        Ok(meta.size)
    }

    pub fn cache(&self) -> &TieredCache {
        &self.cache
    }

    /// Clone a file metadata-only. If it's inline, copy the inline data.
    /// If it's staged, copy the staging folder/files and mapping.
    /// If it's striped, copy the block map and increment all block reference counts.
    pub async fn clone_file(&self, src: &str, dest: &str) -> Result<()> {
        let _src_ino = parse_inode_from_path(src);
        let dest_ino = parse_inode_from_path(dest);

        let _src_lock = self
            .dlm
            .acquire_lock_with_retry(src, None, std::time::Duration::from_secs(5), 5)
            .await?;
        let _dest_lock = self
            .dlm
            .acquire_lock_with_retry(dest, None, std::time::Duration::from_secs(5), 5)
            .await?;

        let meta = self.fetch_metadata(src).await?;

        let mut updated_meta = meta.clone();
        if meta.file_type == "staged" {
            let file_id = meta.file_id.as_ref().ok_or_else(|| {
                SqueezefsError::InvalidOperation("Missing file_id for staged file".to_string())
            })?;
            let new_file_id = Uuid::new_v4().to_string();
            if let Some(data) = self.cache.nvme.read_staged(file_id) {
                self.cache
                    .nvme
                    .stage_write(dest, &new_file_id, &data, _dest_lock.fencing_token())
                    .await?;
            }
            updated_meta.file_id = Some(new_file_id);
        } else if meta.file_type == "striped" {
            if let Some(ref block_map) = meta.block_map {
                for bk in block_map.values() {
                    self.backend_router.increment_refcount(bk);
                }
            }
        }

        self.save_metadata_to_backend(dest_ino, &updated_meta, _dest_lock.fencing_token())
            .await?;

        let mut cached_opt = self.cache.write_lru.get(src);
        if cached_opt.is_none() {
            cached_opt = self.cache.read_lru.get(src);
        }
        if let Some(cached) = cached_opt {
            self.cache.write_lru.put(dest, cached);
        }

        self.metadata_cache.insert(dest.to_string(), updated_meta);
        Ok(())
    }

    /// Safely delete all underlying storage files/blocks associated with the file.
    pub async fn delete_file(
        &self,
        file_path: &str,
        _con: &mut crate::dlm::MetaConnection,
    ) -> Result<()> {
        let ino = parse_inode_from_path(file_path);
        let meta = self.fetch_metadata(file_path).await?;

        if meta.file_type == "striped" {
            if let Some(ref block_map) = meta.block_map {
                for bk in block_map.values() {
                    self.cache.read_lru.remove(bk);
                    let _ = self.backend_router.free_block(bk).await;
                }
            }
        } else if meta.file_type == "staged" {
            if let Some(ref file_id) = meta.file_id {
                self.cache.nvme.remove_staged(file_id);
                if let Some(ref block_map) = meta.block_map {
                    if let Some(bk) = block_map.get(&0) {
                        let _ = self.backend_router.free_block(bk).await;
                    }
                }
            }
        }

        if let Some(backend) = self.inner.meta_backend.get() {
            let _ = backend.removexattr(ino, "layout").await;
        }

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

    /// Resolve a logical filesystem path (e.g., "/dir1/file.txt") to its FUSE inode number.
    pub async fn resolve_path_to_inode(&self, path: &str) -> Result<u64> {
        let mut current_ino = 1u64; // Root inode

        if let Some(backend) = self.inner.meta_backend.get() {
            for part in path.split('/') {
                if part.is_empty() || part == "." {
                    continue;
                }
                let inode = backend.lookup(current_ino, part).await?;
                current_ino = inode.ino;
            }
        }

        Ok(current_ino)
    }

    /// Clone a path to another path metadata-only.
    pub async fn clone_path(&self, src_path: &str, dest_path: &str) -> Result<()> {
        let src_ino = self.resolve_path_to_inode(src_path).await?;

        let dest_p = std::path::Path::new(dest_path);
        let parent_str = dest_p.parent().and_then(|p| p.to_str()).unwrap_or("");
        let file_name = dest_p.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            SqueezefsError::InvalidOperation("Invalid destination filename".to_string())
        })?;

        let parent_ino = self.resolve_path_to_inode(parent_str).await?;

        if let Some(backend) = self.inner.meta_backend.get() {
            let src_inode = backend.getattr(src_ino).await?;
            let dest_inode = backend
                .create(parent_ino, file_name, src_inode.mode)
                .await?;
            let _ = backend
                .setattr(dest_inode.ino, Some(src_inode.mode), Some(src_inode.size))
                .await?;

            self.clone_file(
                crate::keys::inode_path(src_ino).as_str(),
                crate::keys::inode_path(dest_inode.ino).as_str(),
            )
            .await?;
        }

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
