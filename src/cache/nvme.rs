use crate::backend::RustFsClient;
use crate::error::{Result, SqueezefsError};
use log::{debug, error, info, warn};
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use uuid::Uuid;

fn write_aligned_direct(path: &PathBuf, data: &[u8]) -> std::io::Result<()> {
    let align = 4096;
    let padded_size = (data.len() + align - 1) & !(align - 1);
    let mut buf = Vec::with_capacity(padded_size + align);
    let ptr = buf.as_ptr() as usize;
    let offset = (align - (ptr % align)) % align;
    buf.resize(offset + padded_size, 0);
    buf[offset..offset + data.len()].copy_from_slice(data);
    let aligned_data = &buf[offset..offset + padded_size];

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECT);
    }

    let mut file = match options.open(path) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            log::warn!("O_DIRECT write not supported on staging filesystem. Falling back to buffered I/O for: {:?}", path);
            let mut fallback_opts = std::fs::OpenOptions::new();
            fallback_opts.write(true).create(true).truncate(true);
            fallback_opts.open(path)?
        }
        Err(e) => return Err(e),
    };
    file.write_all(aligned_data)?;
    Ok(())
}

fn read_aligned_direct(path: &PathBuf, actual_size: usize) -> std::io::Result<Vec<u8>> {
    let align = 4096;
    let padded_size = (actual_size + align - 1) & !(align - 1);
    let mut buf = Vec::with_capacity(padded_size + align);
    let ptr = buf.as_ptr() as usize;
    let offset = (align - (ptr % align)) % align;
    buf.resize(offset + padded_size, 0);

    let mut options = std::fs::OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECT);
    }

    let mut file = match options.open(path) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            log::warn!("O_DIRECT read not supported on staging filesystem. Falling back to buffered I/O for: {:?}", path);
            let mut fallback_opts = std::fs::OpenOptions::new();
            fallback_opts.read(true);
            fallback_opts.open(path)?
        }
        Err(e) => return Err(e),
    };
    file.read_exact(&mut buf[offset..offset + padded_size])?;

    let mut out = vec![0; actual_size];
    out.copy_from_slice(&buf[offset..offset + actual_size]);
    Ok(out)
}

#[derive(Clone)]
pub struct NvmeStaging {
    staging_dirs: Vec<PathBuf>,
    max_write_bytes: u64,
    max_read_bytes: u64,
    backend: RustFsClient,
    redis_client: crate::dlm::MetaClient,
    write_tx: mpsc::Sender<PendingStagedWrite>,
    pub p2p_addr: std::sync::Arc<std::sync::OnceLock<String>>,
    current_staged_write_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    current_read_cache_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Debug)]
pub struct PendingStagedWrite {
    pub file_path: String,
    pub file_id: String,
    pub fencing_token: u64,
}

fn get_dir_index(file_id: &str, num_dirs: usize) -> usize {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    file_id.hash(&mut hasher);
    (hasher.finish() as usize) % num_dirs
}

