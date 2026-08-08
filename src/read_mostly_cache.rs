//! L3 coherence campaign (2026-08-08, `.benchmarks/2026-08-08-moka-coherence.md`):
//! the read-mostly cache — a value store whose READS write nothing shared.
//!
//! # The term
//!
//! The field ingress profile (`.benchmarks/2026-08-08-field-ingress-profile.md`)
//! priced moka's PER-READ bookkeeping — read-ring push, TinyLFU frequency-
//! sketch touch, entry-timestamp CAS, LRU deque maintenance in
//! `do_run_pending_tasks` — at **54.6 % of svc / 33.4 % of dd cycles** on the
//! 2-socket field box (≈ 12 of the 17.96 µs/op svc service time at 706 k
//! IOPS): every one of those bookkeeping structures is a shared cache line,
//! and 24 threads hammering ~32 hot inos turn each RMW into a UPI ping-pong.
//! The 1-node local venue structurally underprices this (~7 % there). It is
//! a COHERENCE problem, not a hashing problem — the u64 caches already ride
//! ahash.
//!
//! # The mechanism (charter order, blast radius smallest first)
//!
//! **Mechanism 1 — the read-mostly store.** Values live in an
//! [`scc::HashIndex`]; reads are [`HashIndex::peek_with`] — pure loads under
//! an EBR guard, ZERO shared-line writes (the arc-swap read-mostly-snapshot
//! class: the D7 fold-overlay / VL4b `PlacementTable` precedent, per-key).
//! Capacity + idle/lifetime eviction stay on a **moka policy shell**
//! (`Cache<K, u64>` — the u64 is the insert generation) built with exactly
//! the same capacity/TTI/TTL as the moka value cache it replaces; the
//! shell's eviction listener removes the store entry **generation-exactly**
//! (a racing fresh insert is never clobbered by a stale eviction) and under
//! the **pin predicate** (a `layout_dirty` metadata entry is the ONLY
//! authority for acked layout state — the pin keeps it in the store until
//! the persist cadence's clean re-insert restores policy presence; strictly
//! STRONGER than the moka posture, where a capacity/TTI eviction could drop
//! it). A/B lever: `SQUEEZEFS_READ_MOSTLY_CACHE=0` constructs the classic
//! moka value cache verbatim (the control arm).
//!
//! **Mechanism 2 — the sampled policy touch.** TTI caches (the metadata
//! cache) keep the access-refreshed-residency law — "any observed entry
//! stays resident" — without per-read recording: each entry carries a
//! CAS'd coarse touch stamp, and at most once per horizon (derived
//! [`derived_touch_secs`] = TTI/4, so an entry read at least once per
//! horizon keeps residency with 4× margin) one read performs a policy-shell
//! `get` (moka's read recording, now at ~1/75 s per ino instead of per op).
//! `SQUEEZEFS_CACHE_TOUCH_SECS=0` is the isolation lever — every read
//! touches the policy (un-sampled recording through the shell).
//!
//! TTL caches (`stream_lanes`, `attr_cache`) filter expiry AT READ against
//! the entry's insert stamp (moka's observable `get` semantics) on the
//! coarse clock (`CLOCK_MONOTONIC_COARSE`, ±1 tick on 30 s/300 s horizons);
//! physical removal rides the policy shell's own lazy maintenance, exactly
//! like moka's.
//!
//! # Staleness composition (why the serves stay correct)
//!
//! The §5.5.1 serve prelude and the 795 revalidate never relied on moka
//! expiry — they serve any RESIDENT entry, because the write path updates
//! the RAM entry synchronously (D0 excludes remote writers). Residency
//! changes ride [`ReadMostlyCache::insert`]/[`ReadMostlyCache::remove`],
//! which publish to the SAME store the hot reads peek — one source, the
//! same happens-before edges as the moka cache they replace. The custody
//! epoch, fill incarnation, and the rebind ladder remain the backstops for
//! mid-DMA movement, unchanged.

