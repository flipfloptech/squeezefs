use crate::error::{Result, SqueezefsError};
use crate::meta_backend::Metadata;
use bytes::Bytes;
use log::{error, info};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

use xxhash_rust::xxh3::xxh3_64;

pub fn dir_has_segment_data(path: &std::path::Path) -> bool {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        if metadata.is_file() && metadata.len() > 0 {
            return true;
        }
    }

    false
}

fn check_disk_free_safeguard(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Ok(c_path) = CString::new(abs_path.as_os_str().as_bytes()) {
            unsafe {
                let mut stat: libc::statvfs = std::mem::zeroed();
                if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 && stat.f_blocks > 0 {
                    let free_fraction = stat.f_bavail as f64 / stat.f_blocks as f64;
                    let free_bytes = stat.f_bavail as u64 * stat.f_frsize as u64;
                    if free_bytes < 100 * 1024 * 1024
                        || (free_fraction < 0.01 && free_bytes < 1024 * 1024 * 1024)
                    {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Binary header for staged / active-block payloads on local NVMe staging.
/// Shared with crash recovery (`recovery::recover_staging`) — must stay stable.
#[derive(Clone, Debug)]
pub struct StagedMetadata {
    pub fencing_token: u64,
    pub original_size: u64,
    pub file_path: String,
}

impl StagedMetadata {
    pub fn serialize(&self) -> Vec<u8> {
        let path_bytes = self.file_path.as_bytes();
        let mut buf = Vec::with_capacity(20 + path_bytes.len());
        buf.extend_from_slice(&self.fencing_token.to_be_bytes());
        buf.extend_from_slice(&self.original_size.to_be_bytes());
        buf.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
        buf.extend_from_slice(path_bytes);
        buf
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 20 {
            return None;
        }
        let fencing_token = u64::from_be_bytes(bytes[0..8].try_into().ok()?);
        let original_size = u64::from_be_bytes(bytes[8..16].try_into().ok()?);
        let path_len = u32::from_be_bytes(bytes[16..20].try_into().ok()?) as usize;
        if bytes.len() < 20 + path_len {
            return None;
        }
        let file_path = String::from_utf8(bytes[20..20 + path_len].to_vec()).ok()?;
        Some(Self {
            fencing_token,
            original_size,
            file_path,
        })
    }
}

/// Parse a packed staging blob: `[meta_len:u64][meta bytes][payload…]`.
/// Active blocks pad the header to 4 KiB; ordinary staged files do not.
///
/// Returns `None` on malformed / hostile lengths (no panics on overflow — P2-13).
pub fn parse_staged_blob(bytes: &[u8], is_active_block: bool) -> Option<(StagedMetadata, Vec<u8>)> {
    if bytes.len() < 8 {
        return None;
    }
    let meta_len = usize::try_from(u64::from_be_bytes(bytes[0..8].try_into().ok()?)).ok()?;
    let meta_end = 8usize.checked_add(meta_len)?;
    if bytes.len() < meta_end {
        return None;
    }
    let meta = StagedMetadata::deserialize(&bytes[8..meta_end])?;
    let data_start = if is_active_block { 4096usize } else { meta_end };
    let payload_len = usize::try_from(meta.original_size).ok()?;
    let data_end = data_start.checked_add(payload_len)?;
    if bytes.len() < data_end {
        return None;
    }
    Some((meta, bytes[data_start..data_end].to_vec()))
}

#[derive(Clone)]
pub struct NvmeStaging {
    staging_dirs: Vec<PathBuf>,
    max_write_bytes: u64,
    max_read_bytes: u64,
    pub block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
    pub nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    pub backend_router:
        std::sync::Arc<once_cell::sync::OnceCell<std::sync::Arc<crate::routing::BackendRouter>>>,
    redis_client: std::sync::Arc<crate::dlm::MetaClient>,
    pub meta_backend: std::sync::Arc<
        once_cell::sync::OnceCell<std::sync::Arc<crate::meta_backend::RoutedMetaBackend>>,
    >,
    /// Bounded merge-queue sender (P1-1). Full → StorageFull / backpressure.
    write_tx: mpsc::Sender<PendingStagedWrite>,
    pub p2p_addr: std::sync::Arc<std::sync::OnceLock<String>>,
    pub current_staged_write_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub space_freed_notify: std::sync::Arc<tokio::sync::Notify>,
    pub staged_writes_in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub staged_drained_notify: std::sync::Arc<tokio::sync::Notify>,

    // Hypertier NVMe cache instances
    pub read_nvme_cache: std::sync::Arc<crate::tiering::nvme::NvmeCache>,
    pub staging_nvme_cache: std::sync::Arc<crate::tiering::nvme::NvmeCache>,
    pub dht_node: std::sync::Arc<std::sync::OnceLock<std::sync::Arc<crate::tiering::dht::DhtNode>>>,
    pub crypto: std::sync::Arc<std::sync::OnceLock<crate::crypto_compress::CryptoCompressState>>,
}

#[derive(Debug, Clone)]
pub struct PendingStagedWrite {
    pub file_path: String,
    pub file_id: String,
    pub fencing_token: u64,
    pub padded_size: u64,
}

impl NvmeStaging {
    pub fn new(
        staging_dirs: Vec<PathBuf>,
        max_write_bytes: u64,
        max_read_bytes: u64,
        block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
        nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
        redis_client: std::sync::Arc<crate::dlm::MetaClient>,
    ) -> Result<Self> {
        // Initialize directories for segments
        let mut read_cache_dirs = Vec::new();
        let mut staging_segment_dirs = Vec::new();
        let mut read_cache_dirs_have_data = Vec::new();
        let mut staging_segment_dirs_have_data = Vec::new();
        for dir in &staging_dirs {
            let rc_dir = dir.join("cache_segment");
            let ss_dir = dir.join("staging_segment");
            let rc_has_data = dir_has_segment_data(&rc_dir);
            let ss_has_data = dir_has_segment_data(&ss_dir);
            fs::create_dir_all(&rc_dir)?;
            fs::create_dir_all(&ss_dir)?;
            read_cache_dirs.push(rc_dir);
            staging_segment_dirs.push(ss_dir);
            read_cache_dirs_have_data.push(rc_has_data);
            staging_segment_dirs_have_data.push(ss_has_data);
        }

        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(16);
        let default_shards = std::cmp::max(cores.next_power_of_two(), 16);

        let (read_shards, actual_max_read_bytes) = if max_read_bytes < 10 * 1024 * 1024 {
            (1, max_read_bytes)
        } else {
            #[cfg(test)]
            {
                let mut shards = default_shards;
                while shards > 16 && max_read_bytes / (shards as u64) < 4 * 1024 * 1024 {
                    shards /= 2;
                }
                (shards, max_read_bytes)
            }
            #[cfg(not(test))]
            {
                let min_required = (default_shards * 4 * 1024 * 1024) as u64;
                (default_shards, std::cmp::max(max_read_bytes, min_required))
            }
        };

        let read_cache_dirs_refs: Vec<&std::path::Path> =
            read_cache_dirs.iter().map(|p| p.as_path()).collect();
        let read_capacities = if staging_dirs.is_empty() {
            vec![actual_max_read_bytes as usize]
        } else {
            let read_cap = actual_max_read_bytes as usize / staging_dirs.len();
            vec![read_cap; staging_dirs.len()]
        };
        let read_nvme_cache = Arc::new(crate::tiering::nvme::NvmeCache::new(
            &read_cache_dirs_refs,
            &read_capacities,
            read_shards,
        )?);

        let (write_shards, actual_max_write_bytes) = if max_write_bytes < 10 * 1024 * 1024 {
            (1, max_write_bytes)
        } else {
            #[cfg(test)]
            {
                let mut shards = default_shards;
                while shards > 16 && max_write_bytes / (shards as u64) < 4 * 1024 * 1024 {
                    shards /= 2;
                }
                (shards, max_write_bytes)
            }
            #[cfg(not(test))]
            {
                let min_required = (default_shards * 4 * 1024 * 1024) as u64;
                (default_shards, std::cmp::max(max_write_bytes, min_required))
            }
        };

        let staging_dirs_refs: Vec<&std::path::Path> =
            staging_segment_dirs.iter().map(|p| p.as_path()).collect();
        let write_capacities = if staging_dirs.is_empty() {
            vec![actual_max_write_bytes as usize]
        } else {
            let write_cap = actual_max_write_bytes as usize / staging_dirs.len();
            vec![write_cap; staging_dirs.len()]
        };
        let staging_nvme_cache = Arc::new(crate::tiering::nvme::NvmeCache::new(
            &staging_dirs_refs,
            &write_capacities,
            write_shards,
        )?);

        // Recover persistent indexes only when segment data existed before this startup.
        if staging_segment_dirs_have_data
            .iter()
            .any(|has_data| *has_data)
        {
            staging_nvme_cache.recover_index();
        }
        if read_cache_dirs_have_data.iter().any(|has_data| *has_data) {
            read_nvme_cache.recover_index();
        }

        let initial_write_bytes = staging_nvme_cache.current_bytes() as u64;

        // P1-1: bound the merge worker queue to avoid unbounded RAM growth under write storms.
        const STAGING_MERGE_QUEUE_CAP: usize = 1024;
        let (write_tx, write_rx) = mpsc::channel::<PendingStagedWrite>(STAGING_MERGE_QUEUE_CAP);

        let backend_router = std::sync::Arc::new(once_cell::sync::OnceCell::new());

        let staging = Self {
            staging_dirs: staging_dirs.clone(),
            max_write_bytes: actual_max_write_bytes,
            max_read_bytes: actual_max_read_bytes,
            block_allocator: block_allocator.clone(),
            nvme_writer: nvme_writer.clone(),
            backend_router,
            redis_client: redis_client.clone(),
            meta_backend: std::sync::Arc::new(once_cell::sync::OnceCell::new()),
            write_tx,
            p2p_addr: std::sync::Arc::new(std::sync::OnceLock::new()),
            current_staged_write_bytes: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                initial_write_bytes,
            )),
            space_freed_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            staged_writes_in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                staging_nvme_cache.list_keys().len(),
            )),
            staged_drained_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            read_nvme_cache,
            staging_nvme_cache,
            dht_node: std::sync::Arc::new(std::sync::OnceLock::new()),
            crypto: std::sync::Arc::new(std::sync::OnceLock::new()),
        };

        // Spawn background merge worker
        staging.start_merge_worker(write_rx);

        Ok(staging)
    }

    pub fn set_backend_router(&self, router: std::sync::Arc<crate::routing::BackendRouter>) {
        let _ = self.backend_router.set(router);
    }

    pub fn redis_client(&self) -> &std::sync::Arc<crate::dlm::MetaClient> {
        &self.redis_client
    }

    pub fn get_staged_path(&self, _file_id: &str) -> PathBuf {
        self.staging_dirs.first().cloned().unwrap_or_default()
    }

    /// Stage a write locally into staging_nvme_cache using zero-copy memory-mapped segments.
    pub async fn stage_write(
        &self,
        file_path: &str,
        file_id: &str,
        data: &[u8],
        fencing_token: u64,
    ) -> Result<()> {
        let meta = StagedMetadata {
            fencing_token,
            original_size: data.len() as u64,
            file_path: file_path.to_string(),
        };
        let meta_bytes = meta.serialize();
        let meta_len = meta_bytes.len() as u64;
        let unpadded_len = 8 + meta_bytes.len() + data.len();
        let padded_size = unpadded_len as u64;

        let target_dir = self.staging_dirs.first().ok_or_else(|| {
            SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "No staging directories configured (memory mode)",
            ))
        })?;

        if !check_disk_free_safeguard(target_dir) {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "Local NVMe staging disk free space safeguard triggered (< 1% or < 100MB free)"
                    .to_string(),
            )));
        }

        let mut total_staged_bytes = self
            .current_staged_write_bytes
            .load(std::sync::atomic::Ordering::Relaxed);

        if total_staged_bytes + padded_size > self.max_write_bytes {
            let mut attempts = 0;
            while total_staged_bytes + padded_size > self.max_write_bytes && attempts < 4 {
                let notified = self.space_freed_notify.notified();
                tokio::pin!(notified);
                let wait_timeout = tokio::time::timeout(Duration::from_millis(500), notified);
                let _ = wait_timeout.await;
                attempts += 1;
                total_staged_bytes = self
                    .current_staged_write_bytes
                    .load(std::sync::atomic::Ordering::Relaxed);
            }
        }

        if total_staged_bytes + padded_size > self.max_write_bytes {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "Local NVMe staging cache capacity exceeded: current {} bytes, writing {} bytes, max capacity {} bytes",
                    total_staged_bytes, padded_size, self.max_write_bytes
                )
            )));
        }

        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());

        let mut packed_payload = Vec::with_capacity(unpadded_len);
        packed_payload.extend_from_slice(&meta_len.to_be_bytes());
        packed_payload.extend_from_slice(&meta_bytes);
        packed_payload.extend_from_slice(data);

        let payload_bytes = Bytes::from(packed_payload);

        let is_new = self.staging_nvme_cache.get(&key_bytes).is_none();
        // Memory-mapped copy (lock-free, zero disk syscall wait)
        self.staging_nvme_cache
            .put(key_bytes.clone(), payload_bytes);

        self.current_staged_write_bytes
            .fetch_add(padded_size, std::sync::atomic::Ordering::Relaxed);

        if is_new {
            self.staged_writes_in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        info!(
            "NVMe Staging: Staged write for file {} (ID: {}) size = {} bytes. Acknowledging write to OS.",
            file_path, file_id, data.len()
        );

        let pending = PendingStagedWrite {
            file_path: file_path.to_string(),
            file_id: file_id.to_string(),
            fencing_token,
            padded_size,
        };

        // Non-blocking backpressure: full queue maps to StorageFull so callers can
        // fall back to the synchronous backend write path. Roll back the cache put
        // so we do not leave an un-notified staged blob.
        match self.write_tx.try_send(pending) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let _ = self.staging_nvme_cache.remove(&key_bytes);
                self.current_staged_write_bytes
                    .fetch_sub(padded_size, std::sync::atomic::Ordering::Relaxed);
                if is_new {
                    self.staged_writes_in_flight
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
                Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "Staging merge queue full; apply backpressure / fallback",
                )))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                let _ = self.staging_nvme_cache.remove(&key_bytes);
                self.current_staged_write_bytes
                    .fetch_sub(padded_size, std::sync::atomic::Ordering::Relaxed);
                if is_new {
                    self.staged_writes_in_flight
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
                Err(SqueezefsError::Io(std::io::Error::other(
                    "Staging merge worker channel closed",
                )))
            }
        }
    }

    /// Put a packed active block write to staging_nvme_cache.
    pub fn put_active_block(&self, key: &str, data: &[u8], fencing_token: u64) {
        let meta = StagedMetadata {
            fencing_token,
            original_size: data.len() as u64,
            file_path: key.to_string(),
        };
        let meta_bytes = meta.serialize();
        let meta_len = meta_bytes.len() as u64;

        let mut packed_payload = Vec::with_capacity(4096 + data.len());
        packed_payload.extend_from_slice(&meta_len.to_be_bytes());
        packed_payload.extend_from_slice(&meta_bytes);
        packed_payload.resize(4096, 0); // Pad header up to 4KB page boundary
        packed_payload.extend_from_slice(data);

        let key_bytes = Bytes::copy_from_slice(key.as_bytes());
        let payload_bytes = Bytes::from(packed_payload);

        let is_new = self.staging_nvme_cache.get(&key_bytes).is_none();
        self.staging_nvme_cache.put(key_bytes, payload_bytes);
        if is_new {
            self.staged_writes_in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Remove a packed active block write from staging_nvme_cache.
    pub fn remove_active_block(&self, key: &str) -> Option<Vec<u8>> {
        let val = self.read_staged(key);
        let key_bytes = Bytes::copy_from_slice(key.as_bytes());
        if self.staging_nvme_cache.remove(&key_bytes).is_some() {
            let prev = self
                .staged_writes_in_flight
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            if prev == 1 {
                self.staged_drained_notify.notify_waiters();
            }
        }
        val
    }

    /// Remove a staged write from staging_nvme_cache.
    pub fn remove_staged(&self, file_id: &str) -> Option<Vec<u8>> {
        self.remove_active_block(file_id)
    }

    /// Read staged data directly from staging_nvme_cache memory-mapped segments.
    pub fn read_staged(&self, file_id: &str) -> Option<Vec<u8>> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let guard = self.staging_nvme_cache.get(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() >= 8 {
            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() >= 8 + meta_len {
                if let Some(meta) = StagedMetadata::deserialize(&bytes[8..8 + meta_len]) {
                    let data_start = if file_id.starts_with("active_block:") {
                        4096
                    } else {
                        8 + meta_len
                    };
                    let data_end = data_start + meta.original_size as usize;
                    if bytes.len() >= data_end {
                        return Some(bytes[data_start..data_end].to_vec());
                    }
                }
            }
        }
        None
    }

    /// Read staged fencing token directly from staging_nvme_cache memory-mapped segments without copying data.
    pub fn get_staged_fencing_token(&self, file_id: &str) -> Option<u64> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let guard = self.staging_nvme_cache.get(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() >= 8 {
            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() >= 8 + meta_len {
                if let Some(meta) = StagedMetadata::deserialize(&bytes[8..8 + meta_len]) {
                    return Some(meta.fencing_token);
                }
            }
        }
        None
    }

    /// Read staged data zero-copy directly from staging_nvme_cache memory-mapped segments.
    pub fn read_staged_zero_copy(
        &self,
        file_id: &str,
    ) -> Option<crate::tiering::nvme::NvmeCacheReadGuard> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let mut guard = self.staging_nvme_cache.get_static(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() >= 8 {
            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() >= 8 + meta_len {
                if let Some(meta) = StagedMetadata::deserialize(&bytes[8..8 + meta_len]) {
                    let data_start = if file_id.starts_with("active_block:") {
                        4096
                    } else {
                        8 + meta_len
                    };
                    let data_end = data_start + meta.original_size as usize;
                    if bytes.len() >= data_end {
                        guard.offset += data_start;
                        guard.len = meta.original_size as usize;
                        return Some(guard);
                    }
                }
            }
        }
        None
    }

    fn start_merge_worker(&self, mut write_rx: mpsc::Receiver<PendingStagedWrite>) {
        let block_allocator = self.block_allocator.clone();
        let nvme_writer = self.nvme_writer.clone();
        let backend_router = self.backend_router.clone();
        let meta_backend = self.meta_backend.clone();
        let staged_bytes = self.current_staged_write_bytes.clone();
        let space_freed_notify = self.space_freed_notify.clone();
        let staging_nvme_cache = self.staging_nvme_cache.clone();
        let staged_writes_in_flight = self.staged_writes_in_flight.clone();
        let staged_drained_notify = self.staged_drained_notify.clone();
        let crypto = self.crypto.clone();

        tokio::spawn(async move {
            let mut batch: Vec<PendingStagedWrite> = Vec::new();
            let mut current_bytes = 0u64;
            let max_batch_bytes = 4 * 1024 * 1024;
            let flush_timeout = Duration::from_millis(500);

            loop {
                let sleep = time::sleep(flush_timeout);
                tokio::pin!(sleep);

                tokio::select! {
                    Some(pending) = write_rx.recv() => {
                        current_bytes += pending.padded_size;
                        batch.push(pending);

                        if current_bytes >= max_batch_bytes {
                            info!("NVMe Staging: Batch size threshold reached ({} bytes). Flushing merged block.", current_bytes);
                            if let Err(e) = Self::flush_batch(&staging_nvme_cache, &meta_backend, &mut batch, &mut current_bytes, &staged_bytes, &space_freed_notify, &backend_router, &block_allocator, &nvme_writer, &staged_writes_in_flight, &staged_drained_notify, &crypto).await {
                                error!("Failed to flush NVMe staging batch: {:?}", e);
                            }
                        }
                    }
                    _ = &mut sleep => {
                        if !batch.is_empty() {
                            info!("NVMe Staging: Timeout reached. Flushing merged block with {} pending writes.", batch.len());
                            if let Err(e) = Self::flush_batch(&staging_nvme_cache, &meta_backend, &mut batch, &mut current_bytes, &staged_bytes, &space_freed_notify, &backend_router, &block_allocator, &nvme_writer, &staged_writes_in_flight, &staged_drained_notify, &crypto).await {
                                error!("Failed to flush NVMe staging batch on timeout: {:?}", e);
                            }
                        }
                    }
                }
            }
        });
    }

    async fn flush_batch(
        staging_nvme_cache: &crate::tiering::nvme::NvmeCache,
        meta_backend: &std::sync::Arc<
            once_cell::sync::OnceCell<std::sync::Arc<crate::meta_backend::RoutedMetaBackend>>,
        >,
        batch: &mut Vec<PendingStagedWrite>,
        current_bytes: &mut u64,
        staged_bytes: &std::sync::Arc<std::sync::atomic::AtomicU64>,
        space_freed_notify: &tokio::sync::Notify,
        backend_router: &std::sync::Arc<
            once_cell::sync::OnceCell<std::sync::Arc<crate::routing::BackendRouter>>,
        >,
        default_allocator: &std::sync::Arc<crate::block_allocator::BlockAllocator>,
        default_writer: &std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
        staged_writes_in_flight: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
        staged_drained_notify: &tokio::sync::Notify,
        crypto: &std::sync::Arc<std::sync::OnceLock<crate::crypto_compress::CryptoCompressState>>,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        crate::coz_progress!("nvme_flush_batch");

        let default_crypto = crate::crypto_compress::CryptoCompressState::new(
            "none".to_string(),
            "none".to_string(),
            None,
        );
        let crypto_state = crypto.get().unwrap_or(&default_crypto);

        let (_be_id, block_allocator, nvme_writer) = if let Some(router) = backend_router.get() {
            router.get_active_backend()?
        } else {
            (
                "backend_0".to_string(),
                default_allocator.clone(),
                default_writer.clone(),
            )
        };

        let offset = block_allocator.allocate_block().await?;
        let packed_key = offset.to_string();

        let mut packed_payload = Vec::new();
        let mut mappings = Vec::new();
        let mut highest_fencing_token = 0u64;

        for item in batch.iter() {
            let key_bytes = Bytes::copy_from_slice(item.file_id.as_bytes());
            let data_res = if let Some(guard) = staging_nvme_cache.get(&key_bytes) {
                let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
                if bytes.len() >= 8 {
                    let meta_len =
                        u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
                    if bytes.len() >= 8 + meta_len {
                        if let Some(meta) = StagedMetadata::deserialize(&bytes[8..8 + meta_len]) {
                            let data_start = 8 + meta_len;
                            let data_end = data_start + meta.original_size as usize;
                            if bytes.len() >= data_end {
                                Some(bytes[data_start..data_end].to_vec())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(data) = data_res {
                let processed_data = crypto_state.process_write(bytes::Bytes::from(data))?;
                let offset = packed_payload.len() as u64;
                let size = processed_data.len() as u64;
                packed_payload.extend_from_slice(&processed_data);

                mappings.push((item.file_id.clone(), offset, size));
                if item.fencing_token > highest_fencing_token {
                    highest_fencing_token = item.fencing_token;
                }
            }
        }

        let packed_payload_len = packed_payload.len();
        info!("NVMe Staging: Writing packed block {} (size {} bytes) to NVMe-oF backend volume with fencing token {}.", packed_key, packed_payload_len, highest_fencing_token);
        if let Err(e) = nvme_writer
            .write_block(offset, bytes::Bytes::from(packed_payload))
            .await
        {
            let _ = block_allocator.free_block(offset).await;
            return Err(e);
        }

        if let Some(backend) = meta_backend.get() {
            for (item, (_file_id, sub_offset, sub_size)) in batch.iter().zip(mappings.iter()) {
                let file_path = &item.file_path;
                let ino = crate::routing::parse_inode_from_path(file_path);

                let layout_bytes = backend.getxattr(ino, "layout").await.unwrap_or(None);
                let layout_opt = if let Some(ref bytes) = layout_bytes {
                    if bytes.starts_with(b"{") {
                        serde_json::from_slice::<crate::routing::LayoutMetadata>(bytes).ok()
                    } else {
                        bincode::deserialize::<crate::routing::LayoutMetadata>(bytes).ok()
                    }
                } else {
                    None
                };
                let mut meta = layout_opt.unwrap_or_default();

                if meta.file_id.as_deref() == Some(&item.file_id) {
                    let mut block_map = meta.block_map.unwrap_or_default();
                    let val_str = format!("{}:{}:{}", packed_key, sub_offset, sub_size);
                    block_map.insert(0, val_str);
                    meta.block_map = Some(block_map);
                    meta.file_id = None;

                    if let Ok(serialized) = bincode::serialize(&meta) {
                        let _ = backend.setxattr(ino, "layout", &serialized).await;
                    }
                } else {
                    info!(
                        "NVMe Staging: Skip merge worker update for {} because file_id changed or promoted",
                        file_path
                    );
                }
            }
        }

        for item in batch.iter() {
            let key_bytes = Bytes::copy_from_slice(item.file_id.as_bytes());
            if staging_nvme_cache.remove(&key_bytes).is_some() {
                let prev =
                    staged_writes_in_flight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                if prev == 1 {
                    staged_drained_notify.notify_waiters();
                }
            }
            let mut val = staged_bytes.load(std::sync::atomic::Ordering::Relaxed);
            loop {
                let new_val = val.saturating_sub(item.padded_size);
                match staged_bytes.compare_exchange_weak(
                    val,
                    new_val,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => val = actual,
                }
            }
        }

        batch.clear();
        *current_bytes = 0;
        space_freed_notify.notify_waiters();

        Ok(())
    }

    pub fn staging_dirs(&self) -> &[PathBuf] {
        &self.staging_dirs
    }

    /// Cache a block of read data on local NVMe using read_nvme_cache.
    pub fn cache_read_block(&self, block_key: &str, data: Bytes) -> Result<()> {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let val_bytes = data;
        self.read_nvme_cache
            .put(key_bytes.clone(), val_bytes.clone());

        if let Some(dht) = self.dht_node.get() {
            let dht_clone = dht.clone();
            let key_hash = xxh3_64(block_key.as_bytes());
            // P1-5: best-effort peer publish under global admission.
            crate::bg_admit::spawn_bg(async move {
                let owners = dht_clone.find_closest_peers(key_hash, 3);
                if let Some(primary_owner) = owners.first() {
                    if primary_owner != dht_clone.peer_addr() {
                        let _ = dht_clone
                            .store_remote_value(primary_owner, key_bytes, val_bytes)
                            .await;
                    }
                }
            });
        }

        Ok(())
    }

    pub fn current_staged_write_bytes(&self) -> u64 {
        self.current_staged_write_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn current_read_cache_bytes(&self) -> u64 {
        self.read_nvme_cache.current_bytes() as u64
    }

    pub fn read_cached_block(&self, block_key: &str) -> Option<Vec<u8>> {
        self.get_cached_read_block(block_key)
    }

    pub fn get_cached_read_block(&self, block_key: &str) -> Option<Vec<u8>> {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let guard = self.read_nvme_cache.get(&key_bytes)?;
        Some(guard.guard.mmap[guard.offset..guard.offset + guard.len].to_vec())
    }

    pub fn get_cached_read_block_range(
        &self,
        block_key: &str,
        offset: u64,
        size: u32,
    ) -> Option<Vec<u8>> {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let guard = self.read_nvme_cache.get(&key_bytes)?;
        let start = guard.offset + offset as usize;
        if start >= guard.offset + guard.len {
            return Some(Vec::new());
        }
        let read_len = std::cmp::min(size as usize, guard.len - offset as usize);
        let end = start + read_len;
        Some(guard.guard.mmap[start..end].to_vec())
    }

    pub fn get_cached_read_block_range_zero_copy(
        &self,
        block_key: &str,
        offset: u64,
        size: u32,
    ) -> Option<crate::tiering::nvme::NvmeCacheReadGuard> {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let mut guard = self.read_nvme_cache.get_static(&key_bytes)?;
        let start = guard.offset + offset as usize;
        if start >= guard.offset + guard.len {
            guard.offset += guard.len;
            guard.len = 0;
            return Some(guard);
        }
        let read_len = std::cmp::min(size as usize, guard.len - offset as usize);
        guard.offset = start;
        guard.len = read_len;
        Some(guard)
    }

    pub fn list_staged_files(&self) -> Vec<String> {
        self.staging_nvme_cache
            .list_keys()
            .into_iter()
            .filter_map(|k| String::from_utf8(k.to_vec()).ok())
            .collect()
    }

    pub fn list_cached_blocks(&self) -> Vec<String> {
        self.read_nvme_cache
            .list_keys()
            .into_iter()
            .filter_map(|k| String::from_utf8(k.to_vec()).ok())
            .collect()
    }

    pub fn max_write_bytes(&self) -> u64 {
        self.max_write_bytes
    }

    pub fn max_read_bytes(&self) -> u64 {
        self.max_read_bytes
    }
}