impl NvmeStaging {
    pub fn new(
        staging_dirs: Vec<PathBuf>,
        max_write_bytes: u64,
        max_read_bytes: u64,
        backend: RustFsClient,
        redis_client: crate::dlm::MetaClient,
    ) -> Result<Self> {
        if staging_dirs.is_empty() {
            return Err(SqueezefsError::InvalidOperation(
                "At least one staging directory must be specified".to_string(),
            ));
        }

        // Ensure all staging directories and their subdirectories exist
        for dir in &staging_dirs {
            fs::create_dir_all(dir.join("active_writes"))?;
            fs::create_dir_all(dir.join("staging"))?;
            fs::create_dir_all(dir.join("cache"))?;
        }

        let (write_tx, write_rx) = mpsc::channel::<PendingStagedWrite>(1000);

        let mut initial_write_bytes = 0u64;
        let mut initial_read_bytes = 0u64;
        for dir in &staging_dirs {
            // Scan staging subdirectory for staged files (file_*.staged)
            let staging_dir = dir.join("staging");
            if let Ok(entries) = std::fs::read_dir(&staging_dir) {
                for entry in entries.flatten() {
                    if let Ok(meta) = entry.metadata() {
                        if meta.is_file() {
                            let path = entry.path();
                            if path.extension().is_some_and(|ext| ext == "staged") {
                                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                                    if name.starts_with("file_") {
                                        initial_write_bytes += meta.len();
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Scan cache subdirectory for block cache files (block_*.block)
            let cache_dir = dir.join("cache");
            if let Ok(entries) = std::fs::read_dir(&cache_dir) {
                for entry in entries.flatten() {
                    if let Ok(meta) = entry.metadata() {
                        if meta.is_file() {
                            let path = entry.path();
                            if path.extension().is_some_and(|ext| ext == "block") {
                                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                                    if name.starts_with("block_") {
                                        initial_read_bytes += meta.len();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let staging = Self {
            staging_dirs: staging_dirs.clone(),
            max_write_bytes,
            max_read_bytes,
            backend: backend.clone(),
            redis_client: redis_client.clone(),
            write_tx,
            p2p_addr: std::sync::Arc::new(std::sync::OnceLock::new()),
            current_staged_write_bytes: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                initial_write_bytes,
            )),
            current_read_cache_bytes: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                initial_read_bytes,
            )),
        };

        // Spawn the background merge worker
        staging.start_merge_worker(write_rx);

        Ok(staging)
    }

    /// Retrieve the staging directory for a specific file_id.
    pub fn get_staged_path(&self, file_id: &str) -> PathBuf {
        let idx = get_dir_index(file_id, self.staging_dirs.len());
        self.staging_dirs[idx].join("staging")
    }

    /// Stage a write locally to NVMe staging, returning immediately.
    /// The background worker will pack it and upload it asynchronously.
    pub async fn stage_write(
        &self,
        file_path: &str,
        file_id: &str,
        data: &[u8],
        fencing_token: u64,
    ) -> Result<()> {
        let meta_content = serde_json::json!({
            "file_path": file_path,
            "fencing_token": fencing_token,
            "original_size": data.len()
        });
        let meta_json_bytes = serde_json::to_vec(&meta_content).unwrap();
        let unpadded_len = 8 + meta_json_bytes.len() + data.len();
        let align = 4096;
        let padded_size = ((unpadded_len + align - 1) & !(align - 1)) as u64;

        // Enforce max bytes capacity constraint asynchronously using the exact padded file size
        let total_staged_bytes = self
            .current_staged_write_bytes
            .load(std::sync::atomic::Ordering::Relaxed);

        if total_staged_bytes + padded_size > self.max_write_bytes {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "Local NVMe staging cache capacity exceeded: current {} bytes, writing {} bytes, max capacity {} bytes",
                    total_staged_bytes, padded_size, self.max_write_bytes
                )
            )));
        }

        let target_dir = self.get_staged_path(file_id);
        let staged_path = target_dir.join(format!("file_{}.staged", file_id));

        let data_clone = data.to_vec();

        tokio::task::spawn_blocking(move || -> std::result::Result<(), SqueezefsError> {
            let meta_len = meta_json_bytes.len() as u64;

            let mut packed_payload =
                Vec::with_capacity(8 + meta_json_bytes.len() + data_clone.len());
            packed_payload.extend_from_slice(&meta_len.to_be_bytes());
            packed_payload.extend_from_slice(&meta_json_bytes);
            packed_payload.extend_from_slice(&data_clone);

            write_aligned_direct(&staged_path, &packed_payload).map_err(SqueezefsError::Io)?;
            Ok(())
        })
        .await
        .unwrap()?;

        self.current_staged_write_bytes
            .fetch_add(padded_size, std::sync::atomic::Ordering::Relaxed);

        info!(
            "NVMe Staging: Staged write for file {} (ID: {}) size = {} bytes. Acknowledging write to OS.",
            file_path, file_id, data.len()
        );

        // Notify background worker
        let pending = PendingStagedWrite {
            file_path: file_path.to_string(),
            file_id: file_id.to_string(),
            fencing_token,
        };

        self.write_tx.send(pending).await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::other(format!(
                "Failed to notify merge worker: {:?}",
                e
            )))
        })?;

        Ok(())
    }

    /// Read staged data directly from NVMe if it exists locally and has not yet been merged/cleared.
    pub fn read_staged(&self, file_id: &str) -> Option<Vec<u8>> {
        let target_dir = self.get_staged_path(file_id);
        let staged_path = target_dir.join(format!("file_{}.staged", file_id));

        if staged_path.exists() {
            if let Ok(metadata) = fs::metadata(&staged_path) {
                let file_len = metadata.len();
                if let Ok(bytes) = read_aligned_direct(&staged_path, file_len as usize) {
                    if bytes.len() >= 8 {
                        let meta_len =
                            u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
                        if bytes.len() >= 8 + meta_len {
                            if let Ok(meta_json) =
                                serde_json::from_slice::<serde_json::Value>(&bytes[8..8 + meta_len])
                            {
                                if let Some(orig_size) =
                                    meta_json.get("original_size").and_then(|v| v.as_u64())
                                {
                                    let data_start = 8 + meta_len;
                                    let data_end = data_start + orig_size as usize;
                                    if bytes.len() >= data_end {
                                        return Some(bytes[data_start..data_end].to_vec());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Start background merge worker.
    fn start_merge_worker(&self, mut write_rx: mpsc::Receiver<PendingStagedWrite>) {
        let staging_dirs = self.staging_dirs.clone();
        let backend = self.backend.clone();
        let redis_client = self.redis_client.clone();
        let staged_bytes = self.current_staged_write_bytes.clone();

        tokio::spawn(async move {
            let mut batch: Vec<PendingStagedWrite> = Vec::new();
            let mut current_bytes = 0u64;
            let max_batch_bytes = 4 * 1024 * 1024; // 4MB
            let mut flush_timeout = Duration::from_millis(500);

            let mut last_query = time::Instant::now() - Duration::from_secs(60);
            let query_interval = Duration::from_secs(5);

            loop {
                if last_query.elapsed() >= query_interval {
                    if let Ok(mut con) = redis_client.get_connection().await {
                        let delay_str: Option<String> = redis::cmd("HGET")
                            .arg("squeezefs:format")
                            .arg("upload_delay")
                            .query_async(&mut con)
                            .await
                            .unwrap_or(None);
                        if let Some(ds) = delay_str {
                            if let Ok(parsed) = crate::cache::parse_duration(&ds) {
                                if parsed != flush_timeout {
                                    debug!("NVMe Staging: Dynamic upload_delay changed from {:?} to {:?}", flush_timeout, parsed);
                                    flush_timeout = parsed;
                                }
                            }
                        }
                    }
                    last_query = time::Instant::now();
                }

                let sleep = time::sleep(flush_timeout);
                tokio::pin!(sleep);

                tokio::select! {
                    Some(pending) = write_rx.recv() => {
                        let idx = get_dir_index(&pending.file_id, staging_dirs.len());
                        let local_path = staging_dirs[idx].join("staging").join(format!("file_{}.staged", pending.file_id));
                        let (meta_len, ok) = tokio::task::spawn_blocking(move || {
                            if let Ok(metadata) = fs::metadata(&local_path) {
                                (metadata.len(), true)
                            } else { (0, false) }
                        }).await.unwrap();

                        if ok {
                            current_bytes += meta_len;
                            batch.push(pending);
                        }

                        if current_bytes >= max_batch_bytes {
                            info!("NVMe Staging: Batch size threshold reached ({} bytes). Flushing merged block.", current_bytes);
                            if let Err(e) = Self::flush_batch(&staging_dirs, &backend, &redis_client, &mut batch, &mut current_bytes, &staged_bytes).await {
                                error!("Failed to flush NVMe staging batch: {:?}", e);
                            }
                        }
                    }
                    _ = &mut sleep => {
                        if !batch.is_empty() {
                            info!("NVMe Staging: Timeout reached. Flushing merged block with {} pending writes.", batch.len());
                            if let Err(e) = Self::flush_batch(&staging_dirs, &backend, &redis_client, &mut batch, &mut current_bytes, &staged_bytes).await {
                                error!("Failed to flush NVMe staging batch on timeout: {:?}", e);
                            }
                        }
                    }
                }
            }
        });
    }

    /// Merge the batch of NVMe files, upload to S3 (RustFS), and record mappings.
    async fn flush_batch(
        staging_dirs: &[PathBuf],
        backend: &RustFsClient,
        redis_client: &crate::dlm::MetaClient,
        batch: &mut Vec<PendingStagedWrite>,
        current_bytes: &mut u64,
        staged_bytes: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        crate::coz_progress!("nvme_flush_batch");

        // Fetch format settings from Garnet/Redis for compression & encryption
        let mut con = redis_client.get_connection().await?;
        use redis::AsyncCommands;

        let compression: String = con
            .hget("squeezefs:format", "compression")
            .await
            .unwrap_or(None)
            .unwrap_or_else(|| "none".to_string());
        let encrypt_algo: String = con
            .hget("squeezefs:format", "encrypt_algo")
            .await
            .unwrap_or(None)
            .unwrap_or_else(|| "none".to_string());
        let encrypt_key: Option<String> = con
            .hget("squeezefs:format", "encrypt_key")
            .await
            .unwrap_or(None);

        let crypto_state = crate::crypto_compress::CryptoCompressState::new(
            compression,
            encrypt_algo,
            encrypt_key.as_deref(),
        );

        let packed_id = Uuid::new_v4().to_string();
        let packed_key = format!("packed/blocks/{}", packed_id);

        let mut packed_payload = Vec::new();
        let mut mappings = Vec::new(); // Mappings: (file_id, offset, size)
        let mut highest_fencing_token = 0u64;

        // 1. Pack individual staged file bytes into one payload
        for item in batch.iter() {
            let idx = get_dir_index(&item.file_id, staging_dirs.len());
            let local_path = staging_dirs[idx]
                .join("staging")
                .join(format!("file_{}.staged", item.file_id));

            let data_res = tokio::task::spawn_blocking(move || {
                if let Ok(metadata) = fs::metadata(&local_path) {
                    let file_len = metadata.len();
                    if let Ok(bytes) = read_aligned_direct(&local_path, file_len as usize) {
                        if bytes.len() >= 8 {
                            let meta_len =
                                u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8]))
                                    as usize;
                            if bytes.len() >= 8 + meta_len {
                                if let Ok(meta_json) = serde_json::from_slice::<serde_json::Value>(
                                    &bytes[8..8 + meta_len],
                                ) {
                                    if let Some(orig_size) =
                                        meta_json.get("original_size").and_then(|v| v.as_u64())
                                    {
                                        let data_start = 8 + meta_len;
                                        let data_end = data_start + orig_size as usize;
                                        if bytes.len() >= data_end {
                                            return Some(bytes[data_start..data_end].to_vec());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                None
            })
            .await
            .unwrap();

            if let Some(data) = data_res {
                let processed_data = crypto_state.process_write(&data)?;
                let offset = packed_payload.len() as u64;
                let size = processed_data.len() as u64;
                packed_payload.extend_from_slice(&processed_data);

                mappings.push((item.file_id.clone(), offset, size));
                if item.fencing_token > highest_fencing_token {
                    highest_fencing_token = item.fencing_token;
                }
            }
        }

        // 2. Upload the packed payload to RustFS S3
        info!("NVMe Staging: Uploading packed block {} (size {} bytes) to RustFS with fencing token {}.", packed_key, packed_payload.len(), highest_fencing_token);
        backend
            .put_object(&packed_key, packed_payload, highest_fencing_token)
            .await?;

        // 3. Update Garnet metadata mapping for each individual file ID
        if let Ok(mut con) = redis_client.get_connection().await {
            for (file_id, offset, size) in mappings.iter() {
                let mapping_key = format!("mapping:{}", file_id);
                let _: std::result::Result<(), redis::RedisError> = redis::pipe()
                    .hset(&mapping_key, "block", &packed_key)
                    .hset(&mapping_key, "offset", *offset)
                    .hset(&mapping_key, "size", *size)
                    .query_async(&mut con)
                    .await;

                debug!(
                    "NVMe Staging: Recorded Garnet offset map: mapping:{} -> block: {}, offset: {}, size: {}",
                    file_id, packed_key, offset, size
                );
            }
        } else {
            warn!("NVMe Staging: Failed to connect to Redis/Garnet to register packed block mappings. Mappings will not be available in metadata.");
        }

        // 4. Remove local NVMe staging files
        for item in batch.iter() {
            let idx = get_dir_index(&item.file_id, staging_dirs.len());
            let target_dir = &staging_dirs[idx];
            let local_path = target_dir
                .join("staging")
                .join(format!("file_{}.staged", item.file_id));
            if local_path.exists() {
                if let Ok(meta) = fs::metadata(&local_path) {
                    staged_bytes.fetch_sub(meta.len(), std::sync::atomic::Ordering::Relaxed);
                }
                if let Err(e) = fs::remove_file(&local_path) {
                    error!("Failed to remove staged file {:?}: {:?}", local_path, e);
                }
            }
        }

        // Clear batch
        batch.clear();
        *current_bytes = 0;

        Ok(())
    }

    pub fn staging_dirs(&self) -> &[PathBuf] {
        &self.staging_dirs
    }

    /// Cache a block of read data on local NVMe, performing eviction if capacity is reached.
    pub fn cache_read_block(&self, block_key: &str, data: &[u8]) -> Result<()> {
        let this = self.clone();
        let block_key = block_key.to_string();
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || {
            let _ = this.cache_read_block_sync(&block_key, &data);
        });
        Ok(())
    }

    fn cache_read_block_sync(&self, block_key: &str, data: &[u8]) -> Result<()> {
        let safe_name = block_key.replace(['/', ':'], "_");
        let idx = get_dir_index(&safe_name, self.staging_dirs.len());
        let target_dir = self.staging_dirs[idx].join("cache");
        let block_path = target_dir.join(format!("block_{}.block", safe_name));

        if block_path.exists() {
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&block_path) {
                let _ = file.set_times(
                    std::fs::FileTimes::new()
                        .set_accessed(std::time::SystemTime::now())
                        .set_modified(std::time::SystemTime::now()),
                );
            }
            return Ok(());
        }

        let new_data_len = data.len() as u64;
        let current = self
            .current_read_cache_bytes
            .load(std::sync::atomic::Ordering::Relaxed);

        if current + new_data_len > self.max_read_bytes {
            // Only perform directory walks for eviction if capacity is exceeded
            let mut total_bytes = 0u64;
            let mut block_files = Vec::new();

            for dir in &self.staging_dirs {
                let cache_dir = dir.join("cache");
                if let Ok(entries) = fs::read_dir(&cache_dir) {
                    for entry in entries.flatten() {
                        if let Ok(meta) = entry.metadata() {
                            if meta.is_file() {
                                let path = entry.path();
                                if path.extension().is_some_and(|ext| ext == "block") {
                                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                                        if name.starts_with("block_") {
                                            total_bytes += meta.len();
                                            let time = meta
                                                .accessed()
                                                .or_else(|_| meta.modified())
                                                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                                            block_files.push((path, meta.len(), time));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if total_bytes + new_data_len > self.max_read_bytes {
                // Sort block files by accessed time (oldest first)
                block_files.sort_by_key(|&(_, _, time)| time);

                let mut freed_bytes = 0u64;
                // Amortize eviction overhead: free needed space + extra margin (10% of max capacity, up to 100MB)
                let extra_margin = std::cmp::min(100 * 1024 * 1024, self.max_read_bytes / 10);
                let target_to_free =
                    (total_bytes + new_data_len).saturating_sub(self.max_read_bytes) + extra_margin;

                for (path, len, _) in block_files {
                    if freed_bytes >= target_to_free {
                        break;
                    }
                    if fs::remove_file(&path).is_ok() {
                        freed_bytes += len;
                        debug!("NVMe Staging: Evicted block cache file {:?}", path);
                    }
                }

                if total_bytes - freed_bytes + new_data_len > self.max_read_bytes {
                    warn!(
                        "NVMe Staging: Cannot cache block {} - local storage full of staging writes.",
                        block_key
                    );
                    // Synchronize current_read_cache_bytes with actual remaining usage
                    self.current_read_cache_bytes.store(
                        total_bytes - freed_bytes,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    return Ok(()); // Fail silently as caching is opportunistic
                }

                // Synchronize current_read_cache_bytes with actual remaining usage
                self.current_read_cache_bytes.store(
                    total_bytes - freed_bytes,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }

        let tmp_path = target_dir.join(format!("block_{}.{}.block.tmp", safe_name, Uuid::new_v4()));

        fs::write(&tmp_path, data)?;
        if let Err(e) = fs::rename(&tmp_path, &block_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(SqueezefsError::Io(e));
        }
        self.current_read_cache_bytes
            .fetch_add(new_data_len, std::sync::atomic::Ordering::Relaxed);
        debug!(
            "NVMe Staging: Cached block {} -> {:?}",
            block_key, block_path
        );

        if let Some(p2p_addr) = self.p2p_addr.get() {
            let redis_client = self.redis_client.clone();
            let block_key = block_key.to_string();
            let p2p_addr = p2p_addr.clone();
            tokio::spawn(async move {
                if let Ok(mut con) = redis_client.get_connection().await {
                    let safe_name = block_key.replace(['/', ':'], "_");
                    let peer_key = format!("block_peers:{}", safe_name);
                    let _: std::result::Result<(), redis::RedisError> = redis::pipe()
                        .sadd(&peer_key, &p2p_addr)
                        .expire(&peer_key, 60)
                        .query_async(&mut con)
                        .await;
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
        self.current_read_cache_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn read_cached_block(&self, block_key: &str) -> Option<Vec<u8>> {
        self.get_cached_read_block(block_key)
    }

    /// Retrieve a cached block file if it exists locally.
    pub fn get_cached_read_block(&self, block_key: &str) -> Option<Vec<u8>> {
        let safe_name = block_key.replace(['/', ':'], "_");
        let idx = get_dir_index(&safe_name, self.staging_dirs.len());
        let target_dir = self.staging_dirs[idx].join("cache");
        let block_path = target_dir.join(format!("block_{}.block", safe_name));
        if block_path.exists() {
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&block_path) {
                let _ = file.set_times(
                    std::fs::FileTimes::new()
                        .set_accessed(std::time::SystemTime::now())
                        .set_modified(std::time::SystemTime::now()),
                );
            }
            fs::read(block_path).ok()
        } else {
            None
        }
    }

    /// Retrieve a range of bytes from a cached block file if it exists locally.
    pub fn get_cached_read_block_range(
        &self,
        block_key: &str,
        offset: u64,
        size: u32,
    ) -> Option<Vec<u8>> {
        let safe_name = block_key.replace(['/', ':'], "_");
        let idx = get_dir_index(&safe_name, self.staging_dirs.len());
        let target_dir = self.staging_dirs[idx].join("cache");
        let block_path = target_dir.join(format!("block_{}.block", safe_name));
        if block_path.exists() {
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&block_path) {
                let _ = file.set_times(
                    std::fs::FileTimes::new()
                        .set_accessed(std::time::SystemTime::now())
                        .set_modified(std::time::SystemTime::now()),
                );
            }
            use std::io::{Read, Seek, SeekFrom};
            let mut file = fs::File::open(&block_path).ok()?;
            let metadata = file.metadata().ok()?;
            let file_len = metadata.len();

            if offset >= file_len {
                return Some(Vec::new());
            }
            let read_len = std::cmp::min(size as u64, file_len - offset) as usize;

            file.seek(SeekFrom::Start(offset)).ok()?;
            let mut buf = vec![0u8; read_len];
            file.read_exact(&mut buf).ok()?;
            Some(buf)
        } else {
            None
        }
    }

    pub fn list_staged_files(&self) -> Vec<String> {
        let mut files = Vec::new();
        for dir in &self.staging_dirs {
            let staging_dir = dir.join("staging");
            if let Ok(entries) = std::fs::read_dir(&staging_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && path.extension().is_some_and(|ext| ext == "staged") {
                        if let Some(stem) = path.file_stem() {
                            let stem_str = stem.to_string_lossy();
                            if stem_str.starts_with("file_") {
                                files.push(stem_str.trim_start_matches("file_").to_string());
                            }
                        }
                    }
                }
            }
        }
        files
    }

    pub fn list_cached_blocks(&self) -> Vec<String> {
        let mut blocks = Vec::new();
        for dir in &self.staging_dirs {
            let cache_dir = dir.join("cache");
            if let Ok(entries) = std::fs::read_dir(&cache_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && path.extension().is_some_and(|ext| ext == "block") {
                        if let Some(stem) = path.file_stem() {
                            let stem_str = stem.to_string_lossy();
                            if stem_str.starts_with("block_") {
                                blocks.push(stem_str.trim_start_matches("block_").to_string());
                            }
                        }
                    }
                }
            }
        }
        blocks
    }

    pub fn max_write_bytes(&self) -> u64 {
        self.max_write_bytes
    }

    pub fn max_read_bytes(&self) -> u64 {
        self.max_read_bytes
    }
}
