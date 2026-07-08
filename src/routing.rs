use crate::cache::{TieredCache, BUFFER_POOL};
use crate::dlm::DlmClient;

pub const MAX_INLINE_SIZE: usize = 4096;

use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::meta_backend::Metadata;
use crate::stripe_locks::StripeLocks;
use log::debug;
use std::sync::atomic::Ordering;

use std::time::Duration;
use uuid::Uuid;

/// Per-inode serialization for striped layout (`block_map`/size) mutation and
/// the `fetch_metadata` backend refill.
///
/// Concurrent striped writers (the kernel flushes a large file's dirty pages in
/// parallel) each COW their blocks to fresh keys and must merge them into the
/// file's block map. Two races corrupt data without this lock:
///  * **Lost update:** a non-atomic read-merge-save over a stale snapshot drops
///    the other writers' just-committed entries — a dropped entry reverts to a
///    freed key whose physical block is then reallocated, so the block reads as
///    zeros or, worse, as another block's data.
///  * **Stale refill:** `fetch_metadata`'s TTL refill reads a backend snapshot
///    and inserts it into `metadata_cache`; interleaved with a writer's commit
///    it can clobber the fresh entry with the stale map.
///
/// Held only for the short read→merge→save; block *data* I/O stays concurrent
/// (COW) outside the lock.
static INODE_META_LOCKS: once_cell::sync::Lazy<StripeLocks<tokio::sync::Mutex<()>, 4096>> =
    once_cell::sync::Lazy::new(StripeLocks::new);

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

#[derive(serde::Serialize, serde::Deserialize, Clone, Default, Debug)]
pub struct LayoutMetadata {
    pub file_type: String,
    pub size: u64,
    pub block_map_id: Option<String>,
    pub block_prefix: Option<String>,
    pub file_id: Option<String>,
    pub data_key: Option<Vec<u8>>,
    pub block_map: Option<std::collections::HashMap<u32, String>>,
}

#[derive(Clone, Debug)]
pub struct CachedMetadata {
    pub file_type: String,
    pub size: u64,
    pub block_map_id: Option<String>,
    pub block_prefix: Option<String>,
    pub file_id: Option<String>,
    pub cached_at: std::time::Instant,
    /// Inline payload held zero-copy: `Bytes` clones are O(1) refcount bumps, so
    /// hot-path `metadata_cache` gets / `meta.clone()` don't deep-copy the file.
    pub data_key: Option<bytes::Bytes>,
    pub block_map: Option<std::collections::HashMap<u32, String>>,
    /// When true, layout/size live only in RAM (+ staging mmap); must persist on fsync/release.
    pub layout_dirty: bool,
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
            layout_dirty: false,
        }
    }
}

/// The mutation shapes of [`DataRouter::merge_block_mappings`] (§5.3 "One
/// merge discipline") — inserts, removals, AND size-only snapshot-saves —
/// so truncate/fallocate share the serialization domain instead of racing
/// it.
pub enum BlockMapOp<'a> {
    /// Insert/overwrite entries: write-through, flush paths, defrag
    /// `BlockMove`, routing striped merge. `(block_idx, new_block_key)`
    /// pairs.
    ///
    /// `Merge(&[])` is the DEGENERATE, size-only case: no entries change,
    /// but the primitive still re-reads the CURRENT meta under
    /// `INODE_META_LOCKS` and saves size/map from that — which is exactly
    /// what makes the stale-snapshot whole-meta saves (truncate-grow,
    /// fallocate-extend) safe: they can no longer rewrite the block map
    /// "without mutating it".
    Merge(&'a [(u32, String)]),
    /// Remove every block whose start offset ≥ `new_size`
    /// (truncate-shrink): the old `retain`-and-save re-expressed as a
    /// removal set on the same primitive; removed keys come back as the
    /// free list.
    TruncateFrom { new_size: u64 },
}

/// Layout-field policy for [`DataRouter::merge_block_mappings`] — an
/// EXPLICIT parameter, because the converted writers disagree today and a
/// silent "extracted-body default" would change the flush paths'
/// side-effects.
pub enum LayoutFlip {
    /// Flush-path merges (`flush_single_active_block`, `flush_due_…`,
    /// `upload_active_block_bytes`, write-through): force
    /// `file_type = "striped"` but PRESERVE `file_id` / `data_key` —
    /// today's exact field writes. Behavior-preserving by construction.
    ToStripedKeepStagedIdentity,
    /// Layout transitions (routing striped merge): `file_type = "striped"`
    /// AND clear `file_id` / `data_key`. Staged-identity release
    /// bookkeeping (`release_superseded_staged` — ring-entry/budget
    /// release) stays with the CALLER: the primitive never releases staged
    /// identity itself, so a clear is never paired with zero or two
    /// releases.
    ToStripedClearStagedIdentity,
    /// Truncate / fallocate / defrag mutations: leave `file_type`,
    /// `file_id`, `data_key` untouched (truncate's inline/staged handling
    /// stays in its caller — the primitive only owns the striped map +
    /// size).
    KeepLayout,
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

    pub fn get_backend_health(&self, be_id: &str) -> u32 {
        if !self.is_backend_healthy(be_id) {
            return 0;
        }

        let (allocator, device_path) = if be_id == "backend_0" {
            (
                self.default_allocator.clone(),
                self.default_device.device_path.clone(),
            )
        } else if let Some(be) = self.backends.get(be_id) {
            (be.block_allocator.clone(), be.device.device_path.clone())
        } else {
            return 0;
        };

        // Query real device capacity
        let mut dev_size = 100 * 1024 * 1024 * 1024;
        if let Ok(metadata) = std::fs::metadata(&device_path) {
            let len = metadata.len();
            if len > 0 {
                dev_size = len;
            } else if let Ok(mut file) = std::fs::File::open(&device_path) {
                use std::io::Seek;
                if let Ok(len) = file.seek(std::io::SeekFrom::End(0)) {
                    if len > 0 {
                        dev_size = len;
                    }
                }
            }
        }

        let total_blocks = (dev_size / (4 * 1024 * 1024)).max(1);
        let used_blocks = allocator.get_used_blocks();
        let free_blocks = total_blocks.saturating_sub(used_blocks);
        let free_factor = free_blocks as f64 / total_blocks as f64;

        let perf_factor = 1.0;

        let score = (free_factor * 1000.0 * perf_factor) as u32;
        score.min(1000)
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
            let health = self.get_backend_health("backend_0");
            healthy_backends.push((
                "backend_0".to_string(),
                self.default_allocator.clone(),
                self.default_device.clone(),
                health,
            ));
        }

        for entry in self.backends.iter() {
            let be_id = entry.key();
            if self.is_backend_healthy(be_id) {
                let health = self.get_backend_health(be_id);
                healthy_backends.push((
                    be_id.clone(),
                    entry.value().block_allocator.clone(),
                    entry.value().device.clone(),
                    health,
                ));
            }
        }

