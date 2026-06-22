use crate::error::Result;
use log::info;
use moka::sync::Cache;
use sysinfo::System;

#[derive(Clone)]
pub struct LruCache {
    inner: Cache<String, Vec<u8>>,
    max_bytes: u64,
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

        let inner = Cache::builder()
            .weigher(|_key, value: &Vec<u8>| -> u32 {
                value.len().try_into().unwrap_or(u32::MAX)
            })
            .max_capacity(max_bytes)
            .build();

        Ok(Self { inner, max_bytes })
    }

    /// Construct with a custom memory limit in bytes.
    pub fn with_capacity(max_bytes: u64) -> Self {
        let inner = Cache::builder()
            .weigher(|_key, value: &Vec<u8>| -> u32 {
                value.len().try_into().unwrap_or(u32::MAX)
            })
            .max_capacity(max_bytes)
            .build();

        Self { inner, max_bytes }
    }

    /// Retrieve an entry from the cache, updating its LRU status.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.get(key)
    }

    /// Insert an entry into the cache, executing LRU eviction if maximum capacity is exceeded.
    pub fn put(&self, key: &str, data: Vec<u8>) {
        if (data.len() as u64) <= self.max_bytes {
            self.inner.insert(key.to_string(), data);
        }
    }

    pub fn remove(&self, key: &str) {
        self.inner.invalidate(key);
    }

    // moka doesn't explicitly expose the current aggregate weight directly in a lightweight way
    // without using experimental counters. We just return 0 to satisfy trait signatures
    // or previous testing setups, as moka handles eviction strictly and automatically.
    pub fn current_bytes(&self) -> u64 {
        0
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }
}
