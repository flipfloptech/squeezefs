use crate::tiering::memory::EvictClass;
use bytes::Bytes;
use std::sync::Arc;

/// Dehydration channel, typed with the victim's sticky class (R4 / §5.3):
/// plumbed BEHAVIOR-NEUTRAL in PR 3 (the worker still dehydrates every
/// class); PR 4 flips the protected-only gate on this information.
type EvictReceiver = squeezefs_ipc::sqz_channel::mpsc::Receiver<(String, Bytes, EvictClass)>;

/// Insert flavor: the (protected, referenced) bit pairs the clock shard
/// distinguishes (§5.4/§5.5).
enum PutClass {
    /// referenced=true, protected=true — keep-worthy by definition.
    Protected,
    /// referenced=true, protected=true, stream-admitted marker set (the
    /// transient stream window, 2026-07-29): a governor-granted stream
    /// admission — within-pass consumption credits no payback.
    ProtectedStream,
    /// referenced=false, protected=false — one-pass residue, first in line.
    Probation,
    /// referenced=true, protected=false — §5.5 pipeline fill: one-lap
    /// grace, never sticky.
    ProbationReferenced,
}

#[derive(Clone)]
pub struct LruCache {
    inner: Arc<crate::tiering::memory::MemoryCache>,
    max_bytes: u64,
    evict_tx: squeezefs_ipc::sqz_channel::mpsc::Sender<(String, Bytes, EvictClass)>,
    evict_rx: Arc<std::sync::Mutex<Option<EvictReceiver>>>,
    /// LIVE payload bytes parked in the eviction channel (R5): the send
    /// side adds, the dehydration worker subtracts per message. The QUICK
    /// cage-OOM class was exactly this queue holding up to 16,384 full
    /// payloads of live `Bytes` — multi-GiB, ungauged, invisible to the
    /// authority (measured: RSS 2.1 → 7.5 GiB in < 30 s at Green).
    evict_channel_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// Victims dropped at the send side because the channel held
    /// [`Self::EVICT_CHANNEL_BYTE_BOUND`] live bytes (or the slot count
    /// filled). Dehydration is best-effort warmth — dropping is the PR 4
    /// source-drop precedent, never a durability event.
    evict_channel_drops: Arc<std::sync::atomic::AtomicU64>,
    /// True once a dehydration worker took the receiver. Caches without a
    /// worker (write_lru) must never park victims in an unconsumed
    /// channel — pre-R5 that queue silently held up to 16,384 live
    /// payloads with no drain path at all.
    evict_rx_taken: Arc<std::sync::atomic::AtomicBool>,
    /// RES-4: the R5 shed target for this channel, honored ONLY while the
    /// budget is Yellow+ (see [`Self::evict_channel_bound`]). Shedding a
    /// channel cannot pull messages back out of it, so the shed clamps
    /// the SEND side: new victims drop at the source (counted) while the
    /// dehydration worker drains what is already parked — the never-lossy
    /// R5 discipline. Green restores the static bound, so one Red pulse
    /// cannot cold-start the disk tier forever.
    evict_channel_shed_target: Arc<std::sync::atomic::AtomicU64>,
    /// R1b: drop Probation victims at the eviction source (count only —
    /// no channel traffic). Set for the hot-block tier; read_lru keeps
    /// full-channel behavior (its inserts are all protected anyway).
    drop_probation_evictions: bool,
    /// Scan-resistance waste sink (hot-block tier only): every victim is
    /// reported to the admission governor at the eviction SOURCE — clock
    /// evictions, Yellow-freeze clamps and Red sheds alike — so
    /// admitted-but-never-paid-back evictions feed the escalation clamp.
    /// `purge`/`remove` (invalidations) deliberately do not report.
    admission_governor: Option<Arc<crate::routing::AdmissionGovernor>>,
}

impl LruCache {
    // (The former `new()` — a 20 %-of-system-RAM default constructor —
    // had NO callers and was the last system-RAM sizing read outside the
    // fleet-shared root (KD-MW-14 §5.6): deleted per the no-dead-code
    // law rather than converted. Every live construction sizes
    // explicitly via `with_capacity*`.)

