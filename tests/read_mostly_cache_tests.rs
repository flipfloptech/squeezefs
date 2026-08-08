//! L3 coherence campaign (2026-08-08, `.benchmarks/2026-08-08-moka-coherence.md`):
//! contracts for `ReadMostlyCache` — the read-mostly store + moka policy
//! shell that deletes moka's PER-READ bookkeeping (read-ring push, TinyLFU
//! sketch touch, timestamp CAS, LRU deque maintenance) from the serve
//! prelude. The field profile priced that bookkeeping at 54.6 %/33.4 % of
//! svc/dd cycles on the 2-socket box (cross-socket coherence on ~32 hot
//! inos × 24 threads); reads here are `scc::HashIndex::peek_with` — pure
//! loads under an EBR guard, ZERO shared-line writes.
//!
//! The laws pinned here:
//! 1. Visibility: an insert/remove is observable by the very next read on
//!    any thread (the "write path updates RAM synchronously" authority the
//!    §5.5.1 probe and the 795 revalidate ride).
//! 2. TTL caches filter expiry AT READ (moka's observable get semantics),
//!    and an expired entry's `get_or_insert_with` constructs FRESH.
//! 3. TTI caches keep the access-refreshed-residency law via the SAMPLED
//!    policy touch: an entry read at least once per horizon (derived
//!    TTI/4) never idles out; with sampling effectively off, reads do not
//!    extend residency and the policy evicts on its own clock.
//! 4. The dirty pin: a policy eviction may never drop a store entry the
//!    pin predicate holds (a `layout_dirty` entry is the ONLY layout
//!    authority — stronger than the moka posture, where a capacity/TTI
//!    eviction could drop acked layout state).
//! 5. Classic mode (`read_mostly: false`) delegates to moka verbatim —
//!    the A/B lever's control arm.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use squeezefs::read_mostly_cache::{ReadMostlyCache, RmConfig};

/// Stepping test clock (ms) — injected so TTL laws need no wall sleeps.
fn test_clock() -> (Arc<AtomicU64>, Arc<dyn Fn() -> u64 + Send + Sync>) {
    let t = Arc::new(AtomicU64::new(1_000));
    let tc = t.clone();
    (t, Arc::new(move || tc.load(Ordering::Relaxed)))
}

fn rm_config<V>(read_mostly: bool) -> RmConfig<V> {
    RmConfig {
        name: "test",
        read_mostly,
        capacity: 10_000,
        tti: None,
        ttl: None,
        touch_secs: None,
        pin: None,
        clock: None,
    }
}

/// Clone-counting value: `peek_with` must not clone; `get` clones once.
#[derive(Debug)]
struct CountedVal {
    n: u64,
    clones: Arc<AtomicUsize>,
}

impl Clone for CountedVal {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self {
            n: self.n,
            clones: self.clones.clone(),
        }
    }
}

#[test]
fn insert_get_remove_roundtrip_both_modes() {
    for read_mostly in [true, false] {
        let c: ReadMostlyCache<u64, u64, ahash::RandomState> =
            ReadMostlyCache::new(rm_config(read_mostly), ahash::RandomState::new());
        assert_eq!(c.get(&7), None, "read_mostly={read_mostly}");
        c.insert(7, 70);
        assert_eq!(c.get(&7), Some(70), "read_mostly={read_mostly}");
        c.insert(7, 71);
        assert_eq!(
            c.get(&7),
            Some(71),
            "insert must replace (read_mostly={read_mostly})"
        );
        c.remove(&7);
        assert_eq!(
            c.get(&7),
            None,
            "remove must be observable immediately (read_mostly={read_mostly})"
        );
        // `invalidate` is the attr-cache spelling of remove.
        c.insert(9, 90);
        c.invalidate(&9);
        assert_eq!(c.get(&9), None, "read_mostly={read_mostly}");
    }
}

#[test]
fn borrowed_key_get_on_string_cache() {
    for read_mostly in [true, false] {
        let c: ReadMostlyCache<String, u64, std::hash::RandomState> =
            ReadMostlyCache::new(rm_config(read_mostly), std::hash::RandomState::new());
        c.insert("inode:42".to_string(), 42);
        // The stream-lanes warm path probes with a borrowed &str — no key mint.
        assert_eq!(c.get("inode:42"), Some(42), "read_mostly={read_mostly}");
        assert_eq!(
            c.peek_with("inode:42", |v| *v),
            Some(42),
            "read_mostly={read_mostly}"
        );
    }
}

