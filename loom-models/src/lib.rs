//! Exhaustive loom model-checking of SqueezeFS's lock-free protocol cores.
//!
//! The modules under test are `#[path]`-included from `../src` — the models
//! check the exact shipped code, not a copy. Each protocol was extracted
//! into a dependency-free core module for this purpose:
//!
//! - [`alloc_core`]: the inode-bitmap allocator (`fetch_or` claim,
//!   `fetch_and` release, scan hint) — invariant: an inode is never handed
//!   to two concurrent callers, and occupancy popcount matches live claims.
//! - [`incarnation_core`]: the block-key incarnation seqlock (retire /
//!   publish / snapshot / validate) — invariant: a *validated* cache fill
//!   never publishes bytes from a different incarnation of a reused block
//!   key (the striped-write ABA cache-poisoning fix).
//! - [`gauge_core`]: the staged-budget saturating gauge — invariant: racing
//!   charges and credits settle exactly and never wrap below zero (a
//!   wrapped gauge reads as "staging pool full forever").
//!
//! Models run only under `--cfg loom` (see `tests/run_loom.sh`); a plain
//! `cargo test` here compiles the cores against std atomics and runs
//! nothing.

#[path = "../../src/meta_backend/alloc_core.rs"]
pub mod alloc_core;
#[path = "../../src/gauge_core.rs"]
pub mod gauge_core;
#[path = "../../src/incarnation_core.rs"]
pub mod incarnation_core;
#[path = "../../src/refcount_core.rs"]
pub mod refcount_core;

#[cfg(all(test, loom))]
mod models {
    use crate::{alloc_core, gauge_core, incarnation_core};
    use loom::sync::atomic::{AtomicU64, Ordering};
    use loom::sync::Arc;
    use loom::thread;

    /// Allocator invariant #1: two threads racing `alloc` on a small table
    /// never receive the same inode, and the final popcount equals the
    /// number of successful claims.
    #[test]
    fn alloc_never_double_allocates() {
        loom::model(|| {
            // limit 6 -> allocatable {2,3,4,5}.
            let a = Arc::new(alloc_core::AllocCore::new(6));

            let t1 = {
                let a = a.clone();
                thread::spawn(move || {
                    let x = a.alloc().ok().map(|o| o.ino);
                    let y = a.alloc().ok().map(|o| o.ino);
                    (x, y)
                })
            };
            let (x2, y2) = {
                let x = a.alloc().ok().map(|o| o.ino);
                let y = a.alloc().ok().map(|o| o.ino);
                (x, y)
            };
            let (x1, y1) = t1.join().unwrap();

            let claimed: Vec<u64> = [x1, y1, x2, y2].into_iter().flatten().collect();
            let mut dedup = claimed.clone();
            dedup.sort_unstable();
            dedup.dedup();
            assert_eq!(
                claimed.len(),
                dedup.len(),
                "an inode was handed to two threads: {claimed:?}"
            );
            assert_eq!(
                a.allocated_count(),
                claimed.len() as u64,
                "popcount diverged from live claims"
            );
        });
    }

    /// Allocator invariant #2: a `free` racing concurrent `alloc`s hands the
    /// slot to at most one new claimant; occupancy stays exact.
    #[test]
    fn alloc_free_reuse_is_exclusive() {
        loom::model(|| {
            // limit 4 -> allocatable {2,3}.
            let a = Arc::new(alloc_core::AllocCore::new(4));
            let first = a.alloc().expect("seed claim").ino;

            let t = {
                let a = a.clone();
                thread::spawn(move || {
                    let m = a.alloc().ok().map(|o| o.ino);
                    let n = a.alloc().ok().map(|o| o.ino);
                    (m, n)
                })
            };
            a.free(first);
            let (m, n) = t.join().unwrap();

            let mut live: Vec<u64> = [m, n].into_iter().flatten().collect();
            live.sort_unstable();
            let mut dedup = live.clone();
            dedup.dedup();
            assert_eq!(live.len(), dedup.len(), "freed slot double-claimed: {live:?}");
            assert_eq!(
                a.allocated_count(),
                live.len() as u64,
                "popcount diverged after free/alloc race"
            );
        });
    }

