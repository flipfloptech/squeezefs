use crate::backend::RustFsClient;
use crate::error::{Result, SqueezefsError};
use log::{debug, error, info, warn};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use uuid::Uuid;

#[derive(Clone)]
pub struct NvmeStaging {
    staging_dir: PathBuf,
    backend: RustFsClient,
    redis_client: redis::Client,
    write_tx: mpsc::Sender<PendingStagedWrite>,
}

#[derive(Debug)]
pub struct PendingStagedWrite {
    pub file_path: String,
    pub file_id: String,
    pub fencing_token: u64,
}

impl NvmeStaging {
    pub fn new(
        staging_dir: PathBuf,
        backend: RustFsClient,
        redis_client: redis::Client,
    ) -> Result<Self> {
        // Ensure staging directory exists
        if !staging_dir.exists() {
            fs::create_dir_all(&staging_dir)?;
        }

        let (write_tx, write_rx) = mpsc::channel::<PendingStagedWrite>(1000);

        let staging = Self {
            staging_dir: staging_dir.clone(),
            backend: backend.clone(),
            redis_client: redis_client.clone(),
            write_tx,
        };

        // Spawn the background merge worker
        staging.start_merge_worker(write_rx);

        Ok(staging)
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
        let data_path = self.staging_dir.join(format!("{}.data", file_id));
        let meta_path = self.staging_dir.join(format!("{}.meta", file_id));

        // Write the data and metadata to NVMe staging synchronously
        fs::write(&data_path, data)?;

        let meta_content = serde_json::json!({
            "file_path": file_path,
            "fencing_token": fencing_token
        });
        fs::write(&meta_path, serde_json::to_vec(&meta_content).unwrap())?;

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
        let data_path = self.staging_dir.join(format!("{}.data", file_id));
        if data_path.exists() {
            fs::read(data_path).ok()
        } else {
            None
        }
    }

    /// Start background merge worker.
    fn start_merge_worker(&self, mut write_rx: mpsc::Receiver<PendingStagedWrite>) {
        let staging_dir = self.staging_dir.clone();
        let backend = self.backend.clone();
        let redis_client = self.redis_client.clone();

        tokio::spawn(async move {
            let mut batch: Vec<PendingStagedWrite> = Vec::new();
            let mut current_bytes = 0u64;
            let max_batch_bytes = 4 * 1024 * 1024; // 4MB
            let flush_timeout = Duration::from_millis(500);

            loop {
                let sleep = time::sleep(flush_timeout);
                tokio::pin!(sleep);

                tokio::select! {
                    Some(pending) = write_rx.recv() => {
                        let local_path = staging_dir.join(format!("{}.data", pending.file_id));
                        if let Ok(metadata) = fs::metadata(&local_path) {
                            current_bytes += metadata.len();
                            batch.push(pending);
                        }

                        if current_bytes >= max_batch_bytes {
                            info!("NVMe Staging: Batch size threshold reached ({} bytes). Flushing merged block.", current_bytes);
                            if let Err(e) = Self::flush_batch(&staging_dir, &backend, &redis_client, &mut batch, &mut current_bytes).await {
                                error!("Failed to flush NVMe staging batch: {:?}", e);
                            }
                        }
                    }
                    _ = &mut sleep => {
                        if !batch.is_empty() {
                            info!("NVMe Staging: Timeout reached. Flushing merged block with {} pending writes.", batch.len());
                            if let Err(e) = Self::flush_batch(&staging_dir, &backend, &redis_client, &mut batch, &mut current_bytes).await {
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
        staging_dir: &Path,
        backend: &RustFsClient,
        redis_client: &redis::Client,
        batch: &mut Vec<PendingStagedWrite>,
        current_bytes: &mut u64,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let packed_id = Uuid::new_v4().to_string();
        let packed_key = format!("packed/blocks/{}", packed_id);

        let mut packed_payload = Vec::new();
        let mut mappings = Vec::new(); // Mappings: (file_id, offset, size)
        let mut highest_fencing_token = 0u64;

        // 1. Pack individual staged file bytes into one payload
        for item in batch.iter() {
            let local_path = staging_dir.join(format!("{}.data", item.file_id));
            if let Ok(data) = fs::read(&local_path) {
                let offset = packed_payload.len() as u64;
                let size = data.len() as u64;
                packed_payload.extend_from_slice(&data);

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
        if let Ok(mut con) = redis_client.get_multiplexed_tokio_connection().await {
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
            let local_path = staging_dir.join(format!("{}.data", item.file_id));
            if local_path.exists() {
                if let Err(e) = fs::remove_file(&local_path) {
                    error!("Failed to remove staged file {:?}: {:?}", local_path, e);
                }
            }
            let meta_path = staging_dir.join(format!("{}.meta", item.file_id));
            if meta_path.exists() {
                if let Err(e) = fs::remove_file(&meta_path) {
                    error!(
                        "Failed to remove staged metadata file {:?}: {:?}",
                        meta_path, e
                    );
                }
            }
        }

        // Clear batch
        batch.clear();
        *current_bytes = 0;

        Ok(())
    }

    pub fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }
}
