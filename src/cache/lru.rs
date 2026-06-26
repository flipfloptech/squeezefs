use crate::error::Result;
use bytes::Bytes;
use std::sync::Arc;

type EvictReceiver = tokio::sync::mpsc::UnboundedReceiver<(String, Arc<Vec<u8>>)>;

#[derive(Clone)]
pub struct LruCache {
    inner: Arc<hypertier::memory::MemoryCache>,
    max_bytes: u64,
    evict_tx: tokio::sync::mpsc::UnboundedSender<(String, Arc<Vec<u8>>)>,
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
        // 16 shards for high concurrency lock-free reads, scale down for small capacities
        let num_shards = if max_bytes < 10 * 1024 * 1024 { 1 } else { 16 };
        let inner = Arc::new(hypertier::memory::MemoryCache::new(max_bytes as usize, num_shards));
        let (evict_tx, evict_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            inner,
            max_bytes,
            evict_tx,
            evict_rx: Arc::new(std::sync::Mutex::new(Some(evict_rx))),
        }
    }

    /// Retrieve the eviction receiver. Can only be taken once.
    pub fn take_evict_rx(
        &self,
    ) -> Option<EvictReceiver> {
        self.evict_rx.lock().ok()?.take()
    }

    /// Retrieve an entry from the cache, updating its clock status.
    pub fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        let key_bytes = Bytes::copy_from_slice(key.as_bytes());
        let val = self.inner.get(&key_bytes)?;
        Some(Arc::new(val.to_vec()))
    }

    /// Insert an entry into the cache, executing Clock eviction if maximum capacity is exceeded.
    pub fn put(&self, key: &str, data: Arc<Vec<u8>>) {
        if (data.len() as u64) <= self.max_bytes {
            let key_bytes = Bytes::copy_from_slice(key.as_bytes());
            let val_bytes = Bytes::copy_from_slice(&data);
            let evicted = self.inner.put(key_bytes, val_bytes);
            for (ek, ev) in evicted {
                if let Ok(k_str) = String::from_utf8(ek.to_vec()) {
                    let _ = self.evict_tx.send((k_str, Arc::new(ev.to_vec())));
                }
            }
        }
    }

    pub fn remove(&self, key: &str) {
        let key_bytes = Bytes::copy_from_slice(key.as_bytes());
        self.inner.remove(&key_bytes);
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
}
