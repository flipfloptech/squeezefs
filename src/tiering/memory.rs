use bytes::Bytes;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use xxhash_rust::xxh3::xxh3_64;

/// A single thread-safe shard of the Clock cache.
struct MemoryCacheShard {
    map: scc::HashIndex<Bytes, (Bytes, AtomicBool)>,
    eviction_queue: crossbeam::queue::SegQueue<Bytes>,
    eviction_lock: parking_lot::Mutex<()>,
    current_bytes: AtomicUsize,
    max_bytes: usize,
}

impl MemoryCacheShard {
    fn new(max_bytes: usize) -> Self {
        Self {
            map: scc::HashIndex::default(),
            eviction_queue: crossbeam::queue::SegQueue::new(),
            eviction_lock: parking_lot::Mutex::new(()),
            current_bytes: AtomicUsize::new(0),
            max_bytes,
        }
    }

    fn get(&self, key: &[u8]) -> Option<Bytes> {
        let entry = self.map.get_sync(key)?;
        let (value, referenced) = entry.get();
        referenced.store(true, Ordering::Relaxed);
        Some(value.clone())
    }

    fn put(&self, key: Bytes, value: Bytes, evicted: &mut Vec<(Bytes, Bytes)>) {
        let val_len = value.len();
        // Oversized values cannot live in this shard. Remove any smaller stale
        // entry under the same key so readers never observe a truncated prior write.
        if val_len > self.max_bytes {
            let _ = self.remove(&key);
            return;
        }

        let mut old_len = None;
        let _ = unsafe {
            self.map
                .entry_sync(key.clone())
                .and_modify(|(old_val, ref_ok)| {
                    let old = std::mem::replace(old_val, value.clone());
                    ref_ok.store(true, Ordering::Relaxed);
                    old_len = Some(old.len());
                })
        }
        .or_insert_with(|| {
            self.eviction_queue.push(key.clone());
            self.current_bytes.fetch_add(val_len, Ordering::Relaxed);
            (value, AtomicBool::new(true))
        });

        if let Some(old) = old_len {
            self.current_bytes.fetch_add(val_len, Ordering::Relaxed);
            self.current_bytes.fetch_sub(old, Ordering::Relaxed);
        }

        if self.current_bytes.load(Ordering::Relaxed) > self.max_bytes {
            self.try_evict(evicted);
        }
    }

    fn try_evict(&self, evicted: &mut Vec<(Bytes, Bytes)>) {
        if let Some(_guard) = self.eviction_lock.try_lock() {
            let approx_len = self.eviction_queue.len();
            let max_loops = std::cmp::max(approx_len * 2, 512);
            let mut loops = 0;

            while self.current_bytes.load(Ordering::Relaxed) > self.max_bytes && loops < max_loops {
                loops += 1;
                if let Some(evict_key) = self.eviction_queue.pop() {
                    let mut should_evict = false;
                    let mut len = 0;
                    let mut val = None;

                    let inspect_res = self.map.entry_sync(evict_key.clone());
                    match inspect_res {
                        scc::hash_index::Entry::Occupied(mut entry) => {
                            let (val_ref, ref_ok) = unsafe { entry.get_mut() };
                            if ref_ok.load(Ordering::Relaxed) {
                                ref_ok.store(false, Ordering::Relaxed);
                                self.eviction_queue.push(evict_key.clone());
                            } else {
                                should_evict = true;
                                len = val_ref.len();
                                val = Some(val_ref.clone());
                                entry.remove_entry();
                            }
                        }
                        scc::hash_index::Entry::Vacant(_) => {}
                    }

                    if should_evict {
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
    }

    fn remove(&self, key: &[u8]) -> Option<Bytes> {
        let entry = self.map.entry_sync(Bytes::copy_from_slice(key));
        match entry {
            scc::hash_index::Entry::Occupied(entry) => {
                let (val, _) = entry.get();
                let val_clone = val.clone();
                let len = val.len();
                entry.remove_entry();
                self.current_bytes.fetch_sub(len, Ordering::Relaxed);
                Some(val_clone)
            }
            scc::hash_index::Entry::Vacant(_) => None,
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
            while shard.eviction_queue.pop().is_some() {}
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
        let cache = MemoryCache::new(200, 4); // 200 bytes capacity, 4 shards

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
        let index: scc::HashIndex<String, (String, std::sync::atomic::AtomicBool)> =
            scc::HashIndex::default();
        let _ = index.insert_sync(
            "key".to_string(),
            ("val".to_string(), std::sync::atomic::AtomicBool::new(true)),
        );
        let entry = index.entry_sync("key".to_string());
        let val = match entry {
            scc::hash_index::Entry::Occupied(entry) => {
                let (v, _) = entry.get();
                let v_clone = v.clone();
                entry.remove_entry();
                Some(v_clone)
            }
            scc::hash_index::Entry::Vacant(_) => None,
        };
        assert_eq!(val, Some("val".to_string()));
    }
}
