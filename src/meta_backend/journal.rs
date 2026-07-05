use crate::error::{Result, SqueezefsError};
use crate::meta_backend::storage::{MetaLvStorage, SECTOR_SIZE};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, OnceCell};

pub struct Journal {
    pub start_offset: u64,
    pub size: u64,
    inner: OnceCell<Arc<JournalInner>>,
}

struct JournalInner {
    tx: mpsc::Sender<JournalRequest>,
}

struct JournalRequest {
    record: Vec<u8>,
    tx: oneshot::Sender<Result<()>>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct JournalState {
    head: u64,
    tail: u64,
}

impl Journal {
    pub fn new(start_offset: u64, size: u64) -> Self {
        Self {
            start_offset,
            size,
            inner: OnceCell::new(),
        }
    }

    async fn get_inner(&self, storage: &MetaLvStorage) -> &Arc<JournalInner> {
        self.inner
            .get_or_init(|| {
                let (tx, rx) = mpsc::channel(1024);
                let storage_clone = storage.clone();
                let start_offset = self.start_offset;
                let size = self.size;
                tokio::spawn(async move {
                    journal_worker_loop(storage_clone, start_offset, size, rx).await;
                });
                async { Arc::new(JournalInner { tx }) }
            })
            .await
    }

    /// Write a redo log record to the circular journal
    pub async fn write_record(&self, storage: &MetaLvStorage, record: &[u8]) -> Result<()> {
        let inner = self.get_inner(storage).await;
        let (tx, rx) = oneshot::channel();
        inner
            .tx
            .send(JournalRequest {
                record: record.to_vec(),
                tx,
            })
            .await
            .map_err(|_| {
                SqueezefsError::InvalidOperation("Journal worker channel closed".to_string())
            })?;
        rx.await.map_err(|_| {
            SqueezefsError::InvalidOperation("Journal worker task panicked".to_string())
        })?
    }

