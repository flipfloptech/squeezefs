use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use xxhash_rust::xxh3::xxh3_64;

/// A node in the Clock cache.
struct ClockNode {
    key: Bytes,
    value: Bytes,
    referenced: AtomicBool,
}

/// A single thread-safe shard of the Clock cache.
struct MemoryCacheShard {
    map: HashMap<Bytes, usize>,
    arena: Vec<Option<ClockNode>>,
    free_slots: Vec<usize>,
    clock_hand: usize,
    current_bytes: usize,
    max_bytes: usize,
}

impl MemoryCacheShard {
    fn new(max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            arena: Vec::new(),
            free_slots: Vec::new(),
            clock_hand: 0,
            current_bytes: 0,
            max_bytes,
        }
    }

    fn get(&self, key: &[u8]) -> Option<Bytes> {
        if let Some(&idx) = self.map.get(key) {
            if let Some(ref node) = self.arena[idx] {
                node.referenced.store(true, Ordering::Relaxed);
                Some(node.value.clone())
            } else {
                None
            }
        } else {
            None
        }
    }

    fn put(&mut self, key: Bytes, value: Bytes, evicted: &mut Vec<(Bytes, Bytes)>) {
        let val_len = value.len();
        if val_len > self.max_bytes {
            // Value itself is larger than the entire shard capacity
            return;
        }

        if let Some(&idx) = self.map.get(&key) {
            // Update existing
            let old_len = if let Some(ref mut node) = self.arena[idx] {
                let old = node.value.len();
                node.value = value;
                node.referenced.store(true, Ordering::Relaxed);
                old
            } else {
                self.arena[idx] = Some(ClockNode {
                    key: key.clone(),
                    value: value.clone(),
                    referenced: AtomicBool::new(true),
                });
                0
            };
            self.current_bytes = (self.current_bytes + val_len).saturating_sub(old_len);
        } else {
            // Insert new node
            let idx = if let Some(free_idx) = self.free_slots.pop() {
                self.arena[free_idx] = Some(ClockNode {
                    key: key.clone(),
                    value,
                    referenced: AtomicBool::new(true),
                });
                free_idx
            } else {
                let free_idx = self.arena.len();
                self.arena.push(Some(ClockNode {
                    key: key.clone(),
                    value,
                    referenced: AtomicBool::new(true),
                }));
                free_idx
            };

            self.map.insert(key, idx);
            self.current_bytes += val_len;
        }

        // Perform Clock eviction if over capacity
        let total_slots = self.arena.len();
        let mut loops = 0;
        while self.current_bytes > self.max_bytes && total_slots > 0 {
            if self.clock_hand >= total_slots {
                self.clock_hand = 0;
                loops += 1;
                if loops > 2 {
                    // Prevent infinite loop in edge cases where all nodes are referenced or pinned
                    break;
                }
            }

            if let Some(ref node) = self.arena[self.clock_hand] {
                if node.referenced.load(Ordering::Relaxed) {
                    node.referenced.store(false, Ordering::Relaxed);
                    self.clock_hand += 1;
                } else {
                    // Evict this node
                    let idx = self.clock_hand;
                    if let Some(evicted_node) = self.arena[idx].take() {
                        self.map.remove(&evicted_node.key);
                        self.free_slots.push(idx);
                        self.current_bytes =
                            self.current_bytes.saturating_sub(evicted_node.value.len());
                        evicted.push((evicted_node.key, evicted_node.value));
                    }
                    self.clock_hand += 1;
                }
            } else {
                self.clock_hand += 1;
            }
        }
    }

    fn remove(&mut self, key: &[u8]) -> Option<Bytes> {
        if let Some(idx) = self.map.remove(key) {
            if let Some(node) = self.arena[idx].take() {
                self.free_slots.push(idx);
                self.current_bytes = self.current_bytes.saturating_sub(node.value.len());
                Some(node.value)
            } else {
                None
            }
        } else {
            None
        }
    }
}

/// A highly concurrent, sharded in-memory cache using the Clock (second-chance) eviction policy.
pub struct MemoryCache {
    shards: Vec<RwLock<MemoryCacheShard>>,
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
            shards.push(RwLock::new(MemoryCacheShard::new(shard_capacity)));
        }
        Self {
            shards,
            shard_mask: num_shards - 1,
        }
    }

    #[inline]
    fn get_shard_idx(&self, key: &[u8]) -> usize {
        let hash = xxh3_64(key);
        (hash as usize) & self.shard_mask
    }

    /// Retrieves an item from the cache. Clones the `Bytes` pointer (O(1), zero-copy).
    /// Uses only a read lock to avoid lock contention under high concurrent read workloads.
    pub fn get(&self, key: &[u8]) -> Option<Bytes> {
        let idx = self.get_shard_idx(key);
        self.shards[idx].read().get(key)
    }

    /// Inserts an item into the cache. Returns any items evicted from the cache.
    pub fn put(&self, key: Bytes, value: Bytes) -> Vec<(Bytes, Bytes)> {
        let idx = self.get_shard_idx(&key);
        let mut evicted = Vec::new();
        self.shards[idx].write().put(key, value, &mut evicted);
        evicted
    }

    /// Removes an item from the cache, returning the value if it existed.
    pub fn remove(&self, key: &[u8]) -> Option<Bytes> {
        let idx = self.get_shard_idx(key);
        self.shards[idx].write().remove(key)
    }

    /// Get current total memory usage in bytes.
    pub fn current_bytes(&self) -> usize {
        self.shards.iter().map(|s| s.read().current_bytes).sum()
    }

    /// Get all keys in the cache.
    pub fn keys(&self) -> Vec<Bytes> {
        let mut keys = Vec::new();
        for shard in &self.shards {
            let guard = shard.read();
            keys.extend(guard.map.keys().cloned());
        }
        keys
    }

    /// Clear all keys from the cache.
    pub fn clear(&self) {
        for shard in &self.shards {
            let mut guard = shard.write();
            guard.map.clear();
            guard.arena.clear();
            guard.free_slots.clear();
            guard.clock_hand = 0;
            guard.current_bytes = 0;
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
}
