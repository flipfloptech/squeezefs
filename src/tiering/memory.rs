use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use xxhash_rust::xxh3::xxh3_64;

/// A node in the Clock cache.
struct ClockNode {
    value: Bytes,
    referenced: AtomicBool,
}

struct EvictionState {
    queue: VecDeque<Bytes>,
}

/// A single thread-safe shard of the Clock cache.
struct MemoryCacheShard {
    map: scc::HashIndex<Bytes, std::sync::Arc<ClockNode>>,
    eviction_state: parking_lot::Mutex<EvictionState>,
    current_bytes: AtomicUsize,
    max_bytes: usize,
}

impl MemoryCacheShard {
    fn new(max_bytes: usize) -> Self {
        Self {
            map: scc::HashIndex::default(),
            eviction_state: parking_lot::Mutex::new(EvictionState {
                queue: VecDeque::new(),
            }),
            current_bytes: AtomicUsize::new(0),
            max_bytes,
        }
    }

    fn get(&self, key: &[u8]) -> Option<Bytes> {
        let entry = self.map.get_sync(key)?;
        let node = entry.get();
        node.referenced.store(true, Ordering::Relaxed);
        Some(node.value.clone())
    }

    fn put(&self, key: Bytes, value: Bytes, evicted: &mut Vec<(Bytes, Bytes)>) {
        let val_len = value.len();
        if val_len > self.max_bytes {
            // Value itself is larger than the entire shard capacity
            return;
        }

        let node = std::sync::Arc::new(ClockNode {
            value: value.clone(),
            referenced: AtomicBool::new(true),
        });

        let mut state = self.eviction_state.lock();

        let mut old_len = None;
        if let Some(existing) = self.map.get_sync(&key) {
            old_len = Some(existing.get().value.len());
        } // existing guard dropped here

        if let Some(len) = old_len {
            self.map.remove_sync(&key);
            let _ = self.map.insert_sync(key.clone(), node);
            self.current_bytes.fetch_add(val_len, Ordering::Relaxed);
            self.current_bytes.fetch_sub(len, Ordering::Relaxed);
        } else {
            let _ = self.map.insert_sync(key.clone(), node);
            state.queue.push_back(key);
            self.current_bytes.fetch_add(val_len, Ordering::Relaxed);
        }

        // Perform Clock eviction if over capacity
        let mut loops = 0;
        let max_loops = state.queue.len() * 2;
        while self.current_bytes.load(Ordering::Relaxed) > self.max_bytes
            && !state.queue.is_empty()
            && loops < max_loops
        {
            loops += 1;
            if let Some(evict_key) = state.queue.pop_front() {
                let mut should_evict = false;
                let mut len = 0;
                let mut val = None;
                if let Some(entry) = self.map.get_sync(&evict_key) {
                    let node = entry.get();
                    if node.referenced.load(Ordering::Relaxed) {
                        node.referenced.store(false, Ordering::Relaxed);
                        state.queue.push_back(evict_key.clone());
                    } else {
                        should_evict = true;
                        len = node.value.len();
                        val = Some(node.value.clone());
                    }
                } // entry guard dropped here

                if should_evict {
                    self.map.remove_sync(&evict_key);
                    self.current_bytes.fetch_sub(len, Ordering::Relaxed);
                    if let Some(v) = val {
                        evicted.push((evict_key, v));
                    }
                }
            } else {
                break;
            }
        }
    }

    fn remove(&self, key: &[u8]) -> Option<Bytes> {
        let _state = self.eviction_state.lock();
        let mut val = None;
        let mut len = 0;
        if let Some(entry) = self.map.get_sync(key) {
            let node = entry.get();
            val = Some(node.value.clone());
            len = node.value.len();
        } // entry guard dropped here

        if let Some(v) = val {
            self.map.remove_sync(key);
            self.current_bytes.fetch_sub(len, Ordering::Relaxed);
            Some(v)
        } else {
            None
        }
    }
}

/// A highly concurrent, sharded in-memory cache using the Clock (second-chance) eviction policy.
pub struct MemoryCache {
    shards: Vec<MemoryCacheShard>,
    shard_mask: usize,
}

impl MemoryCache {
    /// Creates a new MemoryCache with a maximum total byte capacity and a number of shards.
    /// `num_shards` must be a power of two.
    pub fn new(max_bytes: usize, num_shards: usize) -> Self {
        assert!(
            num_shards.is_power_of_two(),
            "Number of shards must be a power of two"
        );
        let shard_capacity = max_bytes / num_shards;
        let mut shards = Vec::with_capacity(num_shards);
        for _ in 0..num_shards {
            shards.push(MemoryCacheShard::new(shard_capacity));
        }
        Self {
            shards,
            shard_mask: num_shards - 1,
        }
    }

