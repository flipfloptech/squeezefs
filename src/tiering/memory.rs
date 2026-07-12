use bytes::Bytes;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use xxhash_rust::xxh3::xxh3_64;

/// The victim's sticky classification at eviction time, read from the
/// `protected` bit — the value the dehydration worker routes on
/// (docs/design-read-path.md §5.3 table): `Protected` ⇒ eligible to
/// dehydrate to the NVMe tier; `Probation` (inserted probationary, never
/// read) ⇒ dropped. NOTE: PR 3 plumbs this behavior-neutral (all classes
/// still dehydrate); the gate flip is PR 4 policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictClass {
    Probation,
    Protected,
}

/// Entry state: the shard value grows ONE sticky class bit beside the
/// clock bit (§5.4).
///
/// `referenced` stays the CONSUMABLE second-chance bit — the clock scan
/// clears it, so every victim has it false at eviction time by
/// construction and it can never classify victims. `protected` is STICKY:
/// set at protected insert or by any `get` on a probation entry, never
/// cleared by the clock, and read at eviction to route the dehydration
/// decision. Two bits because they answer different questions: "spare
/// this entry one more lap?" vs "was this entry ever worth keeping?".
///
/// Concurrency: both bits are single-word `Relaxed` atomics with no
/// cross-word invariant — a racy lost update is a heuristic miss (an
/// entry evicted one lap early / classified probation once too often),
/// never a correctness event ⇒ no loom model required (the design's
/// stated mandate for this type).
struct EntryState {
    referenced: AtomicBool,
    protected: AtomicBool,
}

/// A single thread-safe shard of the Clock cache.
///
/// `scc::HashMap` (not `HashIndex`): removals MOVE the value out, so a
/// multi-MiB `Bytes` payload is freed the moment its entry is evicted.
/// HashIndex defers value drops through epoch-based reclamation, which
/// under a cold-stream's 4 MiB-value churn parked gigabytes of dead
/// payloads past the cgroup cap (the PR 4 row-2 OOM — latent since PR 3,
/// first exercised by a full-length cold stream through the hot tier).
struct MemoryCacheShard {
    map: scc::HashMap<Bytes, (Bytes, EntryState)>,
    eviction_queue: crossbeam::queue::SegQueue<Bytes>,
    eviction_lock: parking_lot::Mutex<()>,
    current_bytes: AtomicUsize,
    max_bytes: usize,
}

impl MemoryCacheShard {
    fn new(max_bytes: usize) -> Self {
        Self {
            map: scc::HashMap::new(),
            eviction_queue: crossbeam::queue::SegQueue::new(),
            eviction_lock: parking_lot::Mutex::new(()),
            current_bytes: AtomicUsize::new(0),
            max_bytes,
        }
    }

    fn get(&self, key: &[u8], promote: bool) -> Option<Bytes> {
        let entry = self.map.get_sync(key)?;
        let (value, state) = entry.get();
        state.referenced.store(true, Ordering::Relaxed);
        if promote {
            // Sticky promotion (§5.4): a block-level re-access marks the
            // entry worth keeping — never cleared by the clock scan.
            state.protected.store(true, Ordering::Relaxed);
        }
        // Non-promoting gets (stream sub-read CONSUMPTION, the R1b row-2
        // measured correction): the entry still earns its clock second
        // chance for the pass, but consuming a fill's own sub-ranges is
        // not evidence it will ever be needed again — promotion here
        // re-taxed streams through protected-victim dehydration and
        // parked gigabytes in the eviction channel (the PR 4 bench OOM).
        Some(value.clone())
    }

    fn put(
        &self,
        key: Bytes,
        value: Bytes,
        protected: bool,
        evicted: &mut Vec<(Bytes, Bytes, EvictClass)>,
    ) {
        let val_len = value.len();
        // Oversized values cannot live in this shard. Remove any smaller stale
        // entry under the same key so readers never observe a truncated prior write.
        if val_len > self.max_bytes {
            let _ = self.remove(&key);
            return;
        }

        let mut old_len = None;
        let _ = self
            .map
            .entry_sync(key.clone())
            .and_modify(|(old_val, state)| {
                let old = std::mem::replace(old_val, value.clone());
                state.referenced.store(true, Ordering::Relaxed);
                if protected {
                    // A protected re-put promotes; a probationary
                    // re-put never DEMOTES an entry something already
                    // read (sticky).
                    state.protected.store(true, Ordering::Relaxed);
                }
                old_len = Some(old.len());
            })
            .or_insert_with(|| {
                self.eviction_queue.push(key.clone());
                self.current_bytes.fetch_add(val_len, Ordering::Relaxed);
                (
                    value,
                    EntryState {
                        // Probationary inserts start with NO second chance
                        // and NO keep-worthiness: first in eviction line,
                        // cannot displace a protected entry with its lap.
                        referenced: AtomicBool::new(protected),
                        protected: AtomicBool::new(protected),
                    },
                )
            });

        if let Some(old) = old_len {
            self.current_bytes.fetch_add(val_len, Ordering::Relaxed);
            self.current_bytes.fetch_sub(old, Ordering::Relaxed);
        }

        if self.current_bytes.load(Ordering::Relaxed) > self.max_bytes {
            self.try_evict(evicted);
        }
    }

