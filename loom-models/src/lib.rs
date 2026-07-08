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
//! - [`cow_core`]: the exclusive-owner CoW active-block cell (get_mut-or-
//!   copy vs snapshot clone) — invariant: a snapshot's payload never
//!   changes for the snapshot's lifetime, and uniqueness observed after a
//!   remote snapshot drop mutates in place (the shared-`Bytes` write-merge
//!   UB fix, zero-copy write-path design §5.2).
//! - [`lease_core`]: the FUSE-over-io_uring payload-lease re-arm protocol
//!   (refs/parked publish-then-recheck word protocol, zero-copy write-path
//!   design §5.4, PR 5) — invariants: the COMMIT_AND_FETCH for an ent
//!   executes exactly once and never while a payload lease is live
//!   (including the shutdown header-only drain), and a parked commit is
//!   never lost to a missed eventfd wake.
//!
//! Models run only under `--cfg loom` (see `tests/run_loom.sh`); a plain
//! `cargo test` here compiles the cores against std atomics and runs
//! nothing.

#[path = "../../src/meta_backend/alloc_core.rs"]
pub mod alloc_core;
#[path = "../../src/cow_core.rs"]
pub mod cow_core;
#[path = "../../src/gauge_core.rs"]
pub mod gauge_core;
#[path = "../../src/incarnation_core.rs"]
pub mod incarnation_core;
#[path = "../../third_party/fuse3/src/raw/connection/lease_core.rs"]
pub mod lease_core;
#[path = "../../src/refcount_core.rs"]
pub mod refcount_core;

#[cfg(all(test, loom))]
mod models {
    use crate::{alloc_core, gauge_core, incarnation_core, lease_core};
    use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