#[test]
fn peek_with_never_clones_get_clones_once() {
    let clones = Arc::new(AtomicUsize::new(0));
    let c: ReadMostlyCache<u64, CountedVal, ahash::RandomState> =
        ReadMostlyCache::new(rm_config(true), ahash::RandomState::new());
    c.insert(
        1,
        CountedVal {
            n: 5,
            clones: clones.clone(),
        },
    );
    let after_insert = clones.load(Ordering::Relaxed);
    // The hot serve-prelude form: borrowed predicate, zero value clones.
    assert_eq!(c.peek_with(&1, |v| v.n), Some(5));
    assert_eq!(
        clones.load(Ordering::Relaxed),
        after_insert,
        "peek_with must not clone the value (the zero-copy hot read)"
    );
    let got = c.get(&1).expect("resident");
    assert_eq!(got.n, 5);
    assert_eq!(
        clones.load(Ordering::Relaxed),
        after_insert + 1,
        "get clones exactly once (moka-get parity)"
    );
}

#[test]
fn get_or_insert_with_constructs_exactly_once_under_race() {
    for read_mostly in [true, false] {
        let c: Arc<ReadMostlyCache<String, u64, std::hash::RandomState>> = Arc::new(
            ReadMostlyCache::new(rm_config(read_mostly), std::hash::RandomState::new()),
        );
        let runs = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = c.clone();
            let runs = runs.clone();
            handles.push(std::thread::spawn(move || {
                c.get_or_insert_with("k".to_string(), || {
                    runs.fetch_add(1, Ordering::Relaxed);
                    7
                })
            }));
        }
        for h in handles {
            assert_eq!(h.join().expect("no panic"), 7);
        }
        assert_eq!(
            runs.load(Ordering::Relaxed),
            1,
            "constructor must run exactly once (read_mostly={read_mostly})"
        );
    }
}

#[test]
fn ttl_expiry_is_filtered_at_read_with_fresh_reconstruct() {
    let (t, clock) = test_clock();
    let mut cfg = rm_config(true);
    cfg.ttl = Some(std::time::Duration::from_secs(30));
    cfg.clock = Some(clock);
    let c: ReadMostlyCache<String, u64, std::hash::RandomState> =
        ReadMostlyCache::new(cfg, std::hash::RandomState::new());
    c.insert("lane".to_string(), 1);
    t.fetch_add(29_000, Ordering::Relaxed);
    assert_eq!(c.get("lane"), Some(1), "within TTL");
    assert_eq!(c.peek_with("lane", |v| *v), Some(1), "within TTL (peek)");
    t.fetch_add(2_000, Ordering::Relaxed); // 31 s after insert
    assert_eq!(c.get("lane"), None, "expired entries filter at read");
    assert_eq!(c.peek_with("lane", |v| *v), None, "expired (peek)");
    // The stream-lanes re-mint law: an expired entry's get_or_insert_with
    // constructs FRESH (moka get_with-on-expired parity) — never returns
    // the stale value.
    let v = c.get_or_insert_with("lane".to_string(), || 2);
    assert_eq!(v, 2, "expired entry must reconstruct, not serve stale");
    assert_eq!(c.get("lane"), Some(2));
}

#[test]
fn tti_policy_eviction_drops_unread_store_entry() {
    let mut cfg = rm_config(true);
    cfg.tti = Some(std::time::Duration::from_millis(200));
    // Horizon far past the TTI: reads never refresh residency here.
    cfg.touch_secs = Some(86_400);
    let c: ReadMostlyCache<u64, u64, ahash::RandomState> =
        ReadMostlyCache::new(cfg, ahash::RandomState::new());
    c.insert(1, 10);
    assert_eq!(c.get(&1), Some(10));
    // Idle past the TTI (time-based mechanism: the sleep IS the subject).
    std::thread::sleep(std::time::Duration::from_millis(450));
    c.run_policy_maintenance();
    assert_eq!(
        c.get(&1),
        None,
        "policy TTI eviction must drop the store entry (the leak rail)"
    );
    assert_eq!(c.entry_count(), 0);
}

#[test]
fn touch_every_read_keeps_entry_resident_across_tti() {
    // touch_secs = 0 (the mech-2 A/B isolation lever): every read refreshes
    // policy residency — the entry survives arbitrarily many TTIs while read.
    let mut cfg = rm_config(true);
    cfg.tti = Some(std::time::Duration::from_millis(300));
    cfg.touch_secs = Some(0);
    let c: ReadMostlyCache<u64, u64, ahash::RandomState> =
        ReadMostlyCache::new(cfg, ahash::RandomState::new());
    c.insert(1, 10);
    for _ in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(c.get(&1), Some(10), "reads at TTI/3 cadence keep it warm");
        c.run_policy_maintenance();
    }
    assert_eq!(
        c.get(&1),
        Some(10),
        "an entry read at least once per horizon never idles out"
    );
}