    /// Construct with a custom memory limit in bytes.
    pub fn with_capacity(max_bytes: u64) -> Self {
        Self::with_capacity_min_shard(max_bytes, 0)
    }

    /// [`Self::with_capacity`] with a minimum per-shard capacity: caches
    /// holding multi-MiB values (the hot-block tier) must keep several
    /// NEIGHBORS per shard or hash-colliding keys can never coexist —
    /// core-count sharding gave the default 128 MiB hot budget ONE 4 MiB
    /// block per shard (perpetual churn on fitting working sets). The
    /// shard count is reduced (never below 1) until each shard holds at
    /// least `min_shard_bytes`.
    pub fn with_capacity_min_shard(max_bytes: u64, min_shard_bytes: u64) -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(16);
        let mut num_shards = std::cmp::max(cores.next_power_of_two(), 16);
        if min_shard_bytes > 0 {
            while num_shards > 1 && max_bytes / (num_shards as u64) < min_shard_bytes {
                num_shards /= 2;
            }
        }

        let mut actual_shards = num_shards;

        let actual_bytes = if max_bytes < 10 * 1024 * 1024 {
            actual_shards = 1;
            max_bytes
        } else {
            #[cfg(test)]
            {
                // In tests, scale down shards if capacity is too small to avoid huge allocations
                while actual_shards > 16 && max_bytes / (actual_shards as u64) < 4 * 1024 * 1024 {
                    actual_shards /= 2;
                }
                max_bytes
            }
            #[cfg(not(test))]
            {
                let min_required = (num_shards * 4 * 1024 * 1024) as u64;
                // In production, never let capacity be smaller than the minimum required
                std::cmp::max(max_bytes, min_required)
            }
        };