    /// Allocator invariant #3 (PR 2 quarantine): a reserved range is never
    /// handed out under alloc races, `free` inside it is a no-op even racing
    /// concurrent claims, and `allocated_count` stays exactly the number of
    /// live (non-reserved) claims — claim-vs-reserved non-interference.
    #[test]
    fn alloc_reserved_range_never_handed_out() {
        loom::model(|| {
            // limit 8 -> allocatable {2..8}; reserve {4,5} -> claimable {2,3,6,7}.
            let mut core = alloc_core::AllocCore::new(8);
            core.reserve_range(4, 6);
            let a = Arc::new(core);

            let t = {
                let a = a.clone();
                thread::spawn(move || {
                    // Racing free()s of reserved inos must release nothing.
                    a.free(4);
                    let m = a.alloc().ok().map(|o| o.ino);
                    a.free(5);
                    let n = a.alloc().ok().map(|o| o.ino);
                    (m, n)
                })
            };
            let (x, y) = {
                let x = a.alloc().ok().map(|o| o.ino);
                let y = a.alloc().ok().map(|o| o.ino);
                (x, y)
            };
            let (m, n) = t.join().unwrap();

            let claimed: Vec<u64> = [x, y, m, n].into_iter().flatten().collect();
            for ino in &claimed {
                assert!(
                    *ino != 4 && *ino != 5,
                    "reserved ino handed out under race: {claimed:?}"
                );
            }
            let mut dedup = claimed.clone();
            dedup.sort_unstable();
            dedup.dedup();
            assert_eq!(
                claimed.len(),
                dedup.len(),
                "an inode was handed to two threads: {claimed:?}"
            );
            assert_eq!(claimed.len(), 4, "exactly {{2,3,6,7}} must be claimable");
            assert_eq!(
                a.allocated_count(),
                4,
                "popcount must exclude reserved bits and racing frees of them"
            );
            assert!(a.is_set(4) && a.is_set(5), "reserved bits must survive free()");
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

    /// Payload cell for the CoW models: a `loom::cell::UnsafeCell` stands in
    /// for the active block's raw memory, so loom's access tracking flags
    /// any unsynchronized read/write overlap (the exact shared-`Bytes` UB
    /// the protocol removes) in addition to the value assertions.
    struct CowPayload {
        cell: loom::cell::UnsafeCell<u64>,
    }

    impl CowPayload {
        fn new(v: u64) -> Self {
            Self {
                cell: loom::cell::UnsafeCell::new(v),
            }
        }

        fn read(&self) -> u64 {
            // SAFETY: shared read; loom verifies no concurrent mutable access.
            self.cell.with(|p| unsafe { *p })
        }

        fn write(&mut self, v: u64) {
            // SAFETY: exclusive access — reachable only through
            // `CowCell::owned_mut`, which proves the owning Arc unique.
            self.cell.with_mut(|p| unsafe { *p = v });
        }

        fn duplicate(&self) -> Self {
            Self::new(self.read())
        }
    }

    /// CoW invariant #1 (the P0 fix): a reader's snapshot payload never
    /// changes between two reads, no matter how its clone interleaves with
    /// a writer's get_mut-or-copy + mutate + publish. The slot mutex models
    /// the DashMap-entry/`BLOCK_FLUSH_LOCKS` serialization of *cell* access;
    /// payload reads run OUTSIDE it, like the read path's zero-copy slice.
    /// loom's `UnsafeCell` tracking additionally fails the model if any
    /// interleaving lets the writer mutate memory a reader is reading.
    #[test]
    fn cow_snapshot_never_changes_under_racing_writer() {
        loom::model(|| {
            let slot = Arc::new(loom::sync::Mutex::new(crate::cow_core::CowCell::new(
                CowPayload::new(0),
            )));

            let reader = {
                let slot = slot.clone();
                thread::spawn(move || {
                    let snap = slot.lock().unwrap().share();
                    let a = snap.read();
                    let b = snap.read();
                    (a, b)
                })
            };

            {
                let mut cell = slot.lock().unwrap();
                let (_copied, payload) = cell.owned_mut(CowPayload::duplicate);
                payload.write(1);
            }

            let (a, b) = reader.join().unwrap();
            assert_eq!(
                a, b,
                "snapshot payload changed between two reads (in-place \
                 mutation of shared memory)"
            );

            // The writer's publish must always be visible through the cell.
            let final_v = slot.lock().unwrap().peek().read();
            assert_eq!(final_v, 1, "writer's published payload lost");
        });
    }

    /// CoW invariant #2: uniqueness is *restored* once a remote snapshot
    /// drops — `Arc::drop`'s Release decrement must synchronize with
    /// `get_mut`'s Acquire check, so the writer mutates in place (no
    /// spurious copy) and sees its own write.
    #[test]
    fn cow_uniqueness_restored_after_remote_snapshot_drop() {
        loom::model(|| {
            let mut cell = crate::cow_core::CowCell::new(CowPayload::new(7));
            let snap = cell.share();
            let t = thread::spawn(move || snap.read()); // snap drops there
            let observed = t.join().unwrap();
            assert_eq!(observed, 7, "snapshot must read the seeded payload");

            let (copied, payload) = cell.owned_mut(CowPayload::duplicate);
            assert!(
                !copied,
                "get_mut failed to observe a join-synchronized snapshot drop"
            );
            payload.write(8);
            assert_eq!(cell.peek().read(), 8);
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

    /// Transport payload-lease invariant #1 (zero-copy write-path design
    /// §5.4, PR 5): thread A = lease drop (refs 1→0, parked check + wake),
    /// thread B = queue worker (refs load, park, publish-then-recheck).
    /// The COMMIT_AND_FETCH executes exactly once and never while a payload
    /// lease is live, and a parked commit is never lost to a missed wake —
    /// if the worker parks, the lease drop MUST observe `parked` and fire
    /// the eventfd (a silent park at `Q_DEPTH = 4` is a deterministic mount
    /// hang, not a slowdown).
    #[test]
    fn ent_lease_commit_exactly_once_never_while_leased() {
        loom::model(|| {
            let st = Arc::new(lease_core::EntLeaseState::new());
            // Delivery (queue worker, before any reply exists): the single
            // payload lease for this ent.
            assert_eq!(st.acquire(), 0, "fresh ent must be lease-free");

            let wake = Arc::new(AtomicBool::new(false));

            // Thread A: the handler's last payload `Bytes` clone drops.
            let dropper = {
                let st = Arc::clone(&st);
                let wake = Arc::clone(&wake);
                thread::spawn(move || {
                    if st.release() {
                        wake.store(true, Ordering::SeqCst);
                    }
                })
            };

            // Thread B (queue worker): the reply's CommitMsg arrives.
            let mut commits = 0u32;
            let parked = match st.try_commit() {
                lease_core::CommitGate::Ready => {
                    assert!(
                        !st.leased(),
                        "commit fired while the payload lease was live"
                    );
                    commits += 1;
                    false
                }
                lease_core::CommitGate::Parked => true,
            };

            dropper.join().unwrap();

            if parked {
                // Liveness: the lease has fully dropped by now, so the wake
                // MUST have fired — otherwise the parked commit sleeps until
                // an eventfd write that never comes (missed-wake deadlock).
                assert!(
                    wake.load(Ordering::SeqCst),
                    "missed wake: commit parked but the lease drop saw parked == false"
                );
                assert!(
                    st.try_unpark(),
                    "parked commit not releasable after the lease dropped"
                );
                assert!(!st.leased(), "unparked commit with a live lease");
                commits += 1;
            }
            assert_eq!(commits, 1, "the commit must execute exactly once");
        });
    }

    /// Transport payload-lease invariant #2 — the §5.4 shutdown third
    /// interleaving: final drain vs a late lease drop. The
    /// no-write-while-leased rule stays unconditional at shutdown: the
    /// drain may send the payload-writing reply only when the probe proves
    /// the lease is gone; otherwise it degrades to a header-only reply that
    /// never touches the payload region. Once the probe observes refs == 0
    /// no new lease can appear (delivery requires a prior commit), so the
    /// payload write cannot race a resurrection.
    #[test]
    fn ent_lease_shutdown_header_only_never_writes_leased_payload() {
        loom::model(|| {
            let st = Arc::new(lease_core::EntLeaseState::new());
            assert_eq!(st.acquire(), 0);
            // The commit parked before shutdown (worker owns the message).
            assert!(matches!(st.try_commit(), lease_core::CommitGate::Parked));

            // Thread A: late lease drop racing the shutdown drain.
            let dropper = {
                let st = Arc::clone(&st);
                thread::spawn(move || {
                    // The wake goes to a worker that is already draining;
                    // it is at worst spurious, never required here.
                    let _ = st.release();
                })
            };

            // Shutdown drain (bounded wait modeled as a single probe).
            if st.try_unpark() {
                // Full payload-writing reply: legal ONLY with the lease gone.
                assert!(
                    !st.leased(),
                    "shutdown drain wrote a payload while a lease was live"
                );
            }
            // else: header-only reply — payload region untouched, so a live
            // lease is fine; nothing to assert.

            dropper.join().unwrap();
            assert!(!st.leased(), "lease outlived its drop");
        });
    }
}
