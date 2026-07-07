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
    sync: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct JournalState {
    head: u64,
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
    pub async fn write_record(
        &self,
        storage: &MetaLvStorage,
        record: &[u8],
        sync: bool,
    ) -> Result<()> {
        let inner = self.get_inner(storage).await;
        let (tx, rx) = oneshot::channel();
        inner
            .tx
            .send(JournalRequest {
                record: record.to_vec(),
                tx,
                sync,
            })
            .await
            .map_err(|_| {
                SqueezefsError::InvalidOperation("Journal worker channel closed".to_string())
            })?;
        rx.await.map_err(|_| {
            SqueezefsError::InvalidOperation("Journal worker task panicked".to_string())
        })?
    }
}

/// Encode one WAL record: `len (LE u32) | payload | xxh3_64(payload) LE 8B`,
/// zero-padded to sector alignment.
///
/// The 8-byte trailer is an **error-detection** checksum (torn/corrupt
/// record detection), not a security boundary — anyone who can forge WAL
/// records can write the inode table directly. It was SHA-256 truncated to
/// 8 bytes: all of the cryptographic cost (3.7% of daemon cycles under
/// delete storms) for none of the cryptographic strength. `xxh3_64` has the
/// same 64-bit detection power at a fraction of the cost. Records are
/// currently write-only (replay was deleted as unsound in PR 8; the state
/// sector persists tail == head), so the trailer bytes are format-neutral —
/// pinned by `test_encode_record_layout` for any future recovery reader.
fn encode_record(payload: &[u8]) -> Vec<u8> {
    let payload_len = payload.len() as u32;
    let raw_len = 4 + payload.len() + 8;
    let padded_len = (raw_len + SECTOR_SIZE - 1) & !(SECTOR_SIZE - 1);
    let mut record_bytes = Vec::with_capacity(padded_len);
    record_bytes.extend_from_slice(&payload_len.to_le_bytes());
    record_bytes.extend_from_slice(payload);
    record_bytes.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(payload).to_le_bytes());
    record_bytes.resize(padded_len, 0);
    record_bytes
}

async fn journal_worker_loop(
    storage: MetaLvStorage,
    start_offset: u64,
    size: u64,
    mut rx: mpsc::Receiver<JournalRequest>,
) {
    let circular_start = start_offset + SECTOR_SIZE as u64;
    let circular_size = size - SECTOR_SIZE as u64;

    let mut state = JournalState { head: 0 };
    let mut state_sector = [0u8; SECTOR_SIZE];
    if storage
        .read_blocks_direct(start_offset, &mut state_sector)
        .await
        .is_ok()
    {
        if &state_sector[0..8] == b"LVJOURNL" {
            state.head = u64::from_le_bytes(state_sector[8..16].try_into().unwrap());
        }
    }

    // Default 50ms deferred fdatasync for max create/write throughput under
    // same-dir storms. Set to 0 for sync-on-commit durability.
    // FUSE fsync/fsyncdir still force a full barrier via FORCE_SYNC_TX / sync_all_devices.
    let flush_interval_ms = std::env::var("SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS")
        .ok()
        .and_then(|val| val.parse::<u64>().ok())
        .unwrap_or(50);

    let needs_flush = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    if flush_interval_ms > 0 {
        let needs_flush_clone = needs_flush.clone();
        let device_path = storage.device_path().to_path_buf();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(flush_interval_ms));
            loop {
                interval.tick().await;
                if std::sync::Arc::strong_count(&needs_flush_clone) <= 1 {
                    break;
                }
                if needs_flush_clone.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    let _ = crate::uring_fs::fdatasync(&device_path).await;
                }
            }
        });
    }

    while let Some(first_req) = rx.recv().await {
        let mut reqs = vec![first_req];
        while reqs.len() < 32 {
            match rx.try_recv() {
                Ok(r) => reqs.push(r),
                Err(_) => break,
            }
        }
        // §Observability (PR 7): batch fill per drain — headroom before the
        // single WAL worker becomes the commit bottleneck.
        crate::fuse_client::METRICS
            .meta_wal_batch_size
            .record(reqs.len());

        let mut write_failed = false;
        let mut force_sync = false;
        // Build every record in the drain into ONE batched uring-fs write:
        // record boundaries stay sector-aligned (encode_record pads), so the
        // circular wrap splits are themselves aligned batch entries.
        let mut batch_ops: Vec<(u64, bytes::Bytes)> = Vec::with_capacity(reqs.len() + 1);
        for req in &reqs {
            if req.sync {
                force_sync = true;
            }
            let record_bytes = encode_record(&req.record);
            let record_len = record_bytes.len() as u64;
            let record = bytes::Bytes::from(record_bytes);
            let mut cur = state.head % circular_size;
            let mut off = 0usize;
            while off < record.len() {
                let chunk = std::cmp::min(record.len() - off, (circular_size - cur) as usize);
                batch_ops.push((circular_start + cur, record.slice(off..off + chunk)));
                off += chunk;
                cur = (cur + chunk as u64) % circular_size;
            }
            state.head = (state.head + record_len) % circular_size;
        }
        if storage.write_blocks_direct_batch(batch_ops).await.is_err() {
            write_failed = true;
        }

        if !write_failed {
            // State-sector format is unchanged (magic + head + tail) so pre-PR-8
            // binaries still mount. `tail` is persisted equal to `head`
            // ("journal drained"): commits apply their sectors in place before
            // the WAL record is ever needed again, and the deleted (unsound)
            // replay was the only tail consumer — an old binary's replay now
            // correctly sees an empty journal instead of re-applying stale
            // records over newer data (design review Issue 15).
            state_sector[0..8].copy_from_slice(b"LVJOURNL");
            state_sector[8..16].copy_from_slice(&state.head.to_le_bytes());
            state_sector[16..24].copy_from_slice(&state.head.to_le_bytes());
            if storage
                .write_blocks_direct(start_offset, &state_sector)
                .await
                .is_err()
            {
                write_failed = true;
            }
        }

        if !write_failed {
            if force_sync || flush_interval_ms == 0 {
                if crate::uring_fs::fdatasync(storage.device_path())
                    .await
                    .is_err()
                {
                    write_failed = true;
                }
            } else {
                needs_flush.store(true, std::sync::atomic::Ordering::SeqCst);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the WAL record layout for any future recovery reader:
    /// `len (LE u32) | payload | xxh3_64(payload) LE 8B | zero pad to 4096`.
    #[test]
    fn test_encode_record_layout() {
        let payload = b"squeezefs wal record payload".as_slice();
        let rec = encode_record(payload);

        assert_eq!(rec.len() % SECTOR_SIZE, 0, "record must be sector-padded");
        assert_eq!(
            u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize,
            payload.len(),
            "length prefix"
        );
        assert_eq!(&rec[4..4 + payload.len()], payload, "payload bytes");
        let trailer_at = 4 + payload.len();
        assert_eq!(
            u64::from_le_bytes(rec[trailer_at..trailer_at + 8].try_into().unwrap()),
            xxhash_rust::xxh3::xxh3_64(payload),
            "xxh3_64 error-detection trailer"
        );
        assert!(
            rec[trailer_at + 8..].iter().all(|&b| b == 0),
            "padding must be zeroed"
        );
    }

    /// A record whose raw size is exactly sector-aligned gains no extra pad.
    #[test]
    fn test_encode_record_exact_sector_fit() {
        let payload = vec![0x7Fu8; SECTOR_SIZE - 4 - 8];
        let rec = encode_record(&payload);
        assert_eq!(rec.len(), SECTOR_SIZE, "exact fit must stay one sector");
    }
}