        let inner = Arc::new(crate::tiering::memory::MemoryCache::new(
            actual_bytes as usize,
            actual_shards,
        ));
        let (evict_tx, evict_rx) = squeezefs_ipc::sqz_channel::mpsc::channel(16384);
        Self {
            inner,
            max_bytes: actual_bytes,
            evict_tx,
            evict_rx: Arc::new(std::sync::Mutex::new(Some(evict_rx))),
            evict_channel_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            evict_channel_drops: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            evict_rx_taken: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            evict_channel_shed_target: Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX)),
            drop_probation_evictions: false,
            admission_governor: None,
        }
    }

    /// R5: byte bound on LIVE payloads parked in the eviction channel.
    pub const EVICT_CHANNEL_BYTE_BOUND: u64 = 256 * 1024 * 1024;

    /// Current live payload bytes parked in the channel (R5 gauge).
    pub fn evict_channel_bytes(&self) -> u64 {
        self.evict_channel_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// RES-4: the R5 shed hook for this channel — see
    /// `evict_channel_shed_target`.
    pub fn shed_evict_channel(&self, target: u64) {
        self.evict_channel_shed_target
            .store(target, std::sync::atomic::Ordering::Relaxed);
    }

    /// The effective send-side admission bound: the static bound at
    /// Green, `min(bound, shed target)` under pressure.
    fn evict_channel_bound(&self) -> u64 {
        if crate::mem_budget::level() >= crate::mem_budget::Level::Yellow {
            std::cmp::min(
                Self::EVICT_CHANNEL_BYTE_BOUND,
                self.evict_channel_shed_target
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
        } else {
            Self::EVICT_CHANNEL_BYTE_BOUND
        }
    }

    /// Victims dropped at the send side by the byte/slot bound.
    pub fn evict_channel_drops(&self) -> u64 {
        self.evict_channel_drops
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Receiver-side credit: the dehydration worker calls this once per
    /// handled message with the payload length.
    pub fn evict_channel_sub(&self, len: u64) {
        let _ = self.evict_channel_bytes.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |v| Some(v.saturating_sub(len)),
        );
    }

    /// Builder toggle for the hot-block tier (source-drop of probation
    /// victims — see `drop_probation_evictions`).
    pub fn with_drop_probation_evictions(mut self) -> Self {
        self.drop_probation_evictions = true;
        self
    }

    /// Builder: attach the scan-resistance admission governor (hot-block
    /// tier only — see `admission_governor`).
    pub fn with_admission_governor(
        mut self,
        governor: Arc<crate::routing::AdmissionGovernor>,
    ) -> Self {
        self.admission_governor = Some(governor);
        self
    }

    /// Retrieve the eviction receiver. Can only be taken once; taking it
    /// is what ARMS the send side (victims of worker-less caches drop at
    /// the source instead of parking in an unconsumed channel).
    pub fn take_evict_rx(&self) -> Option<EvictReceiver> {
        let rx = self.evict_rx.lock().ok()?.take();
        if rx.is_some() {
            self.evict_rx_taken
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        rx
    }

    /// Retrieve an entry from the cache, updating its clock status and
    /// promoting probation entries (block-level re-access).
    pub fn get(&self, key: &str) -> Option<Bytes> {
        self.inner.get(key.as_bytes())
    }

    /// Non-promoting get — stream sub-read consumption (§5.3): the entry
    /// keeps its clock second chance but is never marked keep-worthy by
    /// consuming its own sub-ranges.
    pub fn get_no_promote(&self, key: &str) -> Option<Bytes> {
        self.inner.get_no_promote(key.as_bytes())
    }

    /// Non-promoting get WITH payback credit — real reader serves pass
    /// the user bytes they served so the entry's eventual eviction can be
    /// classified earned-vs-waste by the admission governor. Probes and
    /// residency checks must use [`Self::get_no_promote`] (credit 0).
    pub fn get_serving(&self, key: &str, served_bytes: u64) -> Option<Bytes> {
        self.inner.get_serving(key.as_bytes(), served_bytes)
    }

    /// Insert an entry as PROTECTED (the pre-R4 semantics — every put via
    /// this API is a keep-worthy entry by definition), executing Clock
    /// eviction if maximum capacity is exceeded.
    ///
    /// P2-7: dehydration `try_send` runs **only** when Clock actually evicts
    /// entries (empty eviction vector is a pure no-op — no channel traffic).
    pub fn put(&self, key: &str, data: Bytes) {
        self.put_with(key, data, PutClass::Protected);
    }

    /// Insert as PROBATION (R4 §5.4): first in eviction line, promoted in
    /// place (sticky) by any `get`. Streaming/one-pass fills use this so a
    /// scan cannot displace protected warmth.
    pub fn put_probationary(&self, key: &str, data: Bytes) {
        self.put_with(key, data, PutClass::Probation);
    }

    /// Probation class WITH the one-lap clock grace (§5.5 pipeline fills):
    /// clock parity with consumed stream residue — see
    /// [`crate::tiering::memory::MemoryCache::put_probationary_referenced`].
    pub fn put_probationary_referenced(&self, key: &str, data: Bytes) {
        self.put_with(key, data, PutClass::ProbationReferenced);
    }

    /// [`crate::tiering::memory::MemoryCache::put_protected_stream`] —
    /// the transient stream window's GRANTED arm (2026-07-29).
    pub fn put_protected_stream(&self, key: &str, data: Bytes) {
        self.put_with(key, data, PutClass::ProtectedStream);
    }

    /// R5 Red clamp (§5.7): force-evict toward `target` bytes. Victims are
    /// DROPPED at the source (counted) — never dehydrated: the Yellow
    /// dehydration pause is already active below Red, and a clamp that
    /// queued multi-MiB payloads would re-create the pressure it sheds.
    pub fn shed_to(&self, target: u64) {
        for (_k, v, class) in self.inner.shed_to(target as usize) {
            if let Some(gov) = &self.admission_governor {
                gov.on_eviction(v.len() as u64, &class);
            }
            crate::fuse_client::METRICS
                .hot_block_evictions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if matches!(class, crate::tiering::memory::EvictClass::Probation) {
                crate::fuse_client::METRICS
                    .hot_block_probation_drops
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    fn put_with(&self, key: &str, data: Bytes, class: PutClass) {
        if (data.len() as u64) <= self.max_bytes {
            // R5 Yellow+ (§5.7 "stop growth: hot-tier inserts evict-first"):
            // net-zero growth — after the insert, clamp back to the
            // pre-insert size (one relaxed level load on the put path).
            let freeze_at = if crate::mem_budget::level() >= crate::mem_budget::Level::Yellow {
                Some(self.inner.current_bytes() as u64)
            } else {
                None
            };
            let key_bytes = Bytes::copy_from_slice(key.as_bytes());
            let mut evicted = match class {
                PutClass::Protected => self.inner.put(key_bytes, data),
                PutClass::ProtectedStream => self.inner.put_protected_stream(key_bytes, data),
                PutClass::Probation => self.inner.put_probationary(key_bytes, data),
                PutClass::ProbationReferenced => {
                    self.inner.put_probationary_referenced(key_bytes, data)
                }
            };
            if let Some(pre) = freeze_at {
                evicted.extend(self.inner.shed_to(pre as usize));
            }
            if evicted.is_empty() {
                return;
            }
            for (ek, ev, class) in evicted {
                // Scan resistance: report every victim's payback verdict
                // to the governor at the SOURCE (before any channel/drop
                // routing — victims dropped by the byte bound must still
                // count their waste).
                if let Some(gov) = &self.admission_governor {
                    gov.on_eviction(ev.len() as u64, &class);
                }
                // R1b liveness (the PR 4 bench OOM): probation victims of a
                // drop-probation cache are one-pass residue — dropping them
                // AT THE SOURCE keeps multi-MiB `Bytes` from ever parking
                // in the eviction channel (16384 slots × 4 MiB blocks was
                // an 8 GiB cgroup kill under a cold stream).
                if self.drop_probation_evictions
                    && matches!(class, crate::tiering::memory::EvictClass::Probation)
                {
                    crate::fuse_client::METRICS
                        .hot_block_evictions
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    crate::fuse_client::METRICS
                        .hot_block_probation_drops
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
                // R5 byte bound: past it the victim is DROPPED here — a
                // full channel of live multi-MiB Bytes is the cage-OOM
                // shape, and dehydration is best-effort warmth.
                let len = ev.len() as u64;
                if !self
                    .evict_rx_taken
                    .load(std::sync::atomic::Ordering::Relaxed)
                    || self.evict_channel_bytes() + len > self.evict_channel_bound()
                {
                    self.evict_channel_drops
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
                // Keys inserted via this API are always valid UTF-8 path/block ids.
                let k_str = match String::from_utf8(ek.to_vec()) {
                    Ok(s) => s,
                    Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
                };
                if self.evict_tx.try_send((k_str, ev, class)).is_ok() {
                    self.evict_channel_bytes
                        .fetch_add(len, std::sync::atomic::Ordering::Relaxed);
                } else {
                    self.evict_channel_drops
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        } else {
            // Do not leave a smaller stale entry under this key when the new
            // payload exceeds the cache budget (e.g. layout growth RMW).
            self.inner.remove(key.as_bytes());
        }
    }

    pub fn remove(&self, key: &str) {
        self.inner.remove(key.as_bytes());
    }

    pub fn current_bytes(&self) -> u64 {
        self.inner.current_bytes() as u64
    }

    /// Live entry count (the per-shard gauge; drift-pinned by
    /// `tests/memory_shard_tombstone_tests.rs`).
    pub fn inner_len(&self) -> usize {
        self.inner.len()
    }

    /// O(shards) emptiness over the entry gauge.
    pub fn inner_is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// RES-10: eviction-queue nodes across the shards (the
    /// tombstone-backlog probe — the byte gauge counts payload only).
    pub fn eviction_queue_len(&self) -> usize {
        self.inner.eviction_queue_len()
    }

    pub fn keys(&self) -> Vec<String> {
        self.inner
            .keys()
            .into_iter()
            .filter_map(|k| String::from_utf8(k.to_vec()).ok())
            .collect()
    }

    /// VAL-7a: live entry count — the `.stats` gauge that stays
    /// unconditional now that [`Self::keys`] rides the opt-in census.
    pub fn len(&self) -> usize {
        self.inner.entry_count()
    }

    /// Clippy's `len_without_is_empty` companion.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn run_pending_tasks(&self) {
        // MemoryCache operations are immediate, so this is a no-op kept for API compatibility.
    }

    pub fn clear(&self) {
        self.inner.clear();
    }

    /// Expose the number of shards for testing.
    pub fn num_shards(&self) -> usize {
        self.inner.num_shards()
    }
}
