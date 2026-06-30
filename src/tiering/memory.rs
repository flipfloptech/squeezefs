use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use xxhash_rust::xxh3::xxh3_64;

/// A node in the Clock cache.
struct ClockNode {
    key: Bytes,
    value: Bytes,
    referenced: AtomicBool,
}

struct WriteState {
    arena: Vec<Option<*mut ClockNode>>,
    free_slots: Vec<usize>,
    clock_hand: usize,
    current_bytes: usize,
    max_bytes: usize,
}

/// A single thread-safe shard of the Clock cache using Epoch-Based Reclamation.
pub struct MemoryCacheShard {
    map: dashmap::DashMap<Bytes, *mut ClockNode, ahash::RandomState>,
    write_state: parking_lot::Mutex<WriteState>,
}

unsafe impl Send for MemoryCacheShard {}
unsafe impl Sync for MemoryCacheShard {}

impl MemoryCacheShard {
    fn new(max_bytes: usize) -> Self {
        Self {
            map: dashmap::DashMap::with_hasher(ahash::RandomState::new()),
            write_state: parking_lot::Mutex::new(WriteState {
                arena: Vec::new(),
                free_slots: Vec::new(),
                clock_hand: 0,
                current_bytes: 0,
                max_bytes,
            }),
        }
    }

    fn get(&self, key: &[u8]) -> Option<Bytes> {
        let _guard = crossbeam::epoch::pin();
        if let Some(r) = self.map.get(key) {
            let ptr = *r.value();
            if !ptr.is_null() {
                unsafe {
                    let node = &*ptr;
                    node.referenced.store(true, Ordering::Relaxed);
                    return Some(node.value.clone());
                }
            }
        }
        None
    }

    fn put(&self, key: Bytes, value: Bytes, evicted_out: &mut Vec<(Bytes, Bytes)>) {
        let val_len = value.len();
        let mut state = self.write_state.lock();
        if val_len > state.max_bytes {
            return;
        }

        let guard = crossbeam::epoch::pin();

        if let Some(r) = self.map.get(&key) {
            let ptr = *r.value();
            if !ptr.is_null() {
                unsafe {
                    let old_node = Box::from_raw(ptr);
                    let old_len = old_node.value.len();
                    
                    let new_node = Box::into_raw(Box::new(ClockNode {
                        key: key.clone(),
                        value,
                        referenced: AtomicBool::new(true),
                    }));
                    
                    self.map.insert(key.clone(), new_node);
                    
                    for slot in &mut state.arena {
                        if let Some(p) = slot {
                            if *p == ptr {
                                *p = new_node;
                                break;
                            }
                        }
                    }
                    
                    state.current_bytes = (state.current_bytes + val_len).saturating_sub(old_len);
                    
                    guard.defer(move || {
                        drop(old_node);
                    });
                }
            }
        } else {
            let new_node = Box::into_raw(Box::new(ClockNode {
                key: key.clone(),
                value,
                referenced: AtomicBool::new(true),
            }));

            if let Some(free_idx) = state.free_slots.pop() {
                state.arena[free_idx] = Some(new_node);
            } else {
                state.arena.push(Some(new_node));
            };

            self.map.insert(key, new_node);
            state.current_bytes += val_len;
        }

        let total_slots = state.arena.len();
        let mut loops = 0;
        while state.current_bytes > state.max_bytes && total_slots > 0 {
            if state.clock_hand >= total_slots {
                state.clock_hand = 0;
                loops += 1;
                if loops > 2 {
                    break;
                }
            }

            if let Some(ptr) = state.arena[state.clock_hand] {
                unsafe {
                    let node = &*ptr;
                    if node.referenced.load(Ordering::Relaxed) {
                        node.referenced.store(false, Ordering::Relaxed);
                        state.clock_hand += 1;
                    } else {
                        let idx = state.clock_hand;
                        state.arena[idx] = None;
                        state.free_slots.push(idx);
                        
                        self.map.remove(&node.key);
                        state.current_bytes = state.current_bytes.saturating_sub(node.value.len());
                        
                        evicted_out.push((node.key.clone(), node.value.clone()));
                        
                        let ptr_val = ptr as usize;
                        guard.defer(move || {
                            let _ = Box::from_raw(ptr_val as *mut ClockNode);
                        });
                        
                        state.clock_hand += 1;
                    }
                }
            } else {
                state.clock_hand += 1;
            }
        }
    }

    fn remove(&self, key: &[u8]) -> Option<Bytes> {
        let mut state = self.write_state.lock();
        if let Some((_, ptr)) = self.map.remove(key) {
            if !ptr.is_null() {
                unsafe {
                    for (i, slot) in state.arena.iter_mut().enumerate() {
                        if let Some(p) = slot {
                            if *p == ptr {
                                *slot = None;
                                state.free_slots.push(i);
                                break;
                            }
                        }
                    }
                    
                    let node = Box::from_raw(ptr);
                    state.current_bytes = state.current_bytes.saturating_sub(node.value.len());
                    
                    let val = node.value.clone();
                    let guard = crossbeam::epoch::pin();
                    guard.defer(move || {
                        drop(node);
                    });
                    return Some(val);
                }
            }
        }
        None
    }
}

impl Drop for MemoryCacheShard {
    fn drop(&mut self) {
        let state = self.write_state.get_mut();
        for slot in &mut state.arena {
            if let Some(ptr) = slot.take() {
                unsafe {
                    let _ = Box::from_raw(ptr);
                }
            }
        }
    }
}

/// A highly concurrent, sharded in-memory cache using the Clock (second-chance) eviction policy and EBR.
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
        let hash = xxh3_64(key);
        (hash as usize) & self.shard_mask
    }

    /// Retrieves an item from the cache. Clones the `Bytes` pointer (O(1), zero-copy).
    /// Uses only a read-free epoch pin to avoid lock contention under high concurrent read workloads.
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
        self.shards.iter().map(|s| s.write_state.lock().current_bytes).sum()
    }

    /// Get all keys in the cache.
    pub fn keys(&self) -> Vec<Bytes> {
        let mut keys = Vec::new();
        for shard in &self.shards {
            for r in shard.map.iter() {
                keys.push(r.key().clone());
            }
        }
        keys
    }

    /// Clear all keys from the cache.
    pub fn clear(&self) {
        for shard in &self.shards {
            let mut state = shard.write_state.lock();
            shard.map.clear();
            for slot in &mut state.arena {
                if let Some(ptr) = slot.take() {
                    unsafe {
                        let _ = Box::from_raw(ptr);
                    }
                }
            }
            state.free_slots.clear();
            state.clock_hand = 0;
            state.current_bytes = 0;
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

        assert_eq!(cache.current_bytes(), 25);

        cache.get(&k1);

        let evicted = cache.put(k3.clone(), v3.clone());
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].0, k1);
        assert_eq!(evicted[0].1, v1);

        assert_eq!(cache.get(&k1), None);
        assert_eq!(cache.get(&k2), Some(v2));
        assert_eq!(cache.get(&k3), Some(v3));
    }
}
