use crate::error::Result;
use bytes::Bytes;
use std::sync::Arc;

type EvictReceiver = tokio::sync::mpsc::Receiver<(String, Bytes)>;

#[derive(Clone)]
pub struct LruCache {
    inner: Arc<crate::tiering::memory::MemoryCache>,
    max_bytes: u64,
    evict_tx: tokio::sync::mpsc::Sender<(String, Bytes)>,
    evict_rx: Arc<std::sync::Mutex<Option<EvictReceiver>>>,
}

impl LruCache {
    pub fn new() -> Result<Self> {
        // Query host system memory
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let total_memory = sys.total_memory(); // In bytes

        // Default to 20% of system RAM
        let max_bytes = total_memory / 5;
        Ok(Self::with_capacity(max_bytes))
    }

    /// Construct with a custom memory limit in bytes.
    pub fn with_capacity(max_bytes: u64) -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(16);
        let num_shards = std::cmp::max(cores.next_power_of_two(), 16);

        let mut actual_shards = num_shards;

        let actual_bytes = if max_bytes < 10 * 1024 * 1024 {
            actual_shards = 1;
            max_bytes
        } else {
            #[cfg(test)]
            {
                // In tests, scale down shards if capacity is too small to avoid huge allocations
                while actual_shards > 16 && max_bytes / (actual_shards as u64) < 4 * 1024 * 1024 {
                    actual_shards /= 2;
                }
                max_bytes
            }
            #[cfg(not(test))]
            {
                let min_required = (num_shards * 4 * 1024 * 1024) as u64;
                // In production, never let capacity be smaller than the minimum required
                std::cmp::max(max_bytes, min_required)
            }
        };

        let inner = Arc::new(crate::tiering::memory::MemoryCache::new(
            actual_bytes as usize,
            actual_shards,
        ));
        let (evict_tx, evict_rx) = tokio::sync::mpsc::channel(16384);
        Self {
            inner,
            max_bytes: actual_bytes,
            evict_tx,
            evict_rx: Arc::new(std::sync::Mutex::new(Some(evict_rx))),
        }
    }

    /// Retrieve the eviction receiver. Can only be taken once.
    pub fn take_evict_rx(&self) -> Option<EvictReceiver> {
        self.evict_rx.lock().ok()?.take()
    }

    /// Retrieve an entry from the cache, updating its clock status.
    pub fn get(&self, key: &str) -> Option<Bytes> {
        self.inner.get(key.as_bytes())
    }

    /// Insert an entry into the cache, executing Clock eviction if maximum capacity is exceeded.
    pub fn put(&self, key: &str, data: Bytes) {
        if (data.len() as u64) <= self.max_bytes {
            let key_bytes = Bytes::copy_from_slice(key.as_bytes());
            let evicted = self.inner.put(key_bytes, data);
            for (ek, ev) in evicted {
                if let Ok(k_str) = String::from_utf8(ek.to_vec()) {
                    let _ = self.evict_tx.try_send((k_str, ev));
                }
            }
        } else {
            // Do not leave a smaller stale entry under this key when the new
            // payload exceeds the cache budget (e.g. layout growth RMW).
            self.inner.remove(key.as_bytes());
        }
    }

    pub fn remove(&self, key: &str) {
        self.inner.remove(key.as_bytes());
    }

    pub fn current_bytes(&self) -> u64 {
        self.inner.current_bytes() as u64
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub fn keys(&self) -> Vec<String> {
        self.inner
            .keys()
            .into_iter()
            .filter_map(|k| String::from_utf8(k.to_vec()).ok())
            .collect()
    }

    pub fn run_pending_tasks(&self) {
        // MemoryCache operations are immediate, so this is a no-op kept for API compatibility.
    }

    pub fn clear(&self) {
        self.inner.clear();
    }

    /// Expose the number of shards for testing.
    pub fn num_shards(&self) -> usize {
        self.inner.num_shards()
    }
}
