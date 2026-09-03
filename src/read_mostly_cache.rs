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
//! [`scc::HashIndex`]; reads are [`scc::HashIndex::peek_with`] — pure loads
//! under an EBR guard, ZERO shared-line writes (the arc-swap read-mostly-
//! snapshot class: the D7 fold-overlay / VL4b `PlacementTable` precedent,
//! per-key). Capacity + idle/lifetime eviction stay on a **moka policy
//! shell** (`Cache<K, ()>`) built with exactly the same capacity/TTI/TTL as
//! the moka value cache it replaces; the shell's eviction listener removes
//! the store entry **stamp-exactly** (the entry's own activity stamps must
//! agree the entry is idle — a racing fresh insert is never clobbered by a
//! stale eviction verdict) and under the **pin predicate** (a `layout_dirty`
//! metadata entry is the ONLY authority for acked layout state — the pin
//! keeps it in the store; strictly STRONGER than the moka posture, where a
//! capacity/TTI eviction could drop it). A/B lever:
//! `SQUEEZEFS_READ_MOSTLY_CACHE=0` constructs the classic moka value cache
//! verbatim (the control arm).
//!
//! **Mechanism 2 — the sampled policy touch.** TTI caches (the metadata
//! cache) keep the access-refreshed-residency law — "any observed entry
//! stays resident" — without per-read recording: each entry carries a CAS'd
//! coarse touch stamp, and at most once per horizon (derived
//! [`derived_touch_secs`] = TTI/4, so an entry active at least once per
//! horizon keeps residency with 4× margin) one read OR insert performs one
//! policy-shell op (moka's recording, now at ~1/75 s per hot ino instead of
//! per op). The same stamp elides the policy half of warm-path INSERTS —
//! the warm write path republishes metadata per op, and the counted PERF-12
//! attribution (`tests/kernel_op_economy_tests.rs`) put the scc store at
//! **24.0 allocs/op vs the 30-budget moka arm** with the policy insert
//! costing ~6/op when un-elided. `SQUEEZEFS_CACHE_TOUCH_SECS=0` is the
//! isolation lever — every read/insert pays the policy op (un-sampled
//! recording through the shell).
//!
//! TTL caches (`stream_lanes`, `attr_cache`) filter expiry AT READ against
//! the entry's insert stamp (moka's observable `get` semantics) on the
//! coarse clock (`CLOCK_MONOTONIC_COARSE`, ±1 tick on 30 s/300 s horizons);
//! their inserts always pay the policy op (they are not warm-path), and
//! physical removal rides the policy shell's own lazy maintenance, exactly
//! like moka's.
//!
//! # The listener protocol (why nothing is orphaned or clobbered)
//!
//! Policy payments happen at least once per horizon of continuous activity
//! (the elision carries the old stamp forward and pays when it ages out),
//! so the policy's recency lags true activity by at most one horizon. The
//! listener therefore removes on `idle ≥ TTI − horizon` measured against
//! the ENTRY's own stamps: an entry whose activity genuinely stopped always
//! satisfies it (policy fires ≥ TTI after its last payment, which is within
//! one horizon of the last activity), and an entry a RACING op just
//! refreshed always fails it (never clobbered). Any decline resets the
//! entry's stamp to the stale marker, so the very next read/insert pays the
//! policy op and restores presence — a declined entry with zero future
//! activity is the one accepted residue (≈ one value held until
//! remove/invalidate_all; reachable only by an eviction racing that ino's
//! final-ever op inside one horizon).
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
//!
//! [`derived_touch_secs`]: crate::read_mostly_cache::derived_touch_secs
//! [`ReadMostlyCache::insert`]: crate::read_mostly_cache::ReadMostlyCache::insert
//! [`ReadMostlyCache::remove`]: crate::read_mostly_cache::ReadMostlyCache::remove

use std::hash::{BuildHasher, Hash};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The mech-2 sampling horizon derivation: TTI/4 — an entry active at least
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
    // +1 keeps 0 free as the STALE marker (the listener's decline reset).
    (ts.tv_sec as u64) * 1_000 + (ts.tv_nsec as u64) / 1_000_000 + 1
}

/// Injectable clock (tests step it; production is `coarse_now_ms`).
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
    /// `Some(0)` = pay the policy op on every read/insert (the isolation
    /// lever). Meaningless without `tti`.
    pub touch_secs: Option<u64>,
    /// The dirty pin (metadata: `|m| m.layout_dirty`).
    pub pin: Option<RmPin<V>>,
    /// Test clock; `None` = the coarse monotonic clock.
    pub clock: Option<RmClock>,
}

