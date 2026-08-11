use bytes::Bytes;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use xxhash_rust::xxh3::xxh3_64;

/// The victim's sticky classification at eviction time, read from the
/// `protected` bit — the value the dehydration worker routes on
/// (docs/design-read-path.md §5.3 table): `Protected` ⇒ eligible to
/// dehydrate to the NVMe tier; `Probation` (inserted probationary, never
/// read) ⇒ dropped. NOTE: PR 3 plumbs this behavior-neutral (all classes
/// still dehydrate); the gate flip is PR 4 policy.
///
/// `served_bytes` (admission-governor waste basis, docs/design-read-path.md
/// §5.3 scan-resistance addendum): the payback credit serve sites accrued
/// on this entry since insert (`get_serving`). A protected — i.e.
/// ghost-admitted — victim whose credit never reached its own length did
/// not pay back its whole-block admission fetch; the governor counts the
/// shortfall as windowed waste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictClass {
    Probation,
    Protected {
        served_bytes: u64,
        /// The entry was admitted by a classified STREAM's ghost hit
        /// (read-saturation campaign, 2026-07-29 — the transient stream
        /// window): its payback basis excludes within-pass consumption
        /// (`get_serving` credits nothing — probation would have served
        /// those sub-reads identically, so the admission's marginal value
        /// is cross-pass retention only), and its eviction is exempt from
        /// the `read_admission_evicted_unhit` tripwire (that counter
        /// keeps meaning admitted-and-never-touched).
        stream_admitted: bool,
    },
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
    /// Payback credit (bytes served to real readers since insert) — the
    /// admission governor's waste basis, reported with the victim at
    /// eviction. Relaxed accumulate; a racy lost add is a slightly
    /// stiffer clamp, never a correctness event.
    served_bytes: AtomicU64,
    /// Stream-admitted marker (see [`EvictClass::Protected`]): set by
    /// [`MemoryCache::put_protected_stream`], cleared when a NON-stream
    /// protected re-put lands (the entry earned real re-read heat
    /// through a random path — restore the ordinary payback basis).
    /// Same racy-tolerance class as its siblings: a lost update is one
    /// mis-based victim report, never a correctness event.
    stream_admitted: AtomicBool,
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
    /// Live ENTRY count (write-IOPS economy, 2026-08-11): maintained at
    /// insert/remove/evict so (a) `MemoryCache::is_empty` is O(shards) —
    /// the `parked_overlay_count` law: per-op invalidations of a tier
    /// nothing populated must cost loads, not hashes+bucket locks — and
    /// (b) the RES-10 reclaim gate stops paying `scc::HashMap::len()`
    /// (an O(buckets) SCAN) on every removal.
    entries: AtomicUsize,
    max_bytes: usize,
}

impl MemoryCacheShard {
    fn new(max_bytes: usize) -> Self {
        Self {
            map: scc::HashMap::new(),
            eviction_queue: crossbeam::queue::SegQueue::new(),
            eviction_lock: parking_lot::Mutex::new(()),
            current_bytes: AtomicUsize::new(0),
            entries: AtomicUsize::new(0),
            max_bytes,
        }
    }