    fn try_evict(&self, evicted: &mut Vec<(Bytes, Bytes, EvictClass)>) {
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
                    let mut class = EvictClass::Probation;

                    let inspect_res = self.map.entry_sync(evict_key.clone());
                    match inspect_res {
                        scc::hash_map::Entry::Occupied(mut entry) => {
                            let needs_second_chance = {
                                let (_, state) = entry.get();
                                state.referenced.load(Ordering::Relaxed)
                            };
                            if needs_second_chance {
                                let (_, state) = entry.get_mut();
                                state.referenced.store(false, Ordering::Relaxed);
                                self.eviction_queue.push(evict_key.clone());
                            } else {
                                // HashMap removal MOVES the value out: the
                                // payload is freed (or handed to the
                                // caller) immediately — no epoch-deferred
                                // multi-MiB garbage under churn.
                                let (value, state) = entry.remove();
                                should_evict = true;
                                len = value.len();
                                // The STICKY bit classifies the victim —
                                // the clock consumed `referenced`, so it
                                // is false for every victim by
                                // construction and could never classify.
                                class = if state.protected.load(Ordering::Relaxed) {
                                    EvictClass::Protected
                                } else {
                                    EvictClass::Probation
                                };
                                val = Some(value);
                            }
                        }
                        scc::hash_map::Entry::Vacant(_) => {}
                    }

                    if should_evict {
                        self.current_bytes.fetch_sub(len, Ordering::Relaxed);
                        if let Some(v) = val {
                            evicted.push((evict_key, v, class));
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
            scc::hash_map::Entry::Occupied(entry) => {
                let (val, _) = entry.remove();
                let len = val.len();
                self.current_bytes.fetch_sub(len, Ordering::Relaxed);
                Some(val)
            }
            scc::hash_map::Entry::Vacant(_) => None,
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
        (xxh3_64(key) as usize) & self.shard_mask
    }

    /// Retrieves an item from the cache. Clones the `Bytes` pointer (O(1), zero-copy).
    /// Promotes probation entries to sticky-protected (block-level re-access).
    pub fn get(&self, key: &[u8]) -> Option<Bytes> {
        let idx = self.get_shard_idx(key);
        self.shards[idx].get(key, true)
    }

    /// [`Self::get`] WITHOUT sticky promotion — stream sub-read
    /// consumption (§5.3/§5.5): serves and refreshes the clock bit, but
    /// consuming a fill's own sub-ranges never marks it keep-worthy.
    pub fn get_no_promote(&self, key: &[u8]) -> Option<Bytes> {
        let idx = self.get_shard_idx(key);
        self.shards[idx].get(key, false)
    }

    /// Inserts an item as PROTECTED (referenced=true, protected=true) —
    /// the pre-R4 insert semantics; the ≤256 KiB `read_lru` population is
    /// all-protected by definition. Returns evicted items with their class.
    ///
    /// P2-7: reuse a thread-local eviction buffer so the common no-eviction
    /// path does not allocate a fresh `Vec` on every put.
    pub fn put(&self, key: Bytes, value: Bytes) -> Vec<(Bytes, Bytes, EvictClass)> {
        self.put_with_class(key, value, true)
    }

    /// Insert with referenced=false AND protected=false (§5.4): a one-pass
    /// (streaming/probation) entry is first in line for clock eviction and
    /// cannot displace a protected entry that still has its second chance.
    /// Any `get` promotes it in place (sticky `protected`).
    pub fn put_probationary(&self, key: Bytes, value: Bytes) -> Vec<(Bytes, Bytes, EvictClass)> {
        self.put_with_class(key, value, false)
    }

    fn put_with_class(
        &self,
        key: Bytes,
        value: Bytes,
        protected: bool,
    ) -> Vec<(Bytes, Bytes, EvictClass)> {
        let idx = self.get_shard_idx(&key);
        thread_local! {
            static EVICT_BUF: std::cell::RefCell<Vec<(Bytes, Bytes, EvictClass)>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        EVICT_BUF.with(|cell| {
            let mut evicted = cell.borrow_mut();
            evicted.clear();
            self.shards[idx].put(key, value, protected, &mut evicted);
            if evicted.is_empty() {
                Vec::new()
            } else {
                std::mem::take(&mut *evicted)
            }
        })
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
            shard.map.retain_sync(|k, _| {
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