#[test]
fn dirty_pin_survives_policy_eviction_until_clean_insert() {
    #[derive(Clone, Debug, PartialEq)]
    struct Meta {
        v: u64,
        dirty: bool,
    }
    let mut cfg = rm_config(true);
    cfg.tti = Some(std::time::Duration::from_millis(200));
    cfg.touch_secs = Some(86_400); // reads never refresh — force evictions
    cfg.pin = Some(Arc::new(|m: &Meta| m.dirty));
    let c: ReadMostlyCache<u64, Meta, ahash::RandomState> =
        ReadMostlyCache::new(cfg, ahash::RandomState::new());
    c.insert(1, Meta { v: 1, dirty: true });
    std::thread::sleep(std::time::Duration::from_millis(450));
    c.run_policy_maintenance();
    assert_eq!(
        c.get(&1),
        Some(Meta { v: 1, dirty: true }),
        "a pinned (dirty) entry must survive policy eviction — it is the \
         only layout authority"
    );
    // The persist cadence's clean re-insert restores policy presence; the
    // NEXT idle-out may then drop it.
    c.insert(1, Meta { v: 2, dirty: false });
    assert_eq!(c.get(&1), Some(Meta { v: 2, dirty: false }));
    std::thread::sleep(std::time::Duration::from_millis(450));
    c.run_policy_maintenance();
    assert_eq!(
        c.get(&1),
        None,
        "a clean entry idles out exactly like the moka posture"
    );
}

#[test]
fn invalidate_all_clears_both_planes() {
    for read_mostly in [true, false] {
        let c: ReadMostlyCache<u64, u64, ahash::RandomState> =
            ReadMostlyCache::new(rm_config(read_mostly), ahash::RandomState::new());
        for i in 0..100u64 {
            c.insert(i, i);
        }
        c.invalidate_all();
        for i in 0..100u64 {
            assert_eq!(c.get(&i), None, "read_mostly={read_mostly}");
        }
    }
}

#[test]
fn entry_count_tracks_inserts_and_removes() {
    let c: ReadMostlyCache<u64, u64, ahash::RandomState> =
        ReadMostlyCache::new(rm_config(true), ahash::RandomState::new());
    for i in 0..50u64 {
        c.insert(i, i);
    }
    assert_eq!(c.entry_count(), 50);
    for i in 0..25u64 {
        c.remove(&i);
    }
    assert_eq!(c.entry_count(), 25);
}

#[test]
fn touch_horizon_default_derives_from_metadata_tti() {
    // Derivation tie test (drift-is-red): the mech-2 sampling horizon
    // derives as TTI/4 — an entry read at least once per horizon keeps
    // residency with 4x margin inside the 300 s TTI.
    assert_eq!(
        squeezefs::read_mostly_cache::derived_touch_secs(
            squeezefs::routing::METADATA_CACHE_TTI_SECS
        ),
        squeezefs::routing::METADATA_CACHE_TTI_SECS / 4,
    );
}

/// Multi-thread stress: values are never torn, removes/inserts/gets stay
/// individually atomic, and the read-mostly plane never serves a value
/// that was not inserted.
#[test]
fn concurrent_stress_values_never_torn() {
    #[derive(Clone)]
    struct Pair {
        a: u64,
        b: u64,
    }
    let c: Arc<ReadMostlyCache<u64, Pair, ahash::RandomState>> = Arc::new(ReadMostlyCache::new(
        rm_config(true),
        ahash::RandomState::new(),
    ));
    let stop = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for w in 0..4u64 {
        let c = c.clone();
        let stop = stop.clone();
        handles.push(std::thread::spawn(move || {
            let mut i = 0u64;
            while stop.load(Ordering::Relaxed) == 0 {
                let v = w * 1_000_000 + i;
                c.insert(w % 8, Pair { a: v, b: !v });
                if i.is_multiple_of(7) {
                    c.remove(&(w % 8));
                }
                i += 1;
            }
        }));
    }
    for _ in 0..8 {
        let c = c.clone();
        let stop = stop.clone();
        handles.push(std::thread::spawn(move || {
            while stop.load(Ordering::Relaxed) == 0 {
                for k in 0..8u64 {
                    if let Some(p) = c.get(&k) {
                        assert_eq!(p.b, !p.a, "torn value observed");
                    }
                    c.peek_with(&k, |p| assert_eq!(p.b, !p.a, "torn peek"));
                }
            }
        }));
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    stop.store(1, Ordering::Relaxed);
    for h in handles {
        h.join().expect("no panics under stress");
    }
}