    /// Seqlock invariant: a fill that passes snapshot+validate never returns
    /// bytes from a different incarnation. The payload cell is modeled as an
    /// atomic that each writer stamps with its generation, so a torn/foreign
    /// observation is detectable.
    #[test]
    fn incarnation_validated_fill_never_serves_dead_bytes() {
        loom::model(|| {
            // Stable generation 0, payload stamped 0.
            let word = Arc::new(AtomicU64::new(incarnation_core::STABLE_FIRST));
            let payload = Arc::new(AtomicU64::new(0));

            let writer = {
                let word = word.clone();
                let payload = payload.clone();
                thread::spawn(move || {
                    // Reuse cycle: retire (free/realloc) -> device write ->
                    // publish.
                    incarnation_core::retire(&word);
                    payload.store(1, Ordering::Relaxed);
                    incarnation_core::publish(&word);
                })
            };

            // Reader = cache fill: snapshot, "device read", validate.
            if let Some(before) = incarnation_core::snapshot(&word) {
                let bytes = payload.load(Ordering::Relaxed);
                if incarnation_core::still(&word, before) {
                    let expected_gen = before >> 1;
                    assert_eq!(
                        bytes, expected_gen,
                        "validated fill published bytes from a dead incarnation \
                         (snapshot gen {expected_gen}, bytes stamped {bytes})"
                    );
                }
            }
            writer.join().unwrap();
        });
    }

    /// Seqlock invariant under two full reuse cycles racing one fill: the
    /// validated fill still always matches its snapshotted generation.
    #[test]
    fn incarnation_two_reuse_cycles_still_exact() {
        loom::model(|| {
            let word = Arc::new(AtomicU64::new(incarnation_core::STABLE_FIRST));
            let payload = Arc::new(AtomicU64::new(0));

            let writer = {
                let word = word.clone();
                let payload = payload.clone();
                thread::spawn(move || {
                    for generation in 1..=2u64 {
                        incarnation_core::retire(&word);
                        payload.store(generation, Ordering::Relaxed);
                        incarnation_core::publish(&word);
                    }
                })
            };

            if let Some(before) = incarnation_core::snapshot(&word) {
                let bytes = payload.load(Ordering::Relaxed);
                if incarnation_core::still(&word, before) {
                    assert_eq!(bytes, before >> 1, "fill crossed incarnations");
                }
            }
            writer.join().unwrap();
        });
    }

    /// Refcount invariant #1: an acquire racing the terminal release either
    /// takes a reference (and the releaser does NOT free) or observes zero
    /// (and must not use the block) — never both, never a resurrected count.
    #[test]
    fn refcount_acquire_never_resurrects_freed_block() {
        use loom::sync::atomic::AtomicU32;
        loom::model(|| {
            let cell = Arc::new(AtomicU32::new(1)); // one live owner

            let cloner = {
                let cell = cell.clone();
                thread::spawn(move || crate::refcount_core::try_acquire(&cell))
            };
            let freed = crate::refcount_core::release(&cell); // owner drops
            let acquired = cloner.join().unwrap();

            assert!(
                !(freed && acquired),
                "block freed while a clone simultaneously took a reference \
                 (resurrection: the clone now aliases a reallocatable offset)"
            );
            assert!(
                freed || acquired,
                "reference neither freed nor transferred — leaked"
            );
            if acquired {
                // The clone holds the only remaining reference; its release
                // must be the terminal one.
                assert!(crate::refcount_core::release(&cell), "clone's release must free");
            }
            assert!(!crate::refcount_core::release(&cell), "double free");
        });
    }

    /// Refcount invariant #2: two concurrent releases of a doubly-referenced
    /// block produce exactly one terminal transition.
    #[test]
    fn refcount_exactly_one_terminal_release() {
        use loom::sync::atomic::AtomicU32;
        loom::model(|| {
            let cell = Arc::new(AtomicU32::new(2));
            let t = {
                let cell = cell.clone();
                thread::spawn(move || crate::refcount_core::release(&cell))
            };
            let a = crate::refcount_core::release(&cell);
            let b = t.join().unwrap();
            assert!(
                a ^ b,
                "exactly one of two racing releases must observe the terminal 1->0"
            );
        });
    }

    /// Gauge invariant: paired charges/credits racing a stale over-credit
    /// settle at exactly zero — saturation clamps, never wraps (a wrapped
    /// gauge = staging pool "full forever").
    #[test]
    fn gauge_settles_exactly_and_never_wraps() {
        loom::model(|| {
            let g = Arc::new(AtomicU64::new(0));

            let t = {
                let g = g.clone();
                thread::spawn(move || {
                    g.fetch_add(4, Ordering::Relaxed);
                    gauge_core::sub_saturating(&g, 4);
                })
            };
            // Stale credit racing the pair (evicted-entry credit shape).
            gauge_core::sub_saturating(&g, 9);
            t.join().unwrap();

            // Every interleaving settles at exactly zero: the stale credit
            // either clamps against an empty gauge or consumes the charge
            // that its own pair then clamps against. Any wrap would leave a
            // huge value here.
            let end = g.load(Ordering::Relaxed);
            assert_eq!(end, 0, "gauge wrapped or leaked: {end}");
        });
    }
}