use std::hash::{BuildHasher, Hash};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The mech-2 sampling horizon derivation: TTI/4 — an entry read at least
/// once per horizon keeps residency with 4× margin inside the TTI. Tie-
/// tested in `tests/read_mostly_cache_tests.rs` (drift-is-red).
pub fn derived_touch_secs(tti_secs: u64) -> u64 {
    tti_secs / 4
}

/// Coarse monotonic milliseconds (`CLOCK_MONOTONIC_COARSE`): a vdso-page
/// pair of loads, no rdtsc, no shared-line write — the r5 single-read
/// clock law's budget is not re-spent here. Granularity is the kernel
/// tick (typically 4 ms), against 30 s / 300 s horizons.
fn coarse_now_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime with a valid timespec out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_COARSE, &mut ts) };
    (ts.tv_sec as u64) * 1_000 + (ts.tv_nsec as u64) / 1_000_000
}

/// Injectable clock (tests step it; production is [`coarse_now_ms`]).
pub type RmClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Pin predicate: entries it holds survive policy eviction (the dirty pin).
pub type RmPin<V> = Arc<dyn Fn(&V) -> bool + Send + Sync>;

/// Construction config — one struct so every call site names its policy.
pub struct RmConfig<V = ()> {
    /// Cache name (logs/diagnostics).
    pub name: &'static str,
    /// `true` = the read-mostly backing (mechanism 1); `false` = classic
    /// moka value cache verbatim (the `SQUEEZEFS_READ_MOSTLY_CACHE=0` arm).
    pub read_mostly: bool,
    /// Max entries (the policy shell's capacity; same derivation as before).
    pub capacity: u64,
    /// Time-to-idle (access-refreshed residency — the metadata cache).
    pub tti: Option<Duration>,
    /// Time-to-live from insert (read-filtered — stream lanes, attrs).
    pub ttl: Option<Duration>,
    /// Mech-2 sampling horizon override, seconds; `None` = derived TTI/4;
    /// `Some(0)` = touch the policy on every read (the isolation lever).
    /// Meaningless without `tti`.
    pub touch_secs: Option<u64>,
    /// The dirty pin (metadata: `|m| m.layout_dirty`).
    pub pin: Option<RmPin<V>>,
    /// Test clock; `None` = the coarse monotonic clock.
    pub clock: Option<RmClock>,
}

/// Store entry: the value plus the coherence-protocol bookkeeping. All of
/// it is entry-local — the `touched` stamp is the ONLY mutable word, CAS'd
/// at most once per horizon per entry (advisory: a lost race costs one
/// extra policy touch, never correctness).
struct Rm<V> {
    v: V,
    /// Insert generation — the eviction listener's exactness token.
    gen: u64,
    /// Insert instant (coarse ms) — the TTL read filter's basis.
    inserted_ms: u64,
    /// Last policy-touch instant (coarse ms) — mech-2's sampling state.
    touched: Arc<AtomicU64>,
}

impl<V: Clone> Clone for Rm<V> {
    fn clone(&self) -> Self {
        Self {
            v: self.v.clone(),
            gen: self.gen,
            inserted_ms: self.inserted_ms,
            touched: self.touched.clone(),
        }
    }
}

enum Backing<K, V, H>
where
    K: 'static + Clone + Eq + Hash,
    V: 'static + Clone,
    H: BuildHasher + Clone,
{
    ReadMostly {
        store: Arc<scc::HashIndex<K, Rm<V>, H>>,
        policy: moka::sync::Cache<K, u64, H>,
        gen: Arc<AtomicU64>,
        ttl_ms: Option<u64>,
        /// `Some(horizon_ms)` = TTI touch sampling armed (0 = every read).
        touch_ms: Option<u64>,
        clock: RmClock,
    },
    Classic(moka::sync::Cache<K, V, H>),
}

