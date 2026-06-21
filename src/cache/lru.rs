use crate::error::Result;
use log::info;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use sysinfo::System;

#[derive(Clone)]
pub struct LruCache {
    inner: Arc<Mutex<LruInner>>,
    max_bytes: u64,
}

struct LruInner {
    map: HashMap<String, Vec<u8>>,
    access_order: Vec<String>, // Index 0 is Least Recently Used
    current_bytes: u64,
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

        let inner = Arc::new(Mutex::new(LruInner {
            map: HashMap::new(),
            access_order: Vec::new(),
            current_bytes: 0,
        }));

        Ok(Self { inner, max_bytes })
    }

    /// Construct with a custom memory limit in bytes.
    pub fn with_capacity(max_bytes: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LruInner {
                map: HashMap::new(),
                access_order: Vec::new(),
                current_bytes: 0,
            })),
            max_bytes,
        }
    }

    /// Retrieve an entry from the cache, updating its LRU status.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(val) = inner.map.get(key).cloned() {
            // Update access order: remove key from its current position and append to the end
            if let Some(idx) = inner.access_order.iter().position(|k| k == key) {
                inner.access_order.remove(idx);
            }
            inner.access_order.push(key.to_string());
            Some(val)
        } else {
            None
        }
    }

    /// Insert an entry into the cache, executing LRU eviction if maximum capacity is exceeded.
    pub fn put(&self, key: &str, data: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        let data_size = data.len() as u64;

        if data_size > self.max_bytes {
            // Entry is too large to fit in the cache at all
            return;
        }

        // If replacing an existing entry, subtract its size first
        if let Some(existing) = inner.map.remove(key) {
            inner.current_bytes -= existing.len() as u64;
            if let Some(idx) = inner.access_order.iter().position(|k| k == key) {
                inner.access_order.remove(idx);
            }
        }

        // Evict until we have enough space
        while inner.current_bytes + data_size > self.max_bytes && !inner.access_order.is_empty() {
            let lru_key = inner.access_order.remove(0); // Remove LRU key
            if let Some(removed_val) = inner.map.remove(&lru_key) {
                inner.current_bytes -= removed_val.len() as u64;
                info!(
                    "LRU Cache: Evicted key '{}' ({} bytes) to free space.",
                    lru_key,
                    removed_val.len()
                );
            }
        }

        // Insert new entry
        inner.current_bytes += data_size;
        inner.map.insert(key.to_string(), data);
        inner.access_order.push(key.to_string());
    }

    pub fn remove(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(removed) = inner.map.remove(key) {
            inner.current_bytes -= removed.len() as u64;
            if let Some(idx) = inner.access_order.iter().position(|k| k == key) {
                inner.access_order.remove(idx);
            }
        }
    }

    pub fn current_bytes(&self) -> u64 {
        self.inner.lock().unwrap().current_bytes
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }
}
