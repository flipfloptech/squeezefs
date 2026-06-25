use crate::error::Result;
use log::info;
use moka::notification::RemovalCause;
use moka::sync::Cache;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use sysinfo::System;

#[derive(Clone)]
pub struct LruCache {
    inner: Cache<String, Arc<Vec<u8>>>,
    max_bytes: u64,
    current_bytes: Arc<AtomicU64>,
    evict_rx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<(String, Arc<Vec<u8>>)>>>>,
}

impl LruCache {
    pub fn new() -> Result<Self> {
        // Query host system memory
        let mut sys = System::new();
        sys.refresh_memory();
        let total_memory = sys.total_memory(); // In bytes

        // Default to 20% of system RAM
        let max_bytes = total_memory / 5;
        info!(
            "Unified System RAM LRU Cache: total system RAM detected = {} MB. Reserving 20% ({} MB) for block caching.",
            total_memory / 1024 / 1024,
            max_bytes / 1024 / 1024
        );

        let current_bytes = Arc::new(AtomicU64::new(0));
        let current_bytes_clone = current_bytes.clone();

        let (evict_tx, evict_rx) = tokio::sync::mpsc::unbounded_channel();
        let evict_tx_clone = evict_tx.clone();

        let inner = Cache::builder()
            .weigher(|_key: &String, value: &Arc<Vec<u8>>| -> u32 {
                value.len().try_into().unwrap_or(u32::MAX)
            })
            .max_capacity(max_bytes)
            .time_to_idle(std::time::Duration::from_secs(30))
            .eviction_listener(move |key, value: Arc<Vec<u8>>, cause| {
                current_bytes_clone.fetch_sub(value.len() as u64, Ordering::Relaxed);
                if cause == RemovalCause::Expired || cause == RemovalCause::Size {
                    let _ = evict_tx_clone.send(((*key).clone(), value));
                }
            })
            .build();

        Ok(Self {
            inner,
            max_bytes,
            current_bytes,
            evict_rx: Arc::new(std::sync::Mutex::new(Some(evict_rx))),
        })
    }

    /// Construct with a custom memory limit in bytes.
    pub fn with_capacity(max_bytes: u64) -> Self {
        let current_bytes = Arc::new(AtomicU64::new(0));
        let current_bytes_clone = current_bytes.clone();

        let (evict_tx, evict_rx) = tokio::sync::mpsc::unbounded_channel();
        let evict_tx_clone = evict_tx.clone();

        let inner = Cache::builder()
            .weigher(|_key: &String, value: &Arc<Vec<u8>>| -> u32 {
                value.len().try_into().unwrap_or(u32::MAX)
            })
            .max_capacity(max_bytes)
            .time_to_idle(std::time::Duration::from_secs(30))
            .eviction_listener(move |key, value: Arc<Vec<u8>>, cause| {
                current_bytes_clone.fetch_sub(value.len() as u64, Ordering::Relaxed);
                if cause == RemovalCause::Expired || cause == RemovalCause::Size {
                    let _ = evict_tx_clone.send(((*key).clone(), value));
                }
            })
            .build();

        Self {
            inner,
            max_bytes,
            current_bytes,
            evict_rx: Arc::new(std::sync::Mutex::new(Some(evict_rx))),
        }
    }

    /// Retrieve the eviction receiver. Can only be taken once.
    pub fn take_evict_rx(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<(String, Arc<Vec<u8>>)>> {
        self.evict_rx.lock().ok()?.take()
    }

    /// Retrieve an entry from the cache, updating its LRU status.
    pub fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        self.inner.get(key)
    }

    /// Insert an entry into the cache, executing LRU eviction if maximum capacity is exceeded.
    pub fn put(&self, key: &str, data: Arc<Vec<u8>>) {
        if (data.len() as u64) <= self.max_bytes {
            self.current_bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
            self.inner.insert(key.to_string(), data);
        }
    }

    pub fn remove(&self, key: &str) {
        self.inner.invalidate(key);
    }

    pub fn current_bytes(&self) -> u64 {
        self.current_bytes.load(Ordering::Relaxed)
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub fn keys(&self) -> Vec<String> {
        self.inner.iter().map(|(k, _)| k.as_ref().clone()).collect()
    }

    pub fn run_pending_tasks(&self) {
        self.inner.run_pending_tasks();
    }

    pub fn clear(&self) {
        self.inner.invalidate_all();
    }
}