    #[inline]
    fn get_shard_idx(&self, key: &[u8]) -> usize {
        let hash = if key.len() <= 32 {
            use std::hash::Hasher;
            let mut h = ahash::AHasher::default();
            h.write(key);
            h.finish()
        } else {
            xxh3_64(key)
        };
        (hash as usize) & self.shard_mask
    }

    /// Retrieves an item from the cache. Clones the `Bytes` pointer (O(1), zero-copy).
    pub fn get(&self, key: &[u8]) -> Option<Bytes> {
        let idx = self.get_shard_idx(key);
        self.shards[idx].get(key)
    }

    /// Inserts an item into the cache. Returns any items evicted from the cache.
    pub fn put(&self, key: Bytes, value: Bytes) -> Vec<(Bytes, Bytes)> {
        let idx = self.get_shard_idx(&key);
        let mut evicted = Vec::new();
        self.shards[idx].put(key, value, &mut evicted);
        evicted
    }

    /// Removes an item from the cache, returning the value if it existed.
    pub fn remove(&self, key: &[u8]) -> Option<Bytes> {
        let idx = self.get_shard_idx(key);
        self.shards[idx].remove(key)
    }

    /// Get current total memory usage in bytes.
    pub fn current_bytes(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.current_bytes.load(Ordering::Relaxed))
            .sum()
    }

    /// Get all keys in the cache.
    pub fn keys(&self) -> Vec<Bytes> {
        let mut keys = Vec::new();
        for shard in &self.shards {
            shard.map.iter_sync(|k, _| {
                keys.push(k.clone());
                true
            });
        }
        keys
    }

    /// Clear all keys from the cache.
    pub fn clear(&self) {
        for shard in &self.shards {
            shard.map.clear_sync();
            let mut state = shard.eviction_state.lock();
            state.queue.clear();
            shard.current_bytes.store(0, Ordering::Relaxed);
        }
    }

    /// Expose the number of shards for testing and diagnostic purposes.
    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_cache_basic() {
        let cache = MemoryCache::new(100, 4); // 100 bytes capacity, 4 shards

        let k1 = Bytes::from("key1");
        let v1 = Bytes::from(vec![0; 20]);
        let k2 = Bytes::from("key2");
        let v2 = Bytes::from(vec![0; 20]);

        assert!(cache.put(k1.clone(), v1.clone()).is_empty());
        assert!(cache.put(k2.clone(), v2.clone()).is_empty());

        assert_eq!(cache.get(&k1), Some(v1.clone()));
        assert_eq!(cache.get(&k2), Some(v2.clone()));
    }

    #[test]
    fn test_memory_cache_eviction() {
        let cache = MemoryCache::new(30, 1); // 30 bytes capacity, 1 shard

        let k1 = Bytes::from("k1");
        let v1 = Bytes::from(vec![1; 15]);
        let k2 = Bytes::from("k2");
        let v2 = Bytes::from(vec![2; 10]);
        let k3 = Bytes::from("k3");
        let v3 = Bytes::from(vec![3; 15]);

        cache.put(k1.clone(), v1.clone());
        cache.put(k2.clone(), v2.clone());

        // At this point, we have 25 bytes.
        assert_eq!(cache.current_bytes(), 25);

        // Access k1 to set referenced = true
        cache.get(&k1);

        // Put k3 (15 bytes). Total capacity is 30. Adding k3 makes it 40.
        // Clock hand will check k1 (referenced = true, clear it, set to false),
        // check k2 (referenced = true, clear it, set to false),
        // loop back to k1 (referenced = false, evicts k1).
        let evicted = cache.put(k3.clone(), v3.clone());
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].0, k1);
        assert_eq!(evicted[0].1, v1);

        assert_eq!(cache.get(&k1), None);
        assert_eq!(cache.get(&k2), Some(v2));
        assert_eq!(cache.get(&k3), Some(v3));
    }

    #[test]
    fn test_scc_api() {
        let index: scc::HashIndex<String, std::sync::Arc<u32>> = scc::HashIndex::default();
        let _ = index.insert_sync("key".to_string(), std::sync::Arc::new(42));
        let entry = index.get_sync("key");
        assert!(entry.is_some());
        assert_eq!(**entry.unwrap().get(), 42);
    }
}
