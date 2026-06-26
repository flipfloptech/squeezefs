use crate::backend::RustFsClient;
use crate::error::{Result, SqueezefsError};
use bytes::Bytes;
use log::{debug, error, info, warn};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use uuid::Uuid;
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

#[derive(Clone)]
pub struct NvmeStaging {
    staging_dirs: Vec<PathBuf>,
    max_write_bytes: u64,
    max_read_bytes: u64,
    pub backend: RustFsClient,
    redis_client: crate::dlm::MetaClient,
    write_tx: mpsc::Sender<PendingStagedWrite>,
    pub p2p_addr: std::sync::Arc<std::sync::OnceLock<String>>,
    pub current_staged_write_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub space_freed_notify: std::sync::Arc<tokio::sync::Notify>,

    // Hypertier NVMe cache instances
    pub read_nvme_cache: std::sync::Arc<hypertier::nvme::NvmeCache>,
    pub staging_nvme_cache: std::sync::Arc<hypertier::nvme::NvmeCache>,
    pub dht_node: std::sync::Arc<std::sync::OnceLock<std::sync::Arc<hypertier::dht::DhtNode>>>,
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
        backend: RustFsClient,
        redis_client: crate::dlm::MetaClient,
    ) -> Result<Self> {
        if staging_dirs.is_empty() {
            return Err(SqueezefsError::InvalidOperation(
                "At least one staging directory must be specified".to_string(),
            ));
        }

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
        let read_cap = actual_max_read_bytes as usize / staging_dirs.len();
        let read_capacities = vec![read_cap; staging_dirs.len()];
        let read_nvme_cache = Arc::new(hypertier::nvme::NvmeCache::new(
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
        let write_cap = actual_max_write_bytes as usize / staging_dirs.len();
        let write_capacities = vec![write_cap; staging_dirs.len()];
        let staging_nvme_cache = Arc::new(hypertier::nvme::NvmeCache::new(
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

        let (write_tx, write_rx) = mpsc::channel::<PendingStagedWrite>(1000);

        let staging = Self {
            staging_dirs: staging_dirs.clone(),
            max_write_bytes: actual_max_write_bytes,
            max_read_bytes: actual_max_read_bytes,
            backend: backend.clone(),
            redis_client: redis_client.clone(),
            write_tx,
            p2p_addr: std::sync::Arc::new(std::sync::OnceLock::new()),
            current_staged_write_bytes: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                initial_write_bytes,
            )),
            space_freed_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            read_nvme_cache,
            staging_nvme_cache,
            dht_node: std::sync::Arc::new(std::sync::OnceLock::new()),
        };

        // Spawn background merge worker
        staging.start_merge_worker(write_rx);

        Ok(staging)
    }

    pub fn redis_client(&self) -> &crate::dlm::MetaClient {
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
        let meta_content = serde_json::json!({
            "file_path": file_path,
            "fencing_token": fencing_token,
            "original_size": data.len()
        });
        let meta_json_bytes = serde_json::to_vec(&meta_content).unwrap();
        let unpadded_len = 8 + meta_json_bytes.len() + data.len();
        let padded_size = unpadded_len as u64;

        let target_dir = self.staging_dirs.first().ok_or_else(|| {
            SqueezefsError::InvalidOperation("No staging directories configured".to_string())
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
        let meta_len = meta_json_bytes.len() as u64;

        let mut packed_payload = Vec::with_capacity(unpadded_len);
        packed_payload.extend_from_slice(&meta_len.to_be_bytes());
        packed_payload.extend_from_slice(&meta_json_bytes);
        packed_payload.extend_from_slice(data);

        let payload_bytes = Bytes::from(packed_payload);

        // Memory-mapped copy (lock-free, zero disk syscall wait)
        self.staging_nvme_cache.put(key_bytes, payload_bytes);

        self.current_staged_write_bytes
            .fetch_add(padded_size, std::sync::atomic::Ordering::Relaxed);

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

        self.write_tx.send(pending).await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::other(format!(
                "Failed to notify merge worker: {:?}",
                e
            )))
        })?;

        Ok(())
    }

    /// Put a packed active block write to staging_nvme_cache.
    pub fn put_active_block(&self, key: &str, data: &[u8], fencing_token: u64) {
        let meta_content = serde_json::json!({
            "file_path": key,
            "fencing_token": fencing_token,
            "original_size": data.len()
        });
        let meta_json_bytes = serde_json::to_vec(&meta_content).unwrap();
        let unpadded_len = 8 + meta_json_bytes.len() + data.len();
        let mut packed_payload = Vec::with_capacity(unpadded_len);
        let meta_len = meta_json_bytes.len() as u64;
        packed_payload.extend_from_slice(&meta_len.to_be_bytes());
        packed_payload.extend_from_slice(&meta_json_bytes);
        packed_payload.extend_from_slice(data);

        let key_bytes = Bytes::copy_from_slice(key.as_bytes());
        let payload_bytes = Bytes::from(packed_payload);

        self.staging_nvme_cache.put(key_bytes, payload_bytes);
    }

    /// Remove a packed active block write from staging_nvme_cache.
    pub fn remove_active_block(&self, key: &str) -> Option<Vec<u8>> {
        let val = self.read_staged(key);
        let key_bytes = Bytes::copy_from_slice(key.as_bytes());
        self.staging_nvme_cache.remove(&key_bytes);
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
                if let Ok(meta_json) =
                    serde_json::from_slice::<serde_json::Value>(&bytes[8..8 + meta_len])
                {
                    if let Some(orig_size) = meta_json.get("original_size").and_then(|v| v.as_u64())
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
        None
    }

    /// Read staged data zero-copy directly from staging_nvme_cache memory-mapped segments.
    pub fn read_staged_zero_copy(
        &self,
        file_id: &str,
    ) -> Option<hypertier::nvme::NvmeCacheReadGuard> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let mut guard = self.staging_nvme_cache.get_static(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() >= 8 {
            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() >= 8 + meta_len {
                if let Ok(meta_json) =
                    serde_json::from_slice::<serde_json::Value>(&bytes[8..8 + meta_len])
                {
                    if let Some(orig_size) = meta_json.get("original_size").and_then(|v| v.as_u64())
                    {
                        let data_start = 8 + meta_len;
                        let data_end = data_start + orig_size as usize;
                        if bytes.len() >= data_end {
                            guard.offset += data_start;
                            guard.len = orig_size as usize;
                            return Some(guard);
                        }
                    }
                }
            }
        }
        None
    }

    fn start_merge_worker(&self, mut write_rx: mpsc::Receiver<PendingStagedWrite>) {
        let backend = self.backend.clone();
        let redis_client = self.redis_client.clone();
        let staged_bytes = self.current_staged_write_bytes.clone();
        let space_freed_notify = self.space_freed_notify.clone();
        let staging_nvme_cache = self.staging_nvme_cache.clone();

        tokio::spawn(async move {
            let mut batch: Vec<PendingStagedWrite> = Vec::new();
            let mut current_bytes = 0u64;
            let max_batch_bytes = 4 * 1024 * 1024;
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
                        current_bytes += pending.padded_size;
                        batch.push(pending);

                        if current_bytes >= max_batch_bytes {
                            info!("NVMe Staging: Batch size threshold reached ({} bytes). Flushing merged block.", current_bytes);
                            if let Err(e) = Self::flush_batch(&staging_nvme_cache, &backend, &redis_client, &mut batch, &mut current_bytes, &staged_bytes, &space_freed_notify).await {
                                error!("Failed to flush NVMe staging batch: {:?}", e);
                            }
                        }
                    }
                    _ = &mut sleep => {
                        if !batch.is_empty() {
                            info!("NVMe Staging: Timeout reached. Flushing merged block with {} pending writes.", batch.len());
                            if let Err(e) = Self::flush_batch(&staging_nvme_cache, &backend, &redis_client, &mut batch, &mut current_bytes, &staged_bytes, &space_freed_notify).await {
                                error!("Failed to flush NVMe staging batch on timeout: {:?}", e);
                            }
                        }
                    }
                }
            }
        });
    }

    async fn flush_batch(
        staging_nvme_cache: &hypertier::nvme::NvmeCache,
        backend: &RustFsClient,
        redis_client: &crate::dlm::MetaClient,
        batch: &mut Vec<PendingStagedWrite>,
        current_bytes: &mut u64,
        staged_bytes: &std::sync::Arc<std::sync::atomic::AtomicU64>,
        space_freed_notify: &tokio::sync::Notify,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        crate::coz_progress!("nvme_flush_batch");

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
        let mut mappings = Vec::new();
        let mut highest_fencing_token = 0u64;

        let mut total_logical_size = 0usize;
        for item in batch.iter() {
            let key_bytes = Bytes::copy_from_slice(item.file_id.as_bytes());
            let data_res = if let Some(guard) = staging_nvme_cache.get(&key_bytes) {
                let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
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
                }
            } else {
                None
            };

            if let Some(data) = data_res {
                total_logical_size += data.len();
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
        info!("NVMe Staging: Uploading packed block {} (size {} bytes) to RustFS with fencing token {}.", packed_key, packed_payload_len, highest_fencing_token);
        backend
            .put_object(
                &packed_key,
                bytes::Bytes::from(packed_payload),
                highest_fencing_token,
            )
            .await?;

        if let Ok(mut con) = redis_client.get_connection().await {
            let refcounts_key = "squeezefs:block_refcounts";
            let _: std::result::Result<(), redis::RedisError> = redis::cmd("HSET")
                .arg(refcounts_key)
                .arg(&packed_key)
                .arg(mappings.len() as i32)
                .query_async(&mut con)
                .await;

            let _: std::result::Result<(), redis::RedisError> = redis::cmd("HSET")
                .arg("squeezefs:block_sizes")
                .arg(&packed_key)
                .arg(format!("{}:{}", total_logical_size, packed_payload_len))
                .query_async(&mut con)
                .await;

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

        for item in batch.iter() {
            let key_bytes = Bytes::copy_from_slice(item.file_id.as_bytes());
            staging_nvme_cache.remove(&key_bytes);
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
    pub fn cache_read_block(&self, block_key: &str, data: &[u8]) -> Result<()> {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let val_bytes = Bytes::copy_from_slice(data);
        self.read_nvme_cache
            .put(key_bytes.clone(), val_bytes.clone());

        if let Some(dht) = self.dht_node.get() {
            let dht_clone = dht.clone();
            let key_hash = xxh3_64(block_key.as_bytes());
            tokio::spawn(async move {
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

        if let Some(p2p_addr) = self.p2p_addr.get() {
            let redis_client = self.redis_client.clone();
            let block_key = block_key.to_string();
            let p2p_addr = p2p_addr.clone();
            tokio::spawn(async move {
                if let Ok(mut con) = redis_client.get_connection().await {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let safe_name = block_key.replace(['/', ':'], "_");
                    let peer_key = format!("block_peers:{}", safe_name);
                    let peer_blocks_key = format!("squeezefs:peer_blocks:{}", p2p_addr);
                    let _: std::result::Result<(), redis::RedisError> = redis::pipe()
                        .sadd(&peer_key, &p2p_addr)
                        .expire(&peer_key, 60)
                        .sadd(&peer_blocks_key, &block_key)
                        .cmd("ZADD")
                        .arg("squeezefs:peer_health_check_schedule")
                        .arg(now + 60)
                        .arg(&p2p_addr)
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
    ) -> Option<hypertier::nvme::NvmeCacheReadGuard> {
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