    /// Replays outstanding log entries on mount/recovery
    pub async fn replay(&self, storage: &MetaLvStorage) -> Result<()> {
        let mut state_sector = [0u8; SECTOR_SIZE];
        if let Err(e) = storage
            .read_blocks_direct(self.start_offset, &mut state_sector)
            .await
        {
            log::info!(
                "Journal: No valid superblock found (read failed: {:?}). Skipping replay.",
                e
            );
            return Ok(());
        }
        if &state_sector[0..8] != b"LVJOURNL" {
            log::info!("Journal: Magic not found. Skipping replay.");
            return Ok(());
        }
        let head = u64::from_le_bytes(state_sector[8..16].try_into().unwrap());
        let tail = u64::from_le_bytes(state_sector[16..24].try_into().unwrap());
        log::info!("Journal replay started: tail = {}, head = {}", tail, head);

        if tail == head {
            log::info!("Journal is empty. No replay needed.");
            return Ok(());
        }

        let circular_start = self.start_offset + SECTOR_SIZE as u64;
        let circular_size = self.size - SECTOR_SIZE as u64;

        let mut current_pos = tail;
        while current_pos != head {
            // 1. Read the first sector of the record
            let mut first_sector = [0u8; SECTOR_SIZE];
            if let Err(e) = read_circular(
                storage,
                circular_start,
                circular_size,
                current_pos,
                &mut first_sector,
            )
            .await
            {
                log::warn!("Journal replay: read first sector failed: {:?}", e);
                break;
            }

            let payload_len = u32::from_le_bytes(first_sector[0..4].try_into().unwrap()) as u64;
            if payload_len == 0 || payload_len > 20 * 1024 * 1024 {
                break;
            }

            let raw_len = 4 + payload_len + 8;
            let padded_len = (raw_len + SECTOR_SIZE as u64 - 1) & !(SECTOR_SIZE as u64 - 1);

            let mut record_data = vec![0u8; padded_len as usize];
            record_data[..SECTOR_SIZE].copy_from_slice(&first_sector);

            if padded_len > SECTOR_SIZE as u64 {
                let mut remaining_buf = vec![0u8; (padded_len - SECTOR_SIZE as u64) as usize];
                if let Err(e) = read_circular(
                    storage,
                    circular_start,
                    circular_size,
                    current_pos + SECTOR_SIZE as u64,
                    &mut remaining_buf,
                )
                .await
                {
                    log::warn!("Journal replay: read remaining sectors failed: {:?}", e);
                    break;
                }
                record_data[SECTOR_SIZE..].copy_from_slice(&remaining_buf);
            }

            let payload = &record_data[4..4 + payload_len as usize];
            let expected_checksum =
                &record_data[4 + payload_len as usize..4 + payload_len as usize + 8];

            use sha2::{Digest, Sha256};
            let hash = Sha256::digest(payload);
            if &hash[..8] != expected_checksum {
                log::warn!("Journal replay: checksum verification failed. Stop replay.");
                break;
            }

            // 3. Deserialize and apply writes
            let ops: Vec<(u64, Vec<u8>)> = match bincode::deserialize(payload) {
                Ok(o) => o,
                Err(e) => {
                    log::warn!("Journal replay: failed to deserialize ops: {:?}", e);
                    break;
                }
            };

            log::info!("Journal replay: reapplying {} write operations", ops.len());
            for (offset, buf) in ops {
                if let Err(e) = storage.write_blocks_direct(offset, &buf).await {
                    log::error!("Journal replay error applying write at {}: {:?}", offset, e);
                    return Err(e);
                }
            }

            current_pos = (current_pos + padded_len) % circular_size;
        }

        // Reset journal state sector
        state_sector[0..8].copy_from_slice(b"LVJOURNL");
        state_sector[8..16].copy_from_slice(&head.to_le_bytes());
        state_sector[16..24].copy_from_slice(&head.to_le_bytes());
        storage
            .write_blocks_direct(self.start_offset, &state_sector)
            .await?;
        crate::uring_fs::fdatasync(storage.device_path()).await?;
        log::info!(
            "Journal replay complete. Reset tail to match head at {}",
            head
        );
        Ok(())
    }
}

async fn read_circular(
    storage: &MetaLvStorage,
    circular_start: u64,
    circular_size: u64,
    pos: u64,
    buf: &mut [u8],
) -> Result<()> {
    let mut read_len = 0;
    let mut cur_pos = pos % circular_size;
    while read_len < buf.len() {
        let chunk_len = std::cmp::min(buf.len() - read_len, (circular_size - cur_pos) as usize);
        let mut temp_buf = vec![0u8; chunk_len];
        storage
            .read_blocks_direct(circular_start + cur_pos, &mut temp_buf)
            .await?;
        buf[read_len..read_len + chunk_len].copy_from_slice(&temp_buf);
        read_len += chunk_len;
        cur_pos = (cur_pos + chunk_len as u64) % circular_size;
    }
    Ok(())
}

async fn write_circular(
    storage: &MetaLvStorage,
    circular_start: u64,
    circular_size: u64,
    pos: u64,
    data: &[u8],
) -> Result<()> {
    let mut written_len = 0;
    let mut cur_pos = pos % circular_size;
    while written_len < data.len() {
        let chunk_len = std::cmp::min(data.len() - written_len, (circular_size - cur_pos) as usize);
        storage
            .write_blocks_direct(
                circular_start + cur_pos,
                &data[written_len..written_len + chunk_len],
            )
            .await?;
        written_len += chunk_len;
        cur_pos = (cur_pos + chunk_len as u64) % circular_size;
    }
    Ok(())
}

async fn journal_worker_loop(
    storage: MetaLvStorage,
    start_offset: u64,
    size: u64,
    mut rx: mpsc::Receiver<JournalRequest>,
) {
    let circular_start = start_offset + SECTOR_SIZE as u64;
    let circular_size = size - SECTOR_SIZE as u64;

    let mut state = JournalState { head: 0, tail: 0 };
    let mut state_sector = [0u8; SECTOR_SIZE];
    if storage
        .read_blocks_direct(start_offset, &mut state_sector)
        .await
        .is_ok()
    {
        if &state_sector[0..8] == b"LVJOURNL" {
            state.head = u64::from_le_bytes(state_sector[8..16].try_into().unwrap());
            state.tail = u64::from_le_bytes(state_sector[16..24].try_into().unwrap());
        }
    }

    while let Some(first_req) = rx.recv().await {
        let mut reqs = vec![first_req];
        while reqs.len() < 32 {
            match rx.try_recv() {
                Ok(r) => reqs.push(r),
                Err(_) => break,
            }
        }

        let mut write_failed = false;
        for req in &reqs {
            let payload_len = req.record.len() as u32;
            let mut record_bytes = Vec::with_capacity(4 + req.record.len() + 8);
            record_bytes.extend_from_slice(&payload_len.to_le_bytes());
            record_bytes.extend_from_slice(&req.record);
            use sha2::{Digest, Sha256};
            let hash = Sha256::digest(&req.record);
            record_bytes.extend_from_slice(&hash[..8]);

            // Pad record to sector alignment (4096 bytes)
            let raw_len = record_bytes.len();
            let padded_len = (raw_len + SECTOR_SIZE - 1) & !(SECTOR_SIZE - 1);
            record_bytes.resize(padded_len, 0);

            let start_pos = state.head;
            if write_circular(
                &storage,
                circular_start,
                circular_size,
                start_pos,
                &record_bytes,
            )
            .await
            .is_err()
            {
                write_failed = true;
                break;
            }
            state.head = (state.head + padded_len as u64) % circular_size;
        }

        if !write_failed {
            state_sector[0..8].copy_from_slice(b"LVJOURNL");
            state_sector[8..16].copy_from_slice(&state.head.to_le_bytes());
            state_sector[16..24].copy_from_slice(&state.tail.to_le_bytes());
            if storage
                .write_blocks_direct(start_offset, &state_sector)
                .await
                .is_err()
            {
                write_failed = true;
            }
        }

        if !write_failed {
            if crate::uring_fs::fdatasync(storage.device_path())
                .await
                .is_err()
            {
                write_failed = true;
            }
        }

        for req in reqs {
            let res = if write_failed {
                Err(SqueezefsError::InvalidOperation(
                    "WAL journal write failed".to_string(),
                ))
            } else {
                Ok(())
            };
            let _ = req.tx.send(res);
        }
    }
}