    fn get(&self, key: &[u8], promote: bool, served_bytes: u64) -> Option<Bytes> {
        let entry = self.map.get_sync(key)?;
        let (value, state) = entry.get();
        state.referenced.store(true, Ordering::Relaxed);
        if served_bytes > 0 && !state.stream_admitted.load(Ordering::Relaxed) {
            // Serve-site payback credit (admission governor): only real
            // reader serves pass a length; probes and residency checks
            // pass 0 and never inflate an entry's payback. STREAM-admitted
            // entries credit NOTHING (the transient stream window,
            // 2026-07-29): probation would have served their within-pass
            // consumption identically, so the admission's marginal value
            // is cross-pass retention only — counting consumption diluted
            // the governor's waste ratio and held the clamp open under
            // beyond-budget stream loops (the 4 GB/s ledger-pollution
            // face; scan resistance broken for co-tenants).
            state
                .served_bytes
                .fetch_add(served_bytes, Ordering::Relaxed);
        }
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
        referenced: bool,
        stream: bool,
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
                    // read (sticky). Protected re-puts own the payback
                    // basis: a NON-stream protected re-put clears the
                    // stream marker (real random re-read heat), a
                    // stream re-put sets it.
                    state.protected.store(true, Ordering::Relaxed);
                    state.stream_admitted.store(stream, Ordering::Relaxed);
                }
                old_len = Some(old.len());
            })
            .or_insert_with(|| {
                self.eviction_queue.push(key.clone());
                self.current_bytes.fetch_add(val_len, Ordering::Relaxed);
                self.entries.fetch_add(1, Ordering::Relaxed);
                (
                    value,
                    EntryState {
                        // Plain probationary inserts start with NO second
                        // chance and NO keep-worthiness: first in eviction
                        // line, cannot displace a protected entry with its
                        // lap. Pipeline fills (§5.5) arrive probation-class
                        // WITH the second chance (`referenced` alone) —
                        // clock parity with consumed stream residue, never
                        // stickiness.
                        referenced: AtomicBool::new(referenced),
                        protected: AtomicBool::new(protected),
                        served_bytes: AtomicU64::new(0),
                        stream_admitted: AtomicBool::new(stream),
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

    /// R5 clamp (§5.7 Red row): force-evict — second chances IGNORED, the
    /// clamp is an order, not a scan — until this shard holds ≤ `target`
    /// bytes. Victims are returned for the caller's drop/count policy
    /// (under Red the dehydration pause is already active, so they die).
    fn shed_to(&self, target: usize, evicted: &mut Vec<(Bytes, Bytes, EvictClass)>) {
        if let Some(_guard) = self.eviction_lock.try_lock() {
            let max_loops = std::cmp::max(self.eviction_queue.len() * 2, 64);
            let mut loops = 0;
            while self.current_bytes.load(Ordering::Relaxed) > target && loops < max_loops {
                loops += 1;
                let Some(evict_key) = self.eviction_queue.pop() else {
                    break;
                };
                if let scc::hash_map::Entry::Occupied(entry) =
                    self.map.entry_sync(evict_key.clone())
                {
                    let (value, state) = entry.remove();
                    self.current_bytes.fetch_sub(value.len(), Ordering::Relaxed);
                    self.entries.fetch_sub(1, Ordering::Relaxed);
                    let class = if state.protected.load(Ordering::Relaxed) {
                        EvictClass::Protected {
                            served_bytes: state.served_bytes.load(Ordering::Relaxed),
                            stream_admitted: state.stream_admitted.load(Ordering::Relaxed),
                        }
                    } else {
                        EvictClass::Probation
                    };
                    evicted.push((evict_key, value, class));
                }
            }
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
                                self.entries.fetch_sub(1, Ordering::Relaxed);
                                should_evict = true;
                                len = value.len();
                                // The STICKY bit classifies the victim —
                                // the clock consumed `referenced`, so it
                                // is false for every victim by
                                // construction and could never classify.
                                class = if state.protected.load(Ordering::Relaxed) {
                                    EvictClass::Protected {
                                        served_bytes: state.served_bytes.load(Ordering::Relaxed),
                                        stream_admitted: state
                                            .stream_admitted
                                            .load(Ordering::Relaxed),
                                    }
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
        // Write-IOPS economy (2026-08-11): the common invalidation is of
        // an ABSENT key (every W1 patch purges tiers the write path never
        // populated). The retired shape paid a heap `Bytes` mint + a
        // bucket WRITER lock + the RES-10 gate's O(buckets) `map.len()`
        // scan per absent remove — pure ceremony at 400k+ patches/s.
        // Absent key: one borrowed reader-lock probe
        // (`Bytes: Borrow<[u8]>`), no alloc. The probe consults the MAP
        // itself — deliberately never the `entries` gauge, whose relaxed
        // staleness could skip a purge of a just-inserted key (the R-6
        // stale-serve class). The probe→remove race is the same window
        // today's remove→concurrent-insert has (the fill-incarnation/
        // rebind ladder owns it either way).
        if !self.map.contains_sync(key) {
            return None;
        }
        let out = match self.map.entry_sync(Bytes::copy_from_slice(key)) {
            scc::hash_map::Entry::Occupied(entry) => {
                let (val, _) = entry.remove();
                let len = val.len();
                self.current_bytes.fetch_sub(len, Ordering::Relaxed);
                self.entries.fetch_sub(1, Ordering::Relaxed);
                Some(val)
            }
            scc::hash_map::Entry::Vacant(_) => None,
        };
        // RES-10: the removal left its ordering node behind. Nothing pops
        // it but an eviction pass, and a cache under budget never runs
        // one — so an invalidation-heavy tier accumulates one dead node
        // per removal for the life of the mount.
        self.reclaim_eviction_queue();
        out
    }

    /// RES-10: drop eviction-queue nodes whose key is no longer mapped.
    ///
    /// A `SegQueue` has no interior removal, so this is a drain-and-refill
    /// pass gated on the node count exceeding `2 × live + slack` —
    /// amortized O(1) per removal, and it bounds the queue at a multiple
    /// of the LIVE set instead of at "every invalidation this mount ever
    /// did". Skipping when the eviction lock is held keeps it off the
    /// evictor's back (that pass is already draining the queue).
    ///
    /// Ordering: kept nodes are re-pushed in their original relative
    /// order. Nodes pushed CONCURRENTLY with a pass land ahead of them,
    /// so a clock scan in that window can pick a slightly-out-of-order
    /// victim — an LRU-hint imprecision over the handful of inserts made
    /// during one pass, whose cost is one refill, never correctness.
    fn reclaim_eviction_queue(&self) {
        let nodes = self.eviction_queue.len();
        // The live count rides the shard's own entry gauge — the retired
        // `scc::HashMap::len()` here was an O(buckets) scan per removal.
        if nodes <= self.entries.load(Ordering::Relaxed) * 2 + EVICTION_QUEUE_RECLAIM_SLACK {
            return;
        }
        let Some(_guard) = self.eviction_lock.try_lock() else {
            return;
        };
        let mut keep: Vec<Bytes> = Vec::new();
        for _ in 0..nodes {
            let Some(key) = self.eviction_queue.pop() else {
                break;
            };
            if self.map.contains_sync(&key) {
                keep.push(key);
            }
        }
        for key in keep {
            self.eviction_queue.push(key);
        }
    }
}

/// RES-10 hysteresis floor: below this many nodes a backlog is not worth
/// a drain pass (an amortization floor, not a resource cap — the bound
/// that matters is the `2 × live` term, which scales with the shard).
const EVICTION_QUEUE_RECLAIM_SLACK: usize = 64;

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
        self.shards[idx].get(key, true, 0)
    }

    /// [`Self::get`] WITHOUT sticky promotion — stream sub-read
    /// consumption (§5.3/§5.5): serves and refreshes the clock bit, but
    /// consuming a fill's own sub-ranges never marks it keep-worthy.
    pub fn get_no_promote(&self, key: &[u8]) -> Option<Bytes> {
        let idx = self.get_shard_idx(key);
        self.shards[idx].get(key, false, 0)
    }

    /// [`Self::get_no_promote`] plus payback credit: a real reader serve
    /// of `served_bytes` user bytes accrues on the entry (the admission
    /// governor's waste basis, reported with the victim at eviction).
    /// Probes/residency checks must use the non-crediting variants.
    pub fn get_serving(&self, key: &[u8], served_bytes: u64) -> Option<Bytes> {
        let idx = self.get_shard_idx(key);
        self.shards[idx].get(key, false, served_bytes)
    }

    /// Inserts an item as PROTECTED (referenced=true, protected=true) —
    /// the pre-R4 insert semantics; the ≤256 KiB `read_lru` population is
    /// all-protected by definition. Returns evicted items with their class.
    ///
    /// P2-7: reuse a thread-local eviction buffer so the common no-eviction
    /// path does not allocate a fresh `Vec` on every put.
    pub fn put(&self, key: Bytes, value: Bytes) -> Vec<(Bytes, Bytes, EvictClass)> {
        self.put_with_class(key, value, true, true, false)
    }

    /// PROTECTED insert carrying the STREAM-ADMITTED marker (the
    /// transient stream window, 2026-07-29): a classified stream's
    /// governor-granted ghost admission. Clock/class semantics identical
    /// to [`Self::put`]; the marker changes only the payback basis
    /// (`get_serving` credits nothing) and the victim's unhit-tripwire
    /// exemption — see [`EvictClass::Protected`].
    pub fn put_protected_stream(
        &self,
        key: Bytes,
        value: Bytes,
    ) -> Vec<(Bytes, Bytes, EvictClass)> {
        self.put_with_class(key, value, true, true, true)
    }

    /// Insert with referenced=false AND protected=false (§5.4): a one-pass
    /// (streaming/probation) entry is first in line for clock eviction and
    /// cannot displace a protected entry that still has its second chance.
    /// Any `get` promotes it in place (sticky `protected`).
    pub fn put_probationary(&self, key: Bytes, value: Bytes) -> Vec<(Bytes, Bytes, EvictClass)> {
        self.put_with_class(key, value, false, false, false)
    }

    /// Probation CLASS with the one-lap second chance (referenced=true,
    /// protected=false) — §5.5 pipeline fills. Consumed stream residue
    /// re-arms `referenced` at every sub-read serve; an unconsumed
    /// speculative fill inserted without it systematically loses the clock
    /// race to the very bytes the reader has already finished with
    /// (measured on the bench row-2 shape: 595 hot-evict refetches).
    /// Grace is NOT keep-worthiness: victims still classify Probation and
    /// drop at the eviction source.
    pub fn put_probationary_referenced(
        &self,
        key: Bytes,
        value: Bytes,
    ) -> Vec<(Bytes, Bytes, EvictClass)> {
        self.put_with_class(key, value, false, true, false)
    }

    fn put_with_class(
        &self,
        key: Bytes,
        value: Bytes,
        protected: bool,
        referenced: bool,
        stream: bool,
    ) -> Vec<(Bytes, Bytes, EvictClass)> {
        let idx = self.get_shard_idx(&key);
        thread_local! {
            static EVICT_BUF: std::cell::RefCell<Vec<(Bytes, Bytes, EvictClass)>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        EVICT_BUF.with(|cell| {
            let mut evicted = cell.borrow_mut();
            evicted.clear();
            self.shards[idx].put(key, value, protected, referenced, stream, &mut evicted);
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

    /// R5 clamp (§5.7): force-evict across shards until total bytes ≤
    /// `target` (proportional per-shard targets). Victims returned for the
    /// caller's drop/count policy.
    pub fn shed_to(&self, target: usize) -> Vec<(Bytes, Bytes, EvictClass)> {
        let mut evicted = Vec::new();
        let per_shard = target / self.shards.len();
        for shard in &self.shards {
            shard.shed_to(per_shard, &mut evicted);
        }
        evicted
    }

    /// Live entry count across shards (O(shards) loads — the per-shard
    /// gauge the probe-first `remove` fast path rides).
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.entries.load(Ordering::Relaxed))
            .sum()
    }

    /// O(shards) emptiness (the `parked_overlay_count` law).
    pub fn is_empty(&self) -> bool {
        self.shards
            .iter()
            .all(|s| s.entries.load(Ordering::Relaxed) == 0)
    }

    /// Get current total memory usage in bytes.
    pub fn current_bytes(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.current_bytes.load(Ordering::Relaxed))
            .sum()
    }

    /// RES-10: total eviction-queue nodes across shards — the
    /// tombstone-backlog probe. The byte gauge above counts PAYLOAD, so
    /// it cannot see the ordering queue; this is what keeps the
    /// reclamation invariant honest
    /// (`tests/memory_shard_tombstone_tests.rs`).
    pub fn eviction_queue_len(&self) -> usize {
        self.shards.iter().map(|s| s.eviction_queue.len()).sum()
    }

    /// Live entry count (VAL-7a): the `.stats` census gauge that replaces
    /// the unconditional key ENUMERATION — a count leaks no key names.
    pub fn entry_count(&self) -> usize {
        self.shards.iter().map(|s| s.map.len()).sum()
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