/// See the module docs. `Clone` is cheap (Arc'd store + moka handles).
pub struct ReadMostlyCache<K, V, H = ahash::RandomState>
where
    K: 'static + Clone + Eq + Hash + Send + Sync,
    V: 'static + Clone + Send + Sync,
    H: BuildHasher + Clone + Send + Sync + 'static,
{
    backing: Backing<K, V, H>,
}

impl<K, V, H> Clone for ReadMostlyCache<K, V, H>
where
    K: 'static + Clone + Eq + Hash + Send + Sync,
    V: 'static + Clone + Send + Sync,
    H: BuildHasher + Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        let backing = match &self.backing {
            Backing::ReadMostly {
                store,
                policy,
                gen,
                ttl_ms,
                touch_ms,
                clock,
            } => Backing::ReadMostly {
                store: store.clone(),
                policy: policy.clone(),
                gen: gen.clone(),
                ttl_ms: *ttl_ms,
                touch_ms: *touch_ms,
                clock: clock.clone(),
            },
            Backing::Classic(c) => Backing::Classic(c.clone()),
        };
        Self { backing }
    }
}

/// The process-wide mech-1 posture (`SQUEEZEFS_READ_MOSTLY_CACHE`, default
/// ON — ruling D17: the bet posture from the mechanism analysis).
pub fn read_mostly_enabled() -> bool {
    crate::env_knobs::bool_knob("SQUEEZEFS_READ_MOSTLY_CACHE", true)
}

/// The process-wide mech-2 horizon (`SQUEEZEFS_CACHE_TOUCH_SECS`; explicit
/// wins verbatim, default = derived TTI/4 per cache).
pub fn cache_touch_secs_override() -> Option<u64> {
    crate::env_knobs::opt_int_knob::<u64>("SQUEEZEFS_CACHE_TOUCH_SECS")
}