/// Store entry: the value plus the coherence-protocol bookkeeping. All of
/// it is entry-local — the `touched` stamp is the ONLY mutable word, CAS'd
/// at most once per horizon per entry (advisory: a lost race costs one
/// extra policy op, never correctness). The stamp is an INLINE atomic
/// (never `Arc`'d): the warm write path inserts per op, and a heap-
/// allocated stamp was a counted PERF-12 budget regression (+1.33
/// allocs/op, caught by `tests/kernel_op_economy_tests.rs`). `0` = the
/// stale marker (the next activity must pay the policy op).
struct Rm<V> {
    v: V,
    /// Insert instant (coarse ms) — the TTL read filter's basis and half
    /// of the listener's activity check.
    inserted_ms: u64,
    /// Last policy-payment instant (coarse ms) — mech-2's sampling state.
    touched: AtomicU64,
}

impl<V: Clone> Clone for Rm<V> {
    fn clone(&self) -> Self {
        // Snapshot copy (scc bucket moves): the stamp is advisory — a
        // moved entry keeps its last-payment instant, never resets it.
        Self {
            v: self.v.clone(),
            inserted_ms: self.inserted_ms,
            touched: AtomicU64::new(self.touched.load(Ordering::Relaxed)),
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
        policy: moka::sync::Cache<K, (), H>,
        ttl_ms: Option<u64>,
        /// `Some(horizon_ms)` = TTI touch sampling armed (0 = every op).
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
                ttl_ms,
                touch_ms,
                clock,
            } => Backing::ReadMostly {
                store: store.clone(),
                policy: policy.clone(),
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
        // The payment horizon derives from the cache's residency horizon
        // (TTI or TTL — whichever the cache rides), ÷4. For TTL caches the
        // policy shell only schedules PHYSICAL cleanup — the observable
        // expiry is the store stamp's read filter — so eliding their
        // warm-path policy inserts lags reclamation by ≤ one horizon and
        // changes nothing a reader can see (the attr cache's per-write
        // insert was the counted +6.4 allocs/op PERF-12 face).
        let touch_ms = tti.or(ttl).map(|d| {
            let secs = touch_secs
                .or_else(cache_touch_secs_override)
                .unwrap_or_else(|| derived_touch_secs(d.as_secs()));
            secs * 1_000
        });
        let ttl_ms = ttl.map(|d| d.as_millis() as u64);
        // The listener's idle threshold: the policy's recency lags true
        // activity by at most one horizon (payment cadence), so the
        // stamp-exact check is `idle ≥ horizon_base − horizon`.
        let idle_threshold_ms = tti
            .or(ttl)
            .map(|d| (d.as_millis() as u64).saturating_sub(touch_ms.unwrap_or(0)))
            .unwrap_or(0);
        let listener_store = store.clone();
        let listener_clock: RmClock = clock.clone().unwrap_or_else(|| Arc::new(coarse_now_ms));
        let mut b = moka::sync::Cache::builder()
            .max_capacity(capacity)
            .eviction_listener(move |k: Arc<K>, (), cause| {
                use moka::notification::RemovalCause;
                // Replaced/Explicit: the wrapper already updated the store
                // itself (insert/remove) — only the policy's OWN verdicts
                // (idle-out, lifetime expiry, capacity) reach the store.
                if !matches!(cause, RemovalCause::Expired | RemovalCause::Size) {
                    return;
                }
                let now = listener_clock();
                let mut pinned = false;
                listener_store.remove_if_sync(k.as_ref(), |e| {
                    let activity = e.inserted_ms.max(e.touched.load(Ordering::Relaxed));
                    if now.saturating_sub(activity) < idle_threshold_ms {
                        // A racing op refreshed this entry after the
                        // policy's eviction decision: never clobber it.
                        // Reset the stamp so its next read/insert pays
                        // the policy op and restores presence.
                        e.touched.store(0, Ordering::Relaxed);
                        return false;
                    }
                    if let Some(pin) = &pin {
                        if pin(&e.v) {
                            pinned = true;
                            e.touched.store(0, Ordering::Relaxed);
                            return false;
                        }
                    }
                    true
                });
                if pinned {
                    // The dirty pin engaged: the entry stays in the store
                    // with no policy presence until its next activity (the
                    // persist cadence's clean re-insert pays — the stamp
                    // was reset). Gauge, must be ≈ 0 in steady state.
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
        log::debug!(
            "read-mostly cache '{name}' armed (capacity {capacity}, tti {tti:?}, ttl {ttl:?}, \
             touch_ms {touch_ms:?}, idle_threshold_ms {idle_threshold_ms})"
        );
        Self {
            backing: Backing::ReadMostly {
                store,
                policy,
                ttl_ms,
                touch_ms,
                clock: clock.unwrap_or_else(|| Arc::new(coarse_now_ms)),
            },
        }
    }

    /// The unexpired screen + the mech-2 stamp election. Entry-local state
    /// only (pure loads + at most one advisory CAS); returns `(admit,
    /// pay_policy)` — the policy op itself runs OUTSIDE the store peek.
    fn admit_read(e: &Rm<V>, ttl_ms: Option<u64>, touch_ms: Option<u64>, now: u64) -> (bool, bool) {
        if let Some(ttl) = ttl_ms {
            if now.saturating_sub(e.inserted_ms) >= ttl {
                return (false, false);
            }
        }
        if let Some(horizon) = touch_ms {
            let last = e.touched.load(Ordering::Relaxed);
            if (last == 0 || now.saturating_sub(last) >= horizon)
                && e.touched
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                return (true, true);
            }
        }
        (true, false)
    }

    /// The elected policy payment (mech-2): one moka `insert` per horizon
    /// per hot entry — refreshes the shell's recency AND restores presence
    /// if the shell already evicted (the listener-decline re-arm).
    fn pay_policy(policy: &moka::sync::Cache<K, (), H>, key: K) {
        policy.insert(key, ());
        crate::fuse_client::METRICS
            .read_mostly_policy_touches
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Owned read (moka-`get` parity: one value clone).
    pub fn get<Q>(&self, key: &Q) -> Option<V>
    where
        Q: Hash + Eq + ?Sized + scc::Equivalent<K> + ToOwned<Owned = K>,
        K: std::borrow::Borrow<Q>,
    {
        self.peek_with(key, |v| v.clone())
    }

    /// Borrowed read — the hot serve-prelude form: ZERO value clones, pure
    /// loads under an EBR guard. `f` must be short and non-blocking (it
    /// runs under the store's read protection).
    pub fn peek_with<Q, R>(&self, key: &Q, f: impl FnOnce(&V) -> R) -> Option<R>
    where
        Q: Hash + Eq + ?Sized + scc::Equivalent<K> + ToOwned<Owned = K>,
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
            } => {
                let now = if ttl_ms.is_some() || touch_ms.is_some() {
                    clock()
                } else {
                    0
                };
                let (out, pay) = match store.peek_with(key, |_, e| {
                    let (admit, pay) = Self::admit_read(e, *ttl_ms, *touch_ms, now);
                    (admit.then(|| f(&e.v)), pay)
                }) {
                    Some((out, pay)) => (out, pay),
                    None => (None, false),
                };
                if pay {
                    Self::pay_policy(policy, key.to_owned());
                }
                out
            }
            // The control arm pays the clone the old call sites paid.
            Backing::Classic(c) => c.get(key).map(|v| f(&v)),
        }
    }

    pub fn insert(&self, key: K, value: V) {
        match &self.backing {
            Backing::ReadMostly {
                store,
                policy,
                touch_ms,
                clock,
                ..
            } => {
                let now = clock();
                let mut pay = true;
                match store.entry_sync(key.clone()) {
                    scc::hash_index::Entry::Occupied(mut o) => {
                        // Warm-path elision (mech-2's insert half): within
                        // the horizon the policy op is skipped and the old
                        // payment stamp carries forward — payment happens
                        // at least once per horizon of continuous activity.
                        let touched = match touch_ms {
                            Some(h) => {
                                let t = o.get().touched.load(Ordering::Relaxed);
                                if t != 0 && now.saturating_sub(t) < *h {
                                    pay = false;
                                    t
                                } else {
                                    now
                                }
                            }
                            None => now,
                        };
                        o.update(Rm {
                            v: value,
                            inserted_ms: now,
                            touched: AtomicU64::new(touched),
                        });
                    }
                    scc::hash_index::Entry::Vacant(slot) => {
                        slot.insert_entry(Rm {
                            v: value,
                            inserted_ms: now,
                            touched: AtomicU64::new(now),
                        });
                    }
                }
                if pay {
                    policy.insert(key, ());
                }
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
                ttl_ms,
                clock,
                ..
            } => {
                let now = clock();
                let expired =
                    |e: &Rm<V>| ttl_ms.is_some_and(|t| now.saturating_sub(e.inserted_ms) >= t);
                let v = match store.entry_sync(key.clone()) {
                    scc::hash_index::Entry::Occupied(mut o) => {
                        if !expired(o.get()) {
                            return o.get().v.clone();
                        }
                        let v = init();
                        o.update(Rm {
                            v: v.clone(),
                            inserted_ms: now,
                            touched: AtomicU64::new(now),
                        });
                        v
                    }
                    scc::hash_index::Entry::Vacant(slot) => {
                        let v = init();
                        slot.insert_entry(Rm {
                            v: v.clone(),
                            inserted_ms: now,
                            touched: AtomicU64::new(now),
                        });
                        v
                    }
                };
                // The policy op runs OUTSIDE the store guard — the module
                // law, and load-bearing: moka::sync delivers its eviction
                // listener INLINE on the paying thread, and the listener
                // takes store bucket locks (`remove_if_sync`). Paying
                // while the entry guard above was live self-deadlocked
                // the thread on its own bucket (same key ⇒ same bucket —
                // the 2026-08-10 LTP writev03 handler-lane wedge; repro
                // `expired_reinsert_never_deadlocks_on_inline_listener`).
                // Both match arms end their guard at the match boundary.
                policy.insert(key, ());
                v
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