        if healthy_backends.is_empty() {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "No healthy storage backends available for write",
            )));
        }

        // Sort by health descending
        healthy_backends.sort_by(|a, b| b.3.cmp(&a.3));

        let max_health = healthy_backends[0].3;
        // Filter candidates within 90% of max health
        let candidates: Vec<_> = healthy_backends
            .into_iter()
            .filter(|b| b.3 >= (max_health * 9) / 10)
            .collect();

        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let idx = COUNTER.fetch_add(1, Ordering::Relaxed) % candidates.len();
        let selected = &candidates[idx];

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

        if !self.is_backend_healthy(&be_id) {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                format!("Storage volume '{}' is disabled/offline", be_id),
            )));
        }

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

    /// Take one reference on the block behind `block_key`. `false` = the
    /// reference was NOT taken (freed/untracked offset, unknown backend, or
    /// unparsable key) — the caller must re-resolve, never proceed unpinned.
    #[must_use]
    pub fn increment_refcount(&self, block_key: &str) -> bool {
        if let Ok((be_id, offset)) = self.parse_block_key(block_key) {
            if be_id == "backend_0" {
                self.default_allocator.increment_refcount(offset)
            } else if let Some(be) = self.backends.get(&be_id) {
                be.block_allocator.increment_refcount(offset)
            } else {
                false
            }
        } else {
            false
        }
    }

    fn punch_hole_sync(device_path: &str, offset: u64, size: u64) {
        #[cfg(target_os = "linux")]
        {
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open(device_path) {
                use std::os::unix::io::AsRawFd;
                let fd = file.as_raw_fd();
                unsafe {
                    let _ = libc::fallocate(
                        fd,
                        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                        offset as libc::off_t,
                        size as libc::off_t,
                    );
                }
            }
        }
    }

    /// The allocator that owns a block key's offset (see incarnation seqlock in
    /// [`crate::block_allocator::BlockAllocator`]).
    fn allocator_for_key(
        &self,
        block_key: &str,
    ) -> Option<(std::sync::Arc<crate::block_allocator::BlockAllocator>, u64)> {
        let (be_id, offset) = self.parse_block_key(block_key).ok()?;
        if be_id == "backend_0" {
            Some((self.default_allocator.clone(), offset))
        } else {
            self.backends
                .get(&be_id)
                .map(|be| (be.block_allocator.clone(), offset))
        }
    }

    /// Owner's durable device write for this block-key incarnation completed;
    /// validated cache fills may now publish bytes for it.
    pub fn publish_block(&self, block_key: &str) {
        if let Some((alloc, offset)) = self.allocator_for_key(block_key) {
            alloc.publish_block(offset);
        }
    }

    /// Incarnation snapshot for a validated cache fill (None = unstable, do not
    /// publish what you read).
    pub fn fill_incarnation(&self, block_key: &str) -> Option<u64> {
        self.allocator_for_key(block_key)
            .and_then(|(alloc, offset)| alloc.fill_incarnation(offset))
    }

    /// True if the incarnation is unchanged since the pre-read snapshot.
    pub fn fill_incarnation_still(&self, block_key: &str, before: u64) -> bool {
        match self.allocator_for_key(block_key) {
            Some((alloc, offset)) => alloc.fill_incarnation_still(offset, before),
            None => false,
        }
    }

    /// Free one reference on a block key. The hole punch is destructive
    /// device I/O and runs ONLY on the terminal release (a non-terminal
    /// free must never zero a clone's still-referenced bytes), and runs in
    /// the `begin_free` → punch → `finish_free` window — the offset is not
    /// reallocatable until after the punch, so the punch can never race a
    /// new owner's DMA at the reused offset (the acked-write lost-update
    /// class surfaced by PR 6's pinned striped concurrency test).
    pub async fn free_block(&self, block_key: &str) -> Result<()> {
        let (be_id, offset) = self.parse_block_key(block_key)?;

        let (allocator, device_path) = if be_id == "backend_0" {
            (
                self.default_allocator.clone(),
                self.default_device.device_path.clone(),
            )
        } else if let Some(be) = self.backends.get(&be_id) {
            (be.block_allocator.clone(), be.device.device_path.clone())
        } else {
            return Ok(());
        };

        if allocator.begin_free(offset) {
            let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
            Self::punch_hole_sync(&device_path, offset, block_size);
            allocator.finish_free(offset);
        }
        Ok(())
    }

    pub async fn free_blocks(&self, block_keys: &[&str]) -> Result<()> {
        for &block_key in block_keys {
            let _ = self.free_block(block_key).await;
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
            // Per-backend probe hysteresis: only FAILURE_THRESHOLD consecutive
            // hard failures mark a backend unhealthy (a starved probe under
            // saturation is inconclusive, never a flip — see crate::health).
            let mut states: std::collections::HashMap<String, crate::health::HealthState> =
                std::collections::HashMap::new();
            loop {
                interval.tick().await;

                let mut outcomes: Vec<(String, crate::health::Probe)> = Vec::new();
                outcomes.push((
                    "backend_0".to_string(),
                    perform_device_health_check(&router.default_device).await,
                ));
                for entry in router.backends.iter() {
                    outcomes.push((
                        entry.key().clone(),
                        perform_device_health_check(&entry.value().device).await,
                    ));
                }

                for (be_id, probe) in outcomes {
                    let state = states.entry(be_id.clone()).or_default();
                    match state.observe(probe) {
                        crate::health::Transition::WentUnhealthy => {
                            log::error!(
                                "Backend health check: backend '{}' is UNHEALTHY ({} consecutive probe failures)!",
                                be_id,
                                crate::health::HealthState::FAILURE_THRESHOLD
                            );
                            router.unhealthy_backends.insert(be_id, true);
                        }
                        crate::health::Transition::Recovered => {
                            log::info!("Backend health check: backend '{}' has recovered.", be_id);
                            router.unhealthy_backends.remove(&be_id);
                        }
                        crate::health::Transition::None => {}
                    }
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

async fn perform_device_health_check(dev: &crate::nvme_dev::NvmeBlockDev) -> crate::health::Probe {
    use crate::health::Probe;
    if !std::path::Path::new(&dev.device_path).exists() {
        return Probe::Failed;
    }
    // The probe shares the device's I/O lanes with real traffic: a timeout
    // means "busy", not "dead" — report it as inconclusive so saturation can
    // never flip a healthy backend offline (hysteresis in crate::health).
    match tokio::time::timeout(Duration::from_secs(2), dev.read_block(0, 4096)).await {
        Ok(Ok(_)) => Probe::Ok,
        Ok(Err(e)) => {
            log::warn!(
                "Device health check failed for path {}: {:?}",
                dev.device_path,
                e
            );
            Probe::Failed
        }
        Err(_) => {
            log::debug!(
                "Device health check timed out for path {} (busy, inconclusive)",
                dev.device_path
            );
            Probe::Inconclusive
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
                let layout_opt = if bytes.starts_with(b"{") {
                    serde_json::from_slice::<LayoutMetadata>(&bytes).ok()
                } else {
                    bincode::deserialize::<LayoutMetadata>(&bytes).ok()
                };
                if let Some(layout) = layout_opt {
                    let mut block_map = layout.block_map.clone();
                    if let Some(ref map_id) = layout.block_map_id {
                        if map_id.starts_with("indirect:") {
                            let block_key = map_id.strip_prefix("indirect:").unwrap();
                            let block_size =
                                self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
                            let raw_bytes = self
                                .backend_router
                                .read_block(block_key, block_size)
                                .await?;
                            let entries = bincode::deserialize::<Vec<(u32, u64)>>(&raw_bytes)
                                .map_err(|e| {
                                    let sample = if raw_bytes.len() >= 32 {
                                        format!("{:x?}", &raw_bytes[..32])
                                    } else {
                                        format!("{:x?}", &raw_bytes[..])
                                    };
                                    SqueezefsError::Io(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        format!(
                                            "Failed to deserialize indirect block map: {:?}, raw_bytes len: {}, sample: {}",
                                            e,
                                            raw_bytes.len(),
                                            sample
                                        ),
                                    ))
                                })?;
                            let mut map = std::collections::HashMap::new();
                            for (b, offset) in entries {
                                map.insert(b, offset.to_string());
                            }
                            block_map = Some(map);
                        }
                    }
                    return Ok(Some(CachedMetadata {
                        file_type: layout.file_type,
                        size: layout.size,
                        block_map_id: layout.block_map_id,
                        block_prefix: layout.block_prefix,
                        file_id: layout.file_id,
                        cached_at: std::time::Instant::now(),
                        data_key: layout.data_key.map(bytes::Bytes::from),
                        block_map,
                        layout_dirty: false,
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
        fencing_token: u64,
    ) -> Result<()> {
        let backend = self.inner.meta_backend.get().ok_or_else(|| {
            SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
        })?;

        // Fencing check
        let file_path = crate::keys::inode_path(ino);
        let current_fencing = self.inner.dlm.get_fencing_token_ino(ino);
        if fencing_token < current_fencing {
            return Err(SqueezefsError::FencingTokenExpired {
                token: fencing_token,
                expected: current_fencing,
            });
        }

        let mut old_indirect_to_free = None;
        let mut layout = LayoutMetadata {
            file_type: m.file_type.clone(),
            size: m.size,
            block_map_id: m.block_map_id.clone(),
            block_prefix: m.block_prefix.clone(),
            file_id: m.file_id.clone(),
            data_key: m.data_key.as_ref().map(|b| b.to_vec()),
            block_map: m.block_map.clone(),
        };

        // Determine if we need an indirect block map
        let needs_indirect = if let Some(ref bm) = m.block_map {
            bm.len() > 32
        } else {
            false
        };

        if needs_indirect {
            let bm = m.block_map.as_ref().unwrap();
            let mut entries: Vec<(u32, u64)> = Vec::with_capacity(bm.len());
            for (&b, s) in bm {
                if let Ok(offset) = self.backend_router.parse_block_offset(s) {
                    entries.push((b, offset));
                }
            }
            let mut serialized_map = bincode::serialize(&entries).map_err(|e| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Failed to serialize indirect block map: {:?}", e),
                ))
            })?;
            let aligned_len = (serialized_map.len() + 4095) & !4095;
            serialized_map.resize(aligned_len, 0);

            // Allocate or reuse indirect block offset
            let mut reuse_info = None;
            if let Some(ref map_id) = m.block_map_id {
                if map_id.starts_with("indirect:") {
                    let old_block_key = map_id.strip_prefix("indirect:").unwrap();
                    if let Ok((be_id, off)) = self.backend_router.parse_block_key(old_block_key) {
                        reuse_info = Some((be_id, off));
                    }
                }
            }

            let (be_id, offset, nvme_writer) = if let Some((be, off)) = reuse_info {
                let (_, dev) = self.backend_router.get_backend(&be)?;
                (be, off, dev)
            } else {
                let (be, block_allocator, dev) = self.backend_router.get_active_backend()?;
                let off = block_allocator.allocate_block().await?;
                (be, off, dev)
            };

            let block_key = format!("{}://{}", be_id, offset);
            let data_bytes = bytes::Bytes::from(serialized_map);
            nvme_writer.write_block(offset, data_bytes).await?;

            layout.block_map = None;
            layout.block_map_id = Some(format!("indirect:{}", block_key));
        } else {
            // Check if we need to free an old indirect block
            if let Some(ref map_id) = m.block_map_id {
                if map_id.starts_with("indirect:") {
                    let old_block_key = map_id.strip_prefix("indirect:").unwrap();
                    old_indirect_to_free = Some(old_block_key.to_string());
                }
            }
            layout.block_map_id = m.block_map.as_ref().map(|_| format!("block_map_{}", ino));
        }

        let bytes = bincode::serialize(&layout).map_err(|e| {
            SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to serialize binary layout: {:?}", e),
            ))
        })?;

        backend.set_layout_and_size(ino, &bytes, m.size).await?;
        // Keep hot cache coherent without a remove+refetch on the next write.
        let mut cached = m.clone();
        cached.cached_at = std::time::Instant::now();
        self.metadata_cache.insert(file_path.to_string(), cached);

        if let Some(ref old_key) = old_indirect_to_free {
            let _ = self.backend_router.free_block(old_key).await;
        }

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

        let router = Self {
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
        };
        // Merge-worker promotion commits layout through the router (weak:
        // the router owns the cache, never the reverse).
        router
            .cache
            .nvme
            .set_data_router(std::sync::Arc::downgrade(&router.inner));
        router
    }

    /// Rehydrate a `DataRouter` from its inner Arc (merge-worker hook).
    pub(crate) fn from_inner(inner: std::sync::Arc<DataRouterInner>) -> Self {
        Self { inner }
    }

    pub fn set_crypto(&self, crypto: crate::crypto_compress::CryptoCompressState) {
        let _ = self.crypto.set(crypto.clone());
        let _ = self.cache.nvme.crypto.set(crypto);
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

        let decompressed = self.get_crypto().process_read_async(raw).await?;
        Ok(decompressed)
    }

    pub async fn get_cached_or_fetch_block(
        &self,
        block_key: &str,
    ) -> Result<crate::cache::pool::ReadBlockValue> {
        // Single-flight block fetch. Waiters must not hang if they miss the
        // completion broadcast (subscribe-after-send race under multi-thread
        // large sequential reads + prefetch). Always re-check caches and use a
        // bounded wait so FUSE cannot wedge permanently (also blocks .config).
        const WAIT_SLICE: Duration = Duration::from_millis(50);
        const MAX_WAIT: Duration = Duration::from_secs(60);
        let deadline = std::time::Instant::now() + MAX_WAIT;

        loop {
            if let Some(cached_block) = self.cache.read_lru.get(block_key) {
                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(crate::cache::pool::ReadBlockValue::Bytes(cached_block));
            }

            if let Some(cached_block) = self.cache.nvme.read_cached_block(block_key) {
                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                let bytes = bytes::Bytes::from(cached_block);
                // NO RAM re-promote: this NVMe→RAM copy would be a cache
                // publish under a possibly-reused key, and the incarnation
                // word cannot prove ENTRY provenance — a tier entry from a
                // key's dying incarnation (undo/purge still in flight, see
                // the fill below) would validate against the NEW owner's
                // stable word and stick its bytes in the RAM LRU until
                // remount (all-zero block reads; surfaced by PR 6 routing
                // aligned striped writes through the no-LRU-put
                // write-through path). The RAM LRU is filled only by
                // device-validated fills (below) and legitimate owners;
                // NVMe-tier hits stay NVMe-tier hits — the bytes still
                // serve this caller.
                return Ok(crate::cache::pool::ReadBlockValue::Bytes(bytes));
            }

            if std::time::Instant::now() >= deadline {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("Timed out fetching block {}", block_key),
                )));
            }

            if let Some(entry) = self.inflight_block_reads.get_sync(block_key) {
                let tx = entry.get().clone();
                drop(entry);
                let mut rx = tx.subscribe();
                // Completion may have raced between get_sync and subscribe — recheck.
                if let Some(cached_block) = self.cache.read_lru.get(block_key) {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(crate::cache::pool::ReadBlockValue::Bytes(cached_block));
                }
                if self.inflight_block_reads.get_sync(block_key).is_none() {
                    // Primary finished; loop to re-read caches.
                    continue;
                }
                match tokio::time::timeout(WAIT_SLICE, rx.recv()).await {
                    Ok(Ok(())) | Ok(Err(_)) | Err(_) => {
                        // Woken, lagged, closed, or slice timeout — recheck caches.
                        continue;
                    }
                }
            }

            // Try to become the primary fetcher.
            let (tx, _rx) = tokio::sync::broadcast::channel(64);
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

                    // Validated fill (block-key incarnation seqlock): block keys
                    // are offset strings, so a freed+reallocated offset reuses
                    // the SAME key string. A fill that raced an owner's
                    // COW-write/free can hold pre-write or hole-punched bytes
                    // (zeros); publishing them poisons the shared caches for the
                    // key's next owner until remount. Snapshot the incarnation
                    // before the device read and publish only if it is stable
                    // and unchanged after — otherwise hand the bytes to the
                    // caller uncached (transient, never sticky).
                    let incarnation = self.backend_router.fill_incarnation(block_key);
                    let downloaded = match self.fetch_block_from_remote(block_key).await {
                        Ok(b) => b,
                        Err(e) => {
                            // Guard drop still notifies waiters so they can retry/fail.
                            return Err(e);
                        }
                    };
                    let downloaded_bytes = downloaded;
                    let publishable = incarnation.filter(|&before| {
                        self.backend_router
                            .fill_incarnation_still(block_key, before)
                    });
                    if let Some(before) = publishable {
                        if downloaded_bytes.len() < 64 * 1024 {
                            let _ = self
                                .cache
                                .nvme
                                .cache_read_block(block_key, downloaded_bytes.clone());
                        } else {
                            let nvme_clone = self.cache.nvme.clone();
                            let backend_router = self.backend_router.clone();
                            let bk_clone = block_key.to_string();
                            let dl_clone = downloaded_bytes.clone();
                            tokio::task::spawn_blocking(move || {
                                // Detached publish: it can run arbitrarily
                                // late, past a free + reallocation of this
                                // key. Re-check before AND after the put —
                                // the residual exposure is then a put→check
                                // instruction window that a whole
                                // free→allocate→DMA→publish cycle cannot
                                // fit inside.
                                if !backend_router.fill_incarnation_still(&bk_clone, before) {
                                    return;
                                }
                                let _ = nvme_clone.cache_read_block(&bk_clone, dl_clone);
                                if !backend_router.fill_incarnation_still(&bk_clone, before) {
                                    nvme_clone.remove_cached_read_block(&bk_clone);
                                }
                            });
                        }
                        // Avoid flooding RAM LRU with full 4 MiB blocks under
                        // multi-GB sequential reads. Small blocks still cache.
                        if downloaded_bytes.len() <= 256 * 1024 {
                            self.cache.read_lru.put(block_key, downloaded_bytes.clone());
                        }
                        // Seqlock completion (publish-then-revalidate): the
                        // pre-publish check alone is check-then-act — this
                        // task can be preempted between it and the puts, and
                        // a put landing after a new owner's
                        // allocate→DMA→publish→purge sequence would stick
                        // the dead incarnation's bytes under the reused key
                        // (all-zero block reads until remount; surfaced by
                        // PR 6 routing aligned writes through the no-put
                        // write-through path). Undo on any movement: a
                        // poisoned entry is at worst transient — removed by
                        // the very task that published it — never sticky.
                        if !self
                            .backend_router
                            .fill_incarnation_still(block_key, before)
                        {
                            self.cache.read_lru.remove(block_key);
                            self.cache.nvme.remove_cached_read_block(block_key);
                        }
                    }
                    return Ok(crate::cache::pool::ReadBlockValue::Bytes(downloaded_bytes));
                }
                Err(_) => {
                    // Lost the race to insert — loop and wait on the winner.
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

        // Refill under the per-inode metadata lock so a stale backend snapshot
        // can never clobber a concurrent writer's fresh cache entry (see
        // INODE_META_LOCKS). Double-check after acquiring: a writer or racing
        // filler may have refreshed the entry while we waited.
        let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
        if let Some(entry) = self.metadata_cache.get(file_path) {
            if entry.cached_at.elapsed() < Duration::from_secs(1) {
                return Ok(entry.clone());
            }
        }
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
            layout_dirty: false,
        };
        self.metadata_cache.insert(file_path.to_string(), m.clone());
        Ok(m)
    }

    /// Persist layout/size if the hot cache marked it dirty (writeback path).
    ///
    /// Runs under the per-inode metadata lock: a concurrent staged-promotion
    /// commit mutates the same RAM entry + backend layout, and an unlocked
    /// persist could clobber the promoted mapping with a pre-promotion
    /// snapshot (stranding the staged data once its ring entry is released).
    pub async fn persist_dirty_layout_if_needed(
        &self,
        file_path: &str,
        fencing_token: u64,
    ) -> Result<()> {
        let ino = parse_inode_from_path(file_path);
        let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
        let Some(meta) = self.metadata_cache.get(file_path) else {
            return Ok(());
        };
        if !meta.layout_dirty {
            return Ok(());
        }
        let mut clean = meta.clone();
        clean.layout_dirty = false;
        clean.cached_at = std::time::Instant::now();
        self.save_metadata_to_backend(ino, &clean, fencing_token)
            .await?;
        self.metadata_cache.insert(file_path.to_string(), clean);
        Ok(())
    }

    /// Promote a resident staged file to a durable backend block and release
    /// its staging-ring entry + budget (merge-worker path, capacity pressure).
    ///
    /// Returns `Ok(true)` when the entry was promoted and released. Any
    /// identity/generation mismatch is a benign skip (`Ok(false)`): the entry
    /// either no longer exists or a racing re-stage/layout-transition now
    /// owns it. Ordering:
    ///
    /// 1. Block data I/O first (io_uring, no locks held).
    /// 2. Layout commit (backend + RAM cache together) under the per-inode
    ///    metadata lock, with staged identity + stage-generation re-checked
    ///    under that lock.
    /// 3. Ring entry + budget release only if the stage generation is still
    ///    the one we promoted (`remove_staged_if_generation`).
    pub(crate) async fn promote_staged_file(
        &self,
        file_path: &str,
        file_id: &str,
        fencing_token: u64,
    ) -> Result<bool> {
        let nvme = &self.cache.nvme;
        let Some(gen) = nvme.staged_generation(file_id) else {
            return Ok(false);
        };
        let Some(raw) = nvme.read_staged(file_id) else {
            // Counted but not resident (should not happen): reconcile so the
            // budget cannot leak.
            nvme.remove_staged_if_generation(file_id, gen);
            return Ok(false);
        };

        let processed = self
            .get_crypto()
            .process_write_async(bytes::Bytes::from(raw))
            .await?;
        let (_be_id, allocator, writer) = self.backend_router.get_active_backend()?;
        if processed.len() as u64 > allocator.chunk_size() {
            // Incompressible expansion past the block size: stays resident.
            return Ok(false);
        }
        let offset = allocator.allocate_block().await?;
        let block_key = offset.to_string();
        if let Err(e) = writer.write_block(offset, processed).await {
            let _ = allocator.free_block(offset).await;
            return Err(e);
        }
        allocator.publish_block(offset);

        let ino = parse_inode_from_path(file_path);
        let commit = async {
            let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
            // Authoritative meta: RAM cache first (post-write truth), then
            // backend. NOTE: `fetch_metadata` would retake this lock.
            let current = match self.metadata_cache.get(file_path) {
                Some(m) => Some(m),
                None => self.fetch_metadata_from_backend(ino).await?,
            };
            let Some(current) = current else {
                return Ok::<bool, SqueezefsError>(false);
            };
            if current.file_type != "staged"
                || current.file_id.as_deref() != Some(file_id)
                || nvme.staged_generation(file_id) != Some(gen)
            {
                return Ok(false);
            }
            let mut updated = current.clone();
            let mut block_map = updated.block_map.take().unwrap_or_default();
            let displaced = block_map.insert(0, block_key.clone());
            updated.block_map = Some(block_map);
            updated.layout_dirty = false;
            updated.cached_at = std::time::Instant::now();
            self.save_metadata_to_backend(ino, &updated, fencing_token)
                .await?;
            self.metadata_cache.insert(file_path.to_string(), updated);
            if let Some(prev) = displaced {
                if prev != block_key {
                    // Re-promotion over an older durable copy: purge + free it.
                    self.cache.read_lru.remove(&prev);
                    self.cache.nvme.remove_cached_read_block(&prev);
                    let _ = self.backend_router.free_block(&prev).await;
                }
            }
            Ok(true)
        }
        .await;

        match commit {
            Ok(true) => {
                nvme.remove_staged_if_generation(file_id, gen);
                Ok(true)
            }
            Ok(false) => {
                let _ = allocator.free_block(offset).await;
                Ok(false)
            }
            Err(e) => {
                let _ = allocator.free_block(offset).await;
                Err(e)
            }
        }
    }

    /// Resolve the durable mapping (`block_map[0]`) of a staged file whose
    /// ring entry is gone. The caller's `meta` snapshot can predate a
    /// concurrent promotion/spill commit, so fall back to the freshest cached
    /// entry and finally the authoritative backend before declaring the
    /// payload unreachable.
    async fn staged_block_mapping(&self, file_path: &str, meta: &CachedMetadata) -> Option<String> {
        let map0 = |m: &CachedMetadata| m.block_map.as_ref().and_then(|bm| bm.get(&0).cloned());
        if let Some(mapping) = map0(meta) {
            return Some(mapping);
        }
        if let Some(mapping) = self.metadata_cache.get(file_path).as_ref().and_then(map0) {
            return Some(mapping);
        }
        let ino = parse_inode_from_path(file_path);
        self.fetch_metadata_from_backend(ino)
            .await
            .ok()
            .flatten()
            .as_ref()
            .and_then(map0)
    }

    /// Release the artifacts of a superseded staged layout *after* the new
    /// layout is published (backend as applicable + RAM cache): the staging
    /// ring entry (returns its budget) and any promoted/spilled durable
    /// copies the new layout no longer references. Callers hold the
    /// per-inode metadata lock; `old_maps` are the pre-publish snapshots
    /// (write-entry meta + last cached meta) so a promotion that landed
    /// between them cannot leak its block.
    async fn release_superseded_staged(
        &self,
        old_ring_id: Option<&str>,
        old_maps: [Option<&std::collections::HashMap<u32, String>>; 2],
        keep_block_key: Option<&str>,
    ) {
        if let Some(fid) = old_ring_id {
            self.cache.nvme.remove_staged(fid);
        }
        let mut freed = std::collections::HashSet::new();
        for map in old_maps.into_iter().flatten() {
            for bk in map.values() {
                if keep_block_key == Some(bk.as_str()) || !freed.insert(bk.clone()) {
                    continue;
                }
                self.cache.read_lru.remove(bk);
                self.cache.nvme.remove_cached_read_block(bk);
                let _ = self.backend_router.free_block(bk).await;
            }
        }
    }

    /// The ONLY way to mutate a striped block map (zero-copy write-path
    /// design §5.3 "One merge discipline"). Serializes under
    /// `INODE_META_LOCKS.get_inode_lock(ino)`; fetches the CURRENT meta
    /// (authoritative backend, falling back to the freshest RAM entry and
    /// finally a default for never-persisted layouts); applies `op`; bumps
    /// size to at least `min_size` (or truncates to `new_size` exactly for
    /// [`BlockMapOp::TruncateFrom`]); applies `layout_flip`; saves with
    /// fencing revalidation (which also republishes the RAM cache entry
    /// coherently); returns the keys actually displaced/removed from the
    /// current map — the caller frees them AFTER this returns (never a
    /// start-of-call snapshot key) — having already purged them from every
    /// RAM/NVMe read tier.
    ///
    /// Lock order: callers may hold `active_inode_locks` (1) and/or
    /// `BLOCK_FLUSH_LOCKS` (3); this primitive MUST NOT acquire either —
    /// `INODE_META_LOCKS` sits strictly after them (P1-9 extended order,
    /// see `stripe_locks.rs`). NOTE: never call `fetch_metadata` from under
    /// this lock — it retakes it on refill and self-deadlocks.
    pub async fn merge_block_mappings(
        &self,
        ino: u64,
        op: BlockMapOp<'_>,
        min_size: u64,
        layout_flip: LayoutFlip,
        fencing_token: u64,
    ) -> Result<Vec<String>> {
        let file_path = crate::keys::inode_path(ino);
        let _map_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;

        let mut current = match self.fetch_metadata_from_backend(ino).await? {
            Some(m) => m,
            // Never-persisted layout: the freshest RAM entry (post-write
            // truth for dirty layouts) beats an empty default.
            None => self.metadata_cache.get(&file_path).unwrap_or_default(),
        };

        let mut block_map = current.block_map.take().unwrap_or_default();
        let mut displaced: Vec<String> = Vec::new();
        let purge = |bk: &str| {
            // Purge every cache tier for a displaced/removed key: its offset
            // will be reallocated under the SAME key string once freed, and
            // a stale tier hit would serve the dead incarnation's bytes.
            self.cache.read_lru.remove(bk);
            self.cache.nvme.remove_cached_read_block(bk);
        };
        match op {
            BlockMapOp::Merge(entries) => {
                for (b, new_key) in entries {
                    if let Some(prev) = block_map.insert(*b, new_key.clone()) {
                        if prev != *new_key {
                            purge(&prev);
                            displaced.push(prev);
                        }
                    }
                }
                // Size floor: never below the caller's bound nor the freshest
                // RAM size (writes publish size to the RAM cache ahead of the
                // deferred layout commit — a merge must not regress it).
                current.size = std::cmp::max(current.size, min_size);
                if let Some(cached) = self.metadata_cache.get(&file_path) {
                    if cached.size > current.size {
                        current.size = cached.size;
                    }
                }
            }
            BlockMapOp::TruncateFrom { new_size } => {
                let block_size = self.block_size.load(Ordering::Relaxed);
                block_map.retain(|&b, bk| {
                    if (b as u64) * block_size >= new_size {
                        purge(bk);
                        displaced.push(bk.clone());
                        false
                    } else {
                        true
                    }
                });
                current.size = new_size;
            }
        }
        current.block_map = Some(block_map);

        match layout_flip {
            LayoutFlip::ToStripedKeepStagedIdentity => {
                current.file_type = "striped".to_string();
            }
            LayoutFlip::ToStripedClearStagedIdentity => {
                current.file_type = "striped".to_string();
                current.file_id = None;
                current.data_key = None;
            }
            LayoutFlip::KeepLayout => {}
        }

        // Fencing revalidation happens inside; the save also republishes the
        // RAM metadata_cache entry, keeping RAM + backend coherent under the
        // same guard.
        self.save_metadata_to_backend(ino, &current, fencing_token)
            .await?;
        Ok(displaced)
    }

    pub async fn update_metadata_cache_size(&self, file_path: &str, size: u64) {
        if let Some(mut entry) = self.metadata_cache.get(file_path) {
            if size > entry.size {
                entry.size = size;
                entry.cached_at = std::time::Instant::now();
                self.metadata_cache.insert(file_path.to_string(), entry);
            }
        } else {
            if let Ok(mut entry) = self.fetch_metadata(file_path).await {
                if size > entry.size {
                    entry.size = size;
                    entry.cached_at = std::time::Instant::now();
                    self.metadata_cache.insert(file_path.to_string(), entry);
                }
            }
        }
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
        // Prefetch is best-effort only. Under multi-thread large sequential reads
        // it previously stampeded get_cached_or_fetch_block and could wedge the
        // daemon; require free admission permits before even scheduling.
        if crate::bg_admit::BG_TASK_SEM.available_permits() < 4 {
            return false;
        }

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

        let current_fencing = self.dlm.get_fencing_token_ino(ino);
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

        let end_offset = (offset as usize) + data.len();
        let _staged_block_guard = if meta.file_type == "staged"
            || (meta.file_type == "inline"
                && end_offset > crate::fuse_client::MAX_INLINE_SIZE as usize)
        {
            let block_lock = crate::fuse_client::BLOCK_FLUSH_LOCKS.get_lock(ino, 0);
            Some(block_lock.lock().await)
        } else {
            None
        };

        let stripe_threshold = if self.cache.nvme.staging_dirs().is_empty() {
            MAX_INLINE_SIZE
        } else {
            self.block_size.load(Ordering::Acquire) as usize
        };

        // Full overwrite of empty / new file (common small-file create+write path):
        // skip loading prior payload and avoid an extra Vec assemble when offset==0.
        let full_overwrite_empty = offset == 0
            && meta.size == 0
            && meta.data_key.as_ref().map(|d| d.is_empty()).unwrap_or(true)
            && meta.file_id.is_none()
            && meta
                .block_map
                .as_ref()
                .map(|m| m.is_empty())
                .unwrap_or(true);

        let mut existing_data = if full_overwrite_empty {
            Vec::new()
        } else if let Some(cached) = self.cache.write_lru.get(file_path) {
            cached.to_vec()
        } else if let Some(cached) = self.cache.read_lru.get(file_path) {
            cached.to_vec()
        } else {
            match meta.file_type.as_str() {
                "inline" => {
                    if let Some(ref d) = meta.data_key {
                        d.to_vec()
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
                                let (offset_u64, off, sz) =
                                    self.parse_block_mapping(&mapping_str)?;
                                let packed_bytes =
                                    self.nvme_writer.read_block(offset_u64 + off, sz).await?;
                                self.get_crypto()
                                    .process_read_async(packed_bytes)
                                    .await?
                                    .to_vec()
                            } else {
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            }
        };

        // Patch / assemble payload
        let (payload_bytes, new_size) = if full_overwrite_empty && offset == 0 {
            let new_size = data.len();
            (data.clone(), new_size)
        } else {
            if existing_data.len() < end_offset {
                existing_data.resize(end_offset, 0);
            }
            existing_data[offset as usize..end_offset].copy_from_slice(&data);
            let new_size = existing_data.len();
            (bytes::Bytes::from(existing_data), new_size)
        };

        if end_offset > stripe_threshold || new_size > stripe_threshold {
            // Transition layout → striped. Block data I/O runs unlocked; only
            // the layout commit is serialized against concurrent staged
            // promotion (see INODE_META_LOCKS).
            let (block_mappings, _sizes, _block_count) =
                self.durable_write_stripe_payload(payload_bytes).await?;

            let mut block_map = std::collections::HashMap::new();
            for (idx_str, key) in block_mappings {
                if let Ok(idx) = idx_str.parse::<u32>() {
                    block_map.insert(idx, key);
                }
            }

            {
                let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
                let fresh = self.metadata_cache.get(file_path);
                let mut updated_meta = meta.clone();
                updated_meta.file_type = "striped".to_string();
                updated_meta.size = new_size as u64;
                updated_meta.block_map = Some(block_map);
                updated_meta.file_id = None;
                self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                    .await?;

                self.cache.write_lru.remove(file_path);
                self.cache.read_lru.remove(file_path);

                self.metadata_cache
                    .insert(file_path.to_string(), updated_meta);
                // The staged form is superseded: release its ring entry
                // (budget) and any promoted/spilled durable copy.
                self.release_superseded_staged(
                    meta.file_id.as_deref(),
                    [
                        meta.block_map.as_ref(),
                        fresh.as_ref().and_then(|f| f.block_map.as_ref()),
                    ],
                    None,
                )
                .await;
            }
            crate::fuse_client::METRICS
                .layout_striped_writes
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        if new_size <= MAX_INLINE_SIZE {
            // Layout: inline — RAM only until fsync/release (writeback).
            crate::fuse_client::METRICS
                .layout_inline_writes
                .fetch_add(1, Ordering::Relaxed);
            let shared_data = payload_bytes;

            let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
            let fresh = self.metadata_cache.get(file_path);
            let mut updated_meta = meta.clone();
            updated_meta.file_type = "inline".to_string();
            updated_meta.size = new_size as u64;
            // Zero-copy store: `shared_data` is `Bytes`; clone is a refcount bump,
            // not a payload copy (was `shared_data.to_vec()` = full memcpy per write).
            updated_meta.data_key = Some(shared_data.clone());
            updated_meta.file_id = None;
            updated_meta.block_map = None;
            updated_meta.layout_dirty = true;
            updated_meta.cached_at = std::time::Instant::now();

            self.cache.write_lru.put(file_path, shared_data.clone());
            self.cache.read_lru.put(file_path, shared_data);
            self.metadata_cache
                .insert(file_path.to_string(), updated_meta);
            // A truncated-then-rewritten staged/spilled file leaves a ring
            // entry and/or a durable copy behind: release them.
            self.release_superseded_staged(
                meta.file_id.as_deref(),
                [
                    meta.block_map.as_ref(),
                    fresh.as_ref().and_then(|f| f.block_map.as_ref()),
                ],
                None,
            )
            .await;
        } else if !self.cache.nvme.staging_dirs().is_empty()
            && (new_size as u64) <= self.block_size.load(Ordering::Acquire)
        {
            // Layout: staged — mmap stage + RAM meta; MetaLV layout deferred to fsync.
            crate::fuse_client::METRICS
                .layout_staged_writes
                .fetch_add(1, Ordering::Relaxed);
            let new_file_id = meta
                .file_id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());

            let stage_res = self
                .cache
                .nvme
                .stage_write(file_path, &new_file_id, &payload_bytes, fencing_token)
                .await;

            let shared_data = payload_bytes;

            match stage_res {
                Ok(_) => {
                    let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
                    let fresh = self.metadata_cache.get(file_path);
                    let mut updated_meta = meta.clone();
                    updated_meta.file_type = "staged".to_string();
                    updated_meta.size = new_size as u64;
                    updated_meta.file_id = Some(new_file_id);
                    updated_meta.data_key = None;
                    updated_meta.block_map = None;
                    updated_meta.layout_dirty = true;
                    updated_meta.cached_at = std::time::Instant::now();
                    self.metadata_cache
                        .insert(file_path.to_string(), updated_meta);
                    // The fresh stage supersedes any promoted/spilled durable
                    // copy of older content. The ring entry itself is the
                    // fresh data (replaced in-place by stage_write) — keep it.
                    self.release_superseded_staged(
                        None,
                        [
                            meta.block_map.as_ref(),
                            fresh.as_ref().and_then(|f| f.block_map.as_ref()),
                        ],
                        None,
                    )
                    .await;
                }
                Err(SqueezefsError::Io(ref e)) if e.kind() == std::io::ErrorKind::StorageFull => {
                    // Spill is the designed degraded mode under sustained
                    // pressure and can fire thousands of times in a burst:
                    // one line per second + a suppressed count keeps the
                    // signal without drowning the log.
                    {
                        static LAST_SPILL_WARN: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        static SUPPRESSED: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let last = LAST_SPILL_WARN.load(Ordering::Relaxed);
                        if now != last
                            && LAST_SPILL_WARN
                                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                                .is_ok()
                        {
                            let suppressed = SUPPRESSED.swap(0, Ordering::Relaxed);
                            log::warn!(
                                "NVMe write staging cache full: direct synchronous backend block write for {} ({} similar spills suppressed)",
                                file_path, suppressed
                            );
                        } else {
                            SUPPRESSED.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let processed_data = self
                        .get_crypto()
                        .process_write_async(shared_data.clone())
                        .await?;

                    let (_be_id, block_allocator, nvme_writer) =
                        self.backend_router.get_active_backend()?;
                    let be_offset = block_allocator.allocate_block().await?;
                    let stored_block_key = be_offset.to_string();

                    nvme_writer.write_block(be_offset, processed_data).await?;
                    block_allocator.publish_block(be_offset);

                    let mut block_map = std::collections::HashMap::new();
                    block_map.insert(0, stored_block_key.clone());

                    // Spill takes a *fresh* file_id: the stale ring entry
                    // under the old id must never shadow this newer durable
                    // payload on reads, and an in-flight promotion of the old
                    // id must never pass its identity check and clobber this
                    // layout with pre-spill content.
                    let spill_file_id = Uuid::new_v4().to_string();

                    let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
                    let fresh = self.metadata_cache.get(file_path);
                    let mut updated_meta = meta.clone();
                    updated_meta.file_type = "staged".to_string();
                    updated_meta.size = new_size as u64;
                    updated_meta.file_id = Some(spill_file_id);
                    updated_meta.data_key = None;
                    updated_meta.block_map = Some(block_map);
                    // Durable backend write already happened — commit layout now.
                    updated_meta.layout_dirty = false;
                    self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                        .await?;
                    self.metadata_cache
                        .insert(file_path.to_string(), updated_meta);
                    // Release the superseded stale ring entry (returns its
                    // budget) and any older durable copy it had.
                    self.release_superseded_staged(
                        meta.file_id.as_deref(),
                        [
                            meta.block_map.as_ref(),
                            fresh.as_ref().and_then(|f| f.block_map.as_ref()),
                        ],
                        Some(&stored_block_key),
                    )
                    .await;
                }
                Err(e) => return Err(e),
            }

            self.cache.write_lru.put(file_path, shared_data.clone());
            self.cache.read_lru.put(file_path, shared_data);
        } else {
            // First-time striped layout (no staging dirs / above staged threshold).
            let (block_mappings, _sizes, _block_count) =
                self.durable_write_stripe_payload(payload_bytes).await?;

            let mut block_map = std::collections::HashMap::new();
            for (idx_str, key) in block_mappings {
                if let Ok(idx) = idx_str.parse::<u32>() {
                    block_map.insert(idx, key);
                }
            }

            {
                // First-time striped commit under INODE_META_LOCKS, for
                // uniformity with its transition siblings above (§5.3
                // census): without the guard, two concurrent first-writers
                // on a brand-new file could interleave their create-time
                // saves (the conditional block-0 guard at the top of
                // `write_file` does not cover a default/empty `file_type`).
                let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
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
            }
            crate::fuse_client::METRICS
                .layout_striped_writes
                .fetch_add(1, Ordering::Relaxed);
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
            let processed = match self.get_crypto().process_write_async(chunk.clone()).await {
                Ok(p) => p,
                Err(e) => {
                    for k in &allocated_keys {
                        let _ = self.backend_router.free_block(k).await;
                    }
                    return Err(e);
                }
            };
            let processed_len = processed.len();

            if let Err(e) = nvme_writer.write_block(offset, processed).await {
                for k in &allocated_keys {
                    let _ = self.backend_router.free_block(k).await;
                }
                return Err(e);
            }

            // Cache plaintext block for subsequent reads (key = block key, not file path).
            self.cache.read_lru.put(&stored_block_key, chunk);
            block_allocator.publish_block(offset);

            block_mappings.push((block_count.to_string(), stored_block_key.clone()));
            sizes_to_register.push((stored_block_key, chunk_len, processed_len));
            offset_cursor = end;
            block_count += 1;
        }

        Ok((block_mappings, sizes_to_register, block_count))
    }

    /// Register a completed stripe layout in MetaLV after durable block writes.
    /// Block-map, refcounts, and sizes are updated atomically using the WAL-redo transaction scope.

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
                        // NVMe hit consumed directly — no RAM re-promote
                        // (unprovable entry provenance under key reuse; see
                        // get_cached_or_fetch_block).
                        resolved = Some(crate::cache::pool::ReadBlockValue::Bytes(
                            bytes::Bytes::from(cached_block),
                        ));
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

            // §5.6 (PR 6) full-coverage slice reuse (kills audit #12 here):
            // when the overlap covers the whole block there is no existing
            // data to RMW-seed (`needs_existing` is provably false) — the
            // payload slice IS the block, so it flows to crypto/DMA and into
            // the read LRU directly instead of being copied into a
            // `PooledBuf` first. Lease-safe by construction: the §5.4
            // severance boundary guarantees no transport lease ever reaches
            // `DataRouter::write_file`, so retaining `data_slice` retains a
            // private copy. The RMW-seed copy below (partial coverage,
            // audit #13) is untouched by design.
            let full_coverage = rel_start == 0 && rel_end == block_size as usize;

            let sem_clone = sem.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = sem_clone.acquire().await.map_err(|e| {
                    SqueezefsError::Io(std::io::Error::other(format!(
                        "Semaphore acquire error: {:?}",
                        e
                    )))
                })?;

                let block_bytes = if full_coverage {
                    data_slice
                } else {
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

                    block_data.into_bytes()
                };

                let (_be_id, block_allocator, nvme_writer) =
                    router_clone.backend_router.get_active_backend()?;
                let offset = block_allocator.allocate_block().await?;
                let stored_new_block_key = offset.to_string();

                let processed_block = crypto.process_write_async(block_bytes.clone()).await?;
                nvme_writer.write_block(offset, processed_block).await?;

                // Cache + publish only after the device write: a racing
                // validated fill for this key must either see the durable bytes
                // or fail its incarnation check — never observe (and cache) the
                // pre-write contents of a reused offset.
                read_lru.put(&stored_new_block_key, block_bytes);
                block_allocator.publish_block(offset);

                Ok::<_, SqueezefsError>((b, stored_new_block_key))
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
            for (_b, new_key) in &results {
                let _ = self.backend_router.free_block(new_key).await;
            }
            return Err(e);
        }

        // Atomic per-inode layout merge through the shared primitive (§5.3
        // one merge discipline): read→merge→save serialized under
        // INODE_META_LOCKS against every other striped-map writer, merging
        // into the *current* map — never into our start-of-call snapshot,
        // which would drop concurrent writers' entries and revert blocks to
        // freed keys. The block data I/O above ran concurrently (COW to
        // fresh keys); displaced-from-current keys are freed only after the
        // new map is published (durable + cached), so no reader can resolve
        // a block to a key we are freeing.
        let displaced_keys = self
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&results),
                end_pos,
                LayoutFlip::ToStripedClearStagedIdentity,
                _fencing_token,
            )
            .await?;
        for bk in displaced_keys {
            let _ = self.backend_router.free_block(&bk).await;
        }

        // Drop any whole-file RAM snapshot: patching a shared whole-file buffer
        // under concurrent writers is itself a lost-update hazard. Reads
        // re-resolve through the now-consistent block map.
        self.cache.write_lru.remove(file_path);
        self.cache.read_lru.remove(file_path);
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
                    d.to_vec()
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
                    let mapping_opt = self.staged_block_mapping(file_path, &meta).await;
                    if let Some(mapping_str) = mapping_opt {
                        let (offset_u64, off, sz) = self.parse_block_mapping(&mapping_str)?;
                        let packed_bytes =
                            self.nvme_writer.read_block(offset_u64 + off, sz).await?;
                        self.get_crypto()
                            .process_read_async(packed_bytes)
                            .await?
                            .to_vec()
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
                    data.to_vec()
                } else {
                    Vec::new()
                };
                let start = std::cmp::min(offset as usize, decompressed.len());
                let end = std::cmp::min((offset + size as u64) as usize, decompressed.len());
                Ok(decompressed[start..end].to_vec())
            }
            "staged" => {
                let file_id = meta.file_id.as_deref().ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Missing file_id for staged file".to_string())
                })?;

                if let Some(staged_data) = self.cache.nvme.read_staged(file_id) {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    let start = std::cmp::min(offset as usize, staged_data.len());
                    let end = std::cmp::min((offset + size as u64) as usize, staged_data.len());
                    Ok(staged_data[start..end].to_vec())
                } else {
                    let mapping_opt = self.staged_block_mapping(file_path, &meta).await;
                    if let Some(mapping_str) = mapping_opt {
                        let (offset_u64, off, sz) = self.parse_block_mapping(&mapping_str)?;
                        let packed_bytes =
                            self.nvme_writer.read_block(offset_u64 + off, sz).await?;
                        let decompressed =
                            self.get_crypto().process_read_async(packed_bytes).await?;
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
                    bytes::Bytes::from_owner(crate::cache::pool::UringBufOwner {
                        ptr: dest_ptr,
                        len,
                    })
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
                    data.to_vec()
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
                            let d = bytes::Bytes::from_owner(crate::cache::pool::UringBufOwner {
                                ptr: dest_ptr,
                                len,
                            });
                            (d, None)
                        }
                    } else {
                        let mut sliced_guard = guard;
                        sliced_guard.offset += start;
                        sliced_guard.len = len;
                        let d = bytes::Bytes::copy_from_slice(&sliced_guard);
                        (d, None)
                    };
                    Ok((data, backing))
                } else {
                    let mapping_opt = self.staged_block_mapping(file_path, &meta).await;
                    if let Some(bk) = mapping_opt {
                        let offset_u64 = self.backend_router.parse_block_offset(&bk)?;
                        let sz = self.block_size.load(Ordering::Acquire) as usize;
                        let packed_bytes = self.nvme_writer.read_block(offset_u64, sz).await?;
                        let decompressed =
                            self.get_crypto().process_read_async(packed_bytes).await?;
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
                        let (data, backing) =
                            if let Some(dest) = dest_addr {
                                let dest_ptr = dest as *mut u8;
                                unsafe {
                                    std::ptr::copy_nonoverlapping(
                                        guard[start..end].as_ptr(),
                                        dest_ptr,
                                        len,
                                    );
                                    let d = bytes::Bytes::from_owner(
                                        crate::cache::pool::UringBufOwner { ptr: dest_ptr, len },
                                    );
                                    (d, None)
                                }
                            } else {
                                let mut sliced_guard = guard;
                                sliced_guard.offset += start;
                                sliced_guard.len = len;
                                let d = bytes::Bytes::copy_from_slice(&sliced_guard);
                                (d, None)
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
                                        bytes::Bytes::from_owner(
                                            crate::cache::pool::UringBufOwner {
                                                ptr: dest_ptr,
                                                len,
                                            },
                                        )
                                    }
                                } else {
                                    bytes::Bytes::copy_from_slice(&guard)
                                };
                                return Ok((data, None));
                            } else {
                                // Single block cache miss: download directly in-line (zero-copy, no spawn)
                                let downloaded = if let Some(dest) = dest_addr {
                                    if slice_start == 0 && slice_len as u64 == block_size {
                                        self.backend_router
                                            .read_block_with_dest(
                                                b_key,
                                                block_size as usize,
                                                Some(dest),
                                            )
                                            .await?;
                                        let len = block_size as usize;
                                        let dest_ptr = dest as *mut u8;
                                        let b = bytes::Bytes::from_owner(
                                            crate::cache::pool::UringBufOwner {
                                                ptr: dest_ptr,
                                                len,
                                            },
                                        );
                                        crate::cache::pool::ReadBlockValue::Bytes(b)
                                    } else {
                                        let val = self.get_cached_or_fetch_block(b_key).await?;
                                        let start = std::cmp::min(slice_start as usize, val.len());
                                        let end = std::cmp::min(
                                            (slice_start + slice_len as u64) as usize,
                                            val.len(),
                                        );
                                        let len = end - start;
                                        let dest_ptr = dest as *mut u8;
                                        unsafe {
                                            std::ptr::copy_nonoverlapping(
                                                val[start..end].as_ptr(),
                                                dest_ptr,
                                                len,
                                            );
                                        }
                                        let b = bytes::Bytes::from_owner(
                                            crate::cache::pool::UringBufOwner {
                                                ptr: dest_ptr,
                                                len,
                                            },
                                        );
                                        crate::cache::pool::ReadBlockValue::Bytes(b)
                                    }
                                } else {
                                    self.get_cached_or_fetch_block(b_key).await?
                                };

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

                                if let Some(dest) = dest_addr {
                                    let len = (end_offset - offset) as usize;
                                    let data = bytes::Bytes::from_owner(
                                        crate::cache::pool::UringBufOwner {
                                            ptr: dest as *mut u8,
                                            len,
                                        },
                                    );
                                    return Ok((data, Some(std::sync::Arc::new(downloaded))));
                                } else {
                                    let start =
                                        std::cmp::min(slice_start as usize, downloaded.len());
                                    let end = std::cmp::min(
                                        (slice_start + slice_len as u64) as usize,
                                        downloaded.len(),
                                    );
                                    let slice: &[u8] = &downloaded[start..end];
                                    let data = bytes::Bytes::copy_from_slice(slice);
                                    return Ok((data, Some(std::sync::Arc::new(downloaded))));
                                }
                            }
                        } else {
                            // Hole support: return zero-filled slice
                            let len = slice_len as usize;
                            let data = if let Some(dest) = dest_addr {
                                let dest_ptr = dest as *mut u8;
                                unsafe {
                                    std::ptr::write_bytes(dest_ptr, 0, len);
                                    bytes::Bytes::from_owner(crate::cache::pool::UringBufOwner {
                                        ptr: dest_ptr,
                                        len,
                                    })
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

                // Spawn concurrent tasks to download block data in parallel.
                // Acquire the admission permit *inside* each task so the coordinator
                // never holds N permits while spawning (can deadlock the semaphore
                // when block_count > permit pool under nested multi-block reads).
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
                    let sem = crate::bg_admit::STRIPED_IO_SEM.clone();

                    futures.push(tokio::spawn(async move {
                        let _permit = sem.acquire_owned().await.map_err(|_| {
                            SqueezefsError::InvalidOperation(
                                "striped read admission closed".to_string(),
                            )
                        })?;
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
                    let data = bytes::Bytes::from_owner(crate::cache::pool::UringBufOwner {
                        ptr: dest_addr.unwrap() as *mut u8,
                        len: final_len,
                    });
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
    pub async fn clone_file(
        &self,
        src: &str,
        dest: &str,
        src_token: Option<u64>,
        dest_token: Option<u64>,
    ) -> Result<()> {
        let _src_ino = parse_inode_from_path(src);
        let dest_ino = parse_inode_from_path(dest);

        let _src_lease;
        let _resolved_src_token = if let Some(t) = src_token {
            t
        } else {
            _src_lease = self
                .dlm
                .acquire_lock(src, None, std::time::Duration::from_secs(5))
                .await?;
            _src_lease.fencing_token()
        };

        let _dest_lease;
        let resolved_dest_token = if let Some(t) = dest_token {
            t
        } else {
            _dest_lease = self
                .dlm
                .acquire_lock(dest, None, std::time::Duration::from_secs(5))
                .await?;
            _dest_lease.fencing_token()
        };

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
                    .stage_write(dest, &new_file_id, &data, resolved_dest_token)
                    .await?;
            }
            updated_meta.file_id = Some(new_file_id);
        } else if meta.file_type == "striped" {
            // All-or-nothing pin of every source block. A refusal means the
            // snapshot map is stale (a block was freed/displaced since the
            // fetch): undo partial pins, re-read the authoritative map under
            // the per-inode metadata lock, and retry. Never proceed with an
            // unpinned block — the clone would alias a reallocatable offset
            // that reads as foreign bytes after reuse.
            let src_ino = parse_inode_from_path(src);
            let mut current = meta.clone();
            let mut attempt = 0usize;
            loop {
                let map = current.block_map.clone().unwrap_or_default();
                let mut pinned: Vec<&String> = Vec::with_capacity(map.len());
                let mut refused = None;
                for bk in map.values() {
                    if self.backend_router.increment_refcount(bk) {
                        pinned.push(bk);
                    } else {
                        refused = Some(bk.clone());
                        break;
                    }
                }
                match refused {
                    None => {
                        updated_meta = current;
                        break;
                    }
                    Some(bad) => {
                        // Undo the partial pins (free_block = one decrement).
                        for bk in pinned {
                            let _ = self.backend_router.free_block(bk).await;
                        }
                        attempt += 1;
                        if attempt >= 3 {
                            return Err(SqueezefsError::InvalidOperation(format!(
                                "clone source {src} block {bad} freed concurrently \
                                 (map still stale after {attempt} attempts); aborting \
                                 to avoid an unpinned clone"
                            )));
                        }
                        let _meta_guard = INODE_META_LOCKS.get_inode_lock(src_ino).lock().await;
                        match self.fetch_metadata_from_backend(src_ino).await? {
                            Some(fresh) if fresh.file_type == "striped" => current = fresh,
                            _ => {
                                return Err(SqueezefsError::InvalidOperation(format!(
                                    "clone source {src} changed layout mid-clone; retry the clone"
                                )))
                            }
                        }
                    }
                }
            }
        }

        self.save_metadata_to_backend(dest_ino, &updated_meta, resolved_dest_token)
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

    /// Truncate a file's layout metadata, reclaiming blocks that fall beyond the new size.
    pub async fn truncate_layout(&self, ino: u64, new_size: u64, fencing_token: u64) -> Result<()> {
        let file_path = crate::keys::inode_path(ino);
        let mut meta = self.fetch_metadata(&file_path).await?;
        let old_size = meta.size;

        // Striped files: every save that carries an existing striped block
        // map goes through the merge primitive (§5.3 one merge discipline).
        // Both legs were stale-snapshot saves racing the data-path merges:
        // grow rewrote the whole map "without mutating it" (a live
        // lost-update once write-through publishes continuously), shrink's
        // retain-and-save was serialized only by setattr's inode write lock.
        // Inline/staged files keep the whole-meta save below — their RAM
        // meta (dirty inline payload / staged identity) is the truth a
        // backend re-read cannot carry, and they have no striped map to
        // lose.
        if meta.file_type == "striped" {
            if new_size >= old_size {
                // Growing: degenerate size-only merge — the CURRENT map is
                // re-read and saved under INODE_META_LOCKS.
                self.merge_block_mappings(
                    ino,
                    BlockMapOp::Merge(&[]),
                    new_size,
                    LayoutFlip::KeepLayout,
                    fencing_token,
                )
                .await?;
                return Ok(());
            }
            // Shrinking: removal-RMW on the same primitive; removed keys
            // come back as the free list (post-publish free discipline).
            let removed = self
                .merge_block_mappings(
                    ino,
                    BlockMapOp::TruncateFrom { new_size },
                    new_size,
                    LayoutFlip::KeepLayout,
                    fencing_token,
                )
                .await?;
            let blocks_to_free: Vec<String> = removed
                .iter()
                .map(|bk| {
                    if let Some(pos) = bk.find("://") {
                        let proto = &bk[..pos];
                        let rest = &bk[pos + 3..];
                        let offset = rest.split(':').next().unwrap_or(rest);
                        format!("{}://{}", proto, offset)
                    } else {
                        bk.split(':').next().unwrap_or(bk).to_string()
                    }
                })
                .collect();
            if !blocks_to_free.is_empty() {
                let free_refs: Vec<&str> = blocks_to_free.iter().map(|s| s.as_str()).collect();
                let _ = self.backend_router.free_blocks(&free_refs).await;
            }
            return Ok(());
        }

        if new_size >= old_size {
            // Growing the file: update size
            meta.size = new_size;
            self.save_metadata_to_backend(ino, &meta, fencing_token)
                .await?;
            self.metadata_cache.insert(file_path, meta);
            return Ok(());
        }

        // Shrinking the file
        meta.size = new_size;

        let mut blocks_to_free = Vec::new();

        if let Some(ref mut block_map) = meta.block_map {
            let block_size = self.block_size.load(Ordering::Relaxed);
            block_map.retain(|&b, bk| {
                let block_start = b as u64 * block_size;
                if block_start >= new_size {
                    self.cache.read_lru.remove(bk);
                    let clean_bk = if let Some(pos) = bk.find("://") {
                        let proto = &bk[..pos];
                        let rest = &bk[pos + 3..];
                        let offset = rest.split(':').next().unwrap_or(rest);
                        format!("{}://{}", proto, offset)
                    } else {
                        bk.split(':').next().unwrap_or(bk).to_string()
                    };
                    blocks_to_free.push(clean_bk);
                    false // Remove from block_map
                } else {
                    true
                }
            });
        }

        if meta.file_type == "inline" {
            if let Some(ref mut data) = meta.data_key {
                data.truncate(new_size as usize);
            }
        } else if meta.file_type == "staged" {
            if let Some(ref file_id) = meta.file_id {
                if let Some(data) = self.cache.nvme.read_staged(file_id) {
                    let mut updated_data = data;
                    updated_data.truncate(new_size as usize);
                    let _ = self
                        .cache
                        .nvme
                        .stage_write(&file_path, file_id, &updated_data, fencing_token)
                        .await;
                }
            }
        }

        // Save updated metadata
        self.save_metadata_to_backend(ino, &meta, fencing_token)
            .await?;
        self.metadata_cache.insert(file_path, meta);

        // Free the shrunken blocks
        if !blocks_to_free.is_empty() {
            let free_refs: Vec<&str> = blocks_to_free.iter().map(|s| s.as_str()).collect();
            let _ = self.backend_router.free_blocks(&free_refs).await;
        }

        Ok(())
    }

    /// Safely delete all underlying storage files/blocks associated with the file.
    pub async fn delete_file(
        &self,
        file_path: &str,
        _con: &mut crate::dlm::MetaConnection,
    ) -> Result<()> {
        let meta = self.fetch_metadata(file_path).await?;

        let mut blocks_to_free: Vec<String> = Vec::new();

        if let Some(ref block_map) = meta.block_map {
            for bk in block_map.values() {
                self.cache.read_lru.remove(bk);
                let clean_bk = if let Some(pos) = bk.find("://") {
                    let proto = &bk[..pos];
                    let rest = &bk[pos + 3..];
                    let offset = rest.split(':').next().unwrap_or(rest);
                    format!("{}://{}", proto, offset)
                } else {
                    bk.split(':').next().unwrap_or(bk).to_string()
                };
                blocks_to_free.push(clean_bk);
            }
        }

        if meta.file_type == "staged" {
            if let Some(ref file_id) = meta.file_id {
                self.cache.nvme.remove_staged(file_id);
            }
        }

        if let Some(ref map_id) = meta.block_map_id {
            if map_id.starts_with("indirect:") {
                let block_key = map_id.strip_prefix("indirect:").unwrap();
                blocks_to_free.push(block_key.to_string());
            }
        }

        if !blocks_to_free.is_empty() {
            let free_refs: Vec<&str> = blocks_to_free.iter().map(|s| s.as_str()).collect();
            let _ = self.backend_router.free_blocks(&free_refs).await;
        }

        // No per-corpse `removexattr("layout")` transaction here: the sole
        // caller is inode reclaim, whose batched `destroy_inodes` kills the
        // whole xattr block inside its own commit (one transaction per batch
        // instead of one per corpse — the extra commit's sector guards
        // collided with foreground unlinks under delete storms).

        // Targeted O(1) active block removals without full staging listing
        let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
        let max_block = if block_size > 0 {
            (meta.size + block_size - 1) / block_size
        } else {
            0
        };
        let mut block_indices = std::collections::HashSet::new();
        for b in 0..=max_block {
            block_indices.insert(b);
        }
        if let Some(ref block_map) = meta.block_map {
            for &b in block_map.keys() {
                block_indices.insert(b as u64);
            }
        }
        for b in block_indices {
            let key = format!("active_block:{}:{}", file_path, b);
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
                .create(
                    parent_ino,
                    file_name,
                    src_inode.mode,
                    src_inode.uid,
                    src_inode.gid,
                )
                .await?;
            let _ = backend
                .setattr(
                    dest_inode.ino,
                    Some(src_inode.mode),
                    None,
                    None,
                    Some(src_inode.size),
                    None,
                    None,
                    None,
                )
                .await?;

            self.clone_file(
                crate::keys::inode_path(src_ino).as_str(),
                crate::keys::inode_path(dest_inode.ino).as_str(),
                None,
                None,
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