impl<K, V, H> ReadMostlyCache<K, V, H>
where
    K: 'static + Clone + Eq + Hash + Send + Sync,
    V: 'static + Clone + Send + Sync,
    H: BuildHasher + Clone + Send + Sync + 'static,
{
    pub fn new(cfg: RmConfig<V>, hasher: H) -> Self {
        let RmConfig {
            name,
            read_mostly,
            capacity,
            tti,
            ttl,
            touch_secs,
            pin,
            clock,
        } = cfg;
        if !read_mostly {
            let mut b = moka::sync::Cache::builder().max_capacity(capacity);
            if let Some(tti) = tti {
                b = b.time_to_idle(tti);
            }
            if let Some(ttl) = ttl {
                b = b.time_to_live(ttl);
            }
            return Self {
                backing: Backing::Classic(b.build_with_hasher(hasher)),
            };
        }
        let store: Arc<scc::HashIndex<K, Rm<V>, H>> =
            Arc::new(scc::HashIndex::with_hasher(hasher.clone()));
        let listener_store = store.clone();
        let mut b = moka::sync::Cache::builder()
            .max_capacity(capacity)
            .eviction_listener(move |k: Arc<K>, evicted_gen: u64, cause| {
                use moka::notification::RemovalCause;
                // Replaced/Explicit: the wrapper already updated the store
                // itself (insert/remove) — only the policy's OWN verdicts
                // (idle-out, lifetime expiry, capacity) reach the store.
                if !matches!(cause, RemovalCause::Expired | RemovalCause::Size) {
                    return;
                }
                let mut pinned = false;
                let removed = listener_store.remove_if_sync(k.as_ref(), |e| {
                    if e.gen != evicted_gen {
                        // A fresh insert raced this eviction: never
                        // clobber it (generation exactness).
                        return false;
                    }
                    if let Some(pin) = &pin {
                        if pin(&e.v) {
                            pinned = true;
                            return false;
                        }
                    }
                    true
                });
                if pinned && !removed {
                    // The dirty pin engaged: the entry stays in the store
                    // with no policy presence until the persist cadence's
                    // clean re-insert restores it. Gauge, must be ≈ 0 in
                    // steady state (the persist cadence outruns the TTI).
                    crate::fuse_client::METRICS
                        .read_mostly_dirty_pins
                        .fetch_add(1, Ordering::Relaxed);
                }
            });
        if let Some(tti) = tti {
            b = b.time_to_idle(tti);
        }
        if let Some(ttl) = ttl {
            b = b.time_to_live(ttl);
        }
        let policy = b.build_with_hasher(hasher);
        let touch_ms = tti.map(|tti| {
            let secs = touch_secs
                .or_else(cache_touch_secs_override)
                .unwrap_or_else(|| derived_touch_secs(tti.as_secs()));
            secs * 1_000
        });
        log::debug!(
            "read-mostly cache '{name}' armed (capacity {capacity}, tti {tti:?}, ttl {ttl:?}, \
             touch_ms {touch_ms:?})"
        );
        Self {
            backing: Backing::ReadMostly {
                store,
                policy,
                gen: Arc::new(AtomicU64::new(1)),
                ttl_ms: ttl.map(|d| d.as_millis() as u64),
                touch_ms,
                clock: clock.unwrap_or_else(|| Arc::new(coarse_now_ms)),
            },
        }
    }

    /// The unexpired screen + the mech-2 sampled policy touch. Entry-local
    /// state only; the CAS is advisory (a lost race = one extra touch).
    fn admit_read(
        e: &Rm<V>,
        k: &K,
        policy: &moka::sync::Cache<K, u64, H>,
        ttl_ms: Option<u64>,
        touch_ms: Option<u64>,
        clock: &RmClock,
    ) -> bool {
        if ttl_ms.is_none() && touch_ms.is_none() {
            return true;
        }
        let now = clock();
        if let Some(ttl) = ttl_ms {
            if now.saturating_sub(e.inserted_ms) >= ttl {
                return false;
            }
        }
        if let Some(horizon) = touch_ms {
            let last = e.touched.load(Ordering::Relaxed);
            if now.saturating_sub(last) >= horizon
                && e.touched
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                // The ONE moka read this entry pays per horizon: refresh
                // the policy shell's idle clock (TTI residency).
                let _ = policy.get(k);
                crate::fuse_client::METRICS
                    .read_mostly_policy_touches
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        true
    }

    /// Owned read (moka-`get` parity: one value clone).
    pub fn get<Q>(&self, key: &Q) -> Option<V>
    where
        Q: Hash + Eq + ?Sized + scc::Equivalent<K>,
        K: std::borrow::Borrow<Q>,
    {
        match &self.backing {
            Backing::ReadMostly {
                store,
                policy,
                ttl_ms,
                touch_ms,
                clock,
                ..
            } => store
                .peek_with(key, |k, e| {
                    Self::admit_read(e, k, policy, *ttl_ms, *touch_ms, clock).then(|| e.v.clone())
                })
                .flatten(),
            Backing::Classic(c) => c.get(key),
        }
    }

    /// Borrowed read — the hot serve-prelude form: ZERO value clones, pure
    /// loads under an EBR guard. `f` must be short and non-blocking (it
    /// runs under the store's read protection).
    pub fn peek_with<Q, R>(&self, key: &Q, f: impl FnOnce(&V) -> R) -> Option<R>
    where
        Q: Hash + Eq + ?Sized + scc::Equivalent<K>,
        K: std::borrow::Borrow<Q>,
    {
        match &self.backing {
            Backing::ReadMostly {
                store,
                policy,
                ttl_ms,
                touch_ms,
                clock,
                ..
            } => store
                .peek_with(key, |k, e| {
                    Self::admit_read(e, k, policy, *ttl_ms, *touch_ms, clock).then(|| f(&e.v))
                })
                .flatten(),
            // The control arm pays the clone the old call sites paid.
            Backing::Classic(c) => c.get(key).map(|v| f(&v)),
        }
    }

    pub fn insert(&self, key: K, value: V) {
        match &self.backing {
            Backing::ReadMostly {
                store,
                policy,
                gen,
                clock,
                ..
            } => {
                let g = gen.fetch_add(1, Ordering::Relaxed);
                let now = clock();
                let entry = Rm {
                    v: value,
                    gen: g,
                    inserted_ms: now,
                    touched: Arc::new(AtomicU64::new(now)),
                };
                match store.entry_sync(key.clone()) {
                    scc::hash_index::Entry::Occupied(mut o) => o.update(entry),
                    scc::hash_index::Entry::Vacant(v) => {
                        v.insert_entry(entry);
                    }
                }
                policy.insert(key, g);
            }
            Backing::Classic(c) => c.insert(key, value),
        }
    }

    /// `get_with` parity: at most one constructor run per key (racing
    /// callers converge on the winner's value); an EXPIRED entry
    /// reconstructs fresh (moka's get_with-on-expired behavior).
    pub fn get_or_insert_with(&self, key: K, init: impl FnOnce() -> V) -> V {
        match &self.backing {
            Backing::ReadMostly {
                store,
                policy,
                gen,
                ttl_ms,
                clock,
                ..
            } => {
                let now = clock();
                let expired =
                    |e: &Rm<V>| ttl_ms.is_some_and(|t| now.saturating_sub(e.inserted_ms) >= t);
                match store.entry_sync(key.clone()) {
                    scc::hash_index::Entry::Occupied(mut o) => {
                        if !expired(o.get()) {
                            return o.get().v.clone();
                        }
                        let g = gen.fetch_add(1, Ordering::Relaxed);
                        let v = init();
                        o.update(Rm {
                            v: v.clone(),
                            gen: g,
                            inserted_ms: now,
                            touched: Arc::new(AtomicU64::new(now)),
                        });
                        policy.insert(key, g);
                        v
                    }
                    scc::hash_index::Entry::Vacant(slot) => {
                        let g = gen.fetch_add(1, Ordering::Relaxed);
                        let v = init();
                        slot.insert_entry(Rm {
                            v: v.clone(),
                            gen: g,
                            inserted_ms: now,
                            touched: Arc::new(AtomicU64::new(now)),
                        });
                        policy.insert(key, g);
                        v
                    }
                }
            }
            Backing::Classic(c) => c.get_with(key, init),
        }
    }

    pub fn remove<Q>(&self, key: &Q)
    where
        Q: Hash + Eq + ?Sized + scc::Equivalent<K>,
        K: std::borrow::Borrow<Q>,
    {
        match &self.backing {
            Backing::ReadMostly { store, policy, .. } => {
                // Store first: a reader between the two sees a miss —
                // the same observable state as after both.
                store.remove_sync(key);
                policy.invalidate(key);
            }
            Backing::Classic(c) => {
                c.invalidate(key);
            }
        }
    }

    /// The attr-cache spelling.
    pub fn invalidate<Q>(&self, key: &Q)
    where
        Q: Hash + Eq + ?Sized + scc::Equivalent<K>,
        K: std::borrow::Borrow<Q>,
    {
        self.remove(key);
    }

    /// The R5 Red shed (refill-from-backend caches; posture unchanged).
    pub fn invalidate_all(&self) {
        match &self.backing {
            Backing::ReadMostly { store, policy, .. } => {
                store.clear_sync();
                policy.invalidate_all();
            }
            Backing::Classic(c) => c.invalidate_all(),
        }
    }

    /// Live entry population (the R5 gauge basis).
    pub fn entry_count(&self) -> u64 {
        match &self.backing {
            Backing::ReadMostly { store, .. } => store.len() as u64,
            Backing::Classic(c) => c.entry_count(),
        }
    }

    /// Drive the policy shell's pending maintenance (tests: deterministic
    /// eviction delivery; production relies on moka's own cadence).
    pub fn run_policy_maintenance(&self) {
        match &self.backing {
            Backing::ReadMostly { policy, .. } => policy.run_pending_tasks(),
            Backing::Classic(c) => c.run_pending_tasks(),
        }
    }
}
