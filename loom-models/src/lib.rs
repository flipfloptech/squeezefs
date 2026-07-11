//! Exhaustive loom model-checking of SqueezeFS's lock-free protocol cores.
//!
//! The modules under test are `#[path]`-included from `../src` — the models
//! check the exact shipped code, not a copy. Each protocol was extracted
//! into a dependency-free core module for this purpose:
//!
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
//! - [`journal_core`]: the KV journal ring's lock-free admission +
//!   reservation core (CoW KV metadata design §4.4 pts 2/5, §4.6 pt 3,
//!   PR K3) — invariants: reservations never overlap and are contiguous
//!   (multi-page ones span consecutive pages), seqs are strictly monotonic
//!   in reservation order, lap/offset/wrap derivation is exact, admissions
//!   never over-commit the ring (`head + admitted` never exceeds
//!   `reusable_upto + capacity` less the user-invisible checkpoint
//!   reserve), admitted bytes are conserved across transfer/release, and
//!   the checkpoint carve-out is never consumable by user admissions.
//! - [`alloc_ext_core`]: the KV extent allocator's lock-free bitmap /
//!   pending-free / reserve core (CoW KV metadata design §4.7, PR K4) —
//!   invariants: an extent is never handed to two concurrent claimers, a
//!   pending-freed extent is never claimable before its checkpoint-durable
//!   seq (the root-fallback soundness rule, risk R3), and the compaction
//!   reserve is never consumable by user claims while internal claims
//!   drain it exactly.
//! - [`node_state_core`]: the KV node lifecycle word (CoW KV metadata
//!   design §4.6/§10, PR K5 —
//!   clean/dirty/serializing/superseded) — invariants: a commit's
//!   `mark_dirty` and an SMO's `supersede` can never jointly lose a
//!   record (either the apply is refused or the supersede outcome
//!   reports the dirt for the successor build); clock eviction wins only
//!   from the exact clean state and never against a node that just
//!   accepted dirt (§4.5 dirty pinning); freeze-swap conserves records
//!   across a racing apply (frozen + open == applied, dirty bit exact);
//!   at most one freeze is ever in flight.
//!
//! Models run only under `--cfg loom` (see `tests/run_loom.sh`); a plain
//! `cargo test` here compiles the cores against std atomics and runs
//! nothing.

#[path = "../../src/meta_backend/kv/alloc_ext_core.rs"]
pub mod alloc_ext_core;
#[path = "../../src/cow_core.rs"]
pub mod cow_core;
#[path = "../../src/gauge_core.rs"]
pub mod gauge_core;
#[path = "../../src/incarnation_core.rs"]
pub mod incarnation_core;
#[path = "../../src/meta_backend/kv/journal_core.rs"]
pub mod journal_core;
#[path = "../../third_party/fuse3/src/raw/connection/lease_core.rs"]
pub mod lease_core;
#[path = "../../src/meta_backend/kv/node_state_core.rs"]
pub mod node_state_core;
#[path = "../../src/refcount_core.rs"]
pub mod refcount_core;

#[cfg(all(test, loom))]
mod models {
    use crate::{
        alloc_ext_core, gauge_core, incarnation_core, journal_core, lease_core, node_state_core,
    };
    use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use loom::sync::Arc;
    use loom::thread;

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
                assert!(
                    crate::refcount_core::release(&cell),
                    "clone's release must free"
                );
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

    /// Tiny journal-core geometry shared by the ring models: a few pages of
    /// 4 entry bytes each, so wrap interleavings are reachable in a handful
    /// of operations.
    fn tiny_ring(pages: u64, reserve_bytes: u64) -> journal_core::CoreGeometry {
        journal_core::CoreGeometry {
            page_data_len: 4,
            pages,
            reserve_bytes,
        }
    }

    /// Journal-core invariant #1 (design §4.4 pt 2): two committers racing
    /// admit+reserve receive disjoint, back-to-back logical ranges with
    /// strictly monotonic seqs (seq == start position), and the derived
    /// lap/offset/page/segment geometry is exact — including a multi-page
    /// reservation's contiguous segments and, after a watermark advance, a
    /// reservation that wraps the ring into lap 1.
    ///
    /// Capacity 12 for two 3-byte admissions: a racing `try_admit` may
    /// observe a transfer's transient double-count (`admitted` still
    /// carrying bytes `head` already claimed — the module's deliberate
    /// conservative read, worst case 3 + 3 + 3 = 9 here), and the protocol
    /// promises refusal is only ever spurious, never an over-commit — so
    /// the model sizes the ring to make both admissions unconditional and
    /// keeps every geometry assertion deterministic. (Refusal semantics
    /// under pressure are model #2's subject.)
    #[test]
    fn journal_core_reservations_disjoint_monotonic_wrap_exact() {
        loom::model(|| {
            let geo = tiny_ring(3, 0); // capacity 12
            let core = Arc::new(journal_core::JournalCore::new(geo, 0, 0));

            let t = {
                let core = Arc::clone(&core);
                thread::spawn(move || {
                    let adm = core
                        .try_admit(3, journal_core::AdmissionClass::User)
                        .expect("3 of 12 bytes must admit even against a transient double-count");
                    core.reserve(adm)
                })
            };
            let adm = core
                .try_admit(3, journal_core::AdmissionClass::User)
                .expect("3 more of 12 bytes must admit even against a transient double-count");
            let r_main = core.reserve(adm);
            let r_thread = t.join().unwrap();

            // Disjoint + contiguous: the two ranges are exactly [0,3) and
            // [3,6), in either assignment.
            let (a, b) = if r_main.start < r_thread.start {
                (r_main, r_thread)
            } else {
                (r_thread, r_main)
            };
            assert_eq!(
                (a.start, a.len),
                (0, 3),
                "first reservation must start the ring"
            );
            assert_eq!(
                (b.start, b.len),
                (3, 3),
                "reservations must be back-to-back"
            );
            assert!(a.end() <= b.start, "reservations overlap");
            assert_eq!(a.seq(), a.start, "seq is the start position");
            assert!(b.seq() > a.seq(), "seqs must be strictly monotonic");
            assert_eq!(core.admitted(), 0, "both admissions fully transferred");
            assert_eq!(core.head(), 6, "head == total reserved bytes");

            // Geometry of the second range [3, 6): crosses the page-0/page-1
            // data boundary — multi-page, contiguous segments.
            assert_eq!(geo.lap(3), 0);
            assert_eq!(geo.ring_offset(3), 3);
            assert_eq!(geo.page_index(3), 0);
            assert_eq!(geo.in_page_off(3), 3);
            let segs = geo.segments(3, 3);
            assert_eq!(
                segs,
                vec![
                    journal_core::PageSegment {
                        page: 0,
                        data_off: 3,
                        len: 1
                    },
                    journal_core::PageSegment {
                        page: 1,
                        data_off: 0,
                        len: 2
                    },
                ],
                "multi-page reservation must map to contiguous page segments"
            );
            assert_eq!(
                geo.owned_page_starts(3, 3),
                vec![4],
                "[3,6) contains exactly page 1's first logical byte (pos 4)"
            );

            // Ring full at head 6 + 7 > watermark 0 + 12: admission refused
            // until the watermark advances (§4.6 pt 3), then the wrapping
            // reservation's lap/page derivation is exact.
            assert!(
                core.try_admit(7, journal_core::AdmissionClass::User)
                    .is_none(),
                "admission past reusable_upto must be refused"
            );
            core.advance_reusable_upto(6);
            let adm = core
                .try_admit(7, journal_core::AdmissionClass::User)
                .expect("watermark advance must open admission");
            let r = core.reserve(adm);
            assert_eq!((r.start, r.len), (6, 7));
            assert_eq!(geo.lap(6), 0);
            assert_eq!(geo.lap(r.end() - 1), 1, "range [6,13) crosses into lap 1");
            assert_eq!(
                geo.segments(6, 7),
                vec![
                    journal_core::PageSegment {
                        page: 1,
                        data_off: 2,
                        len: 2
                    },
                    journal_core::PageSegment {
                        page: 2,
                        data_off: 0,
                        len: 4
                    },
                    journal_core::PageSegment {
                        page: 0,
                        data_off: 0,
                        len: 1
                    },
                ],
                "wrap: the range re-enters page 0 at lap 1"
            );
            assert_eq!(
                geo.owned_page_starts(6, 7),
                vec![8, 12],
                "[6,13) contains page 2's first byte (8) and page 0's lap-1 first byte (12)"
            );
        });
    }

    /// Journal-core invariant #2 (design §4.4 pt 5): admissions never
    /// over-commit the ring, and the checkpoint carve-out is never
    /// consumable by user admissions — under a race, exactly one of two
    /// users fits the user budget, while the checkpoint task still admits
    /// from the reserve it alone may touch.
    #[test]
    fn journal_core_admission_never_overcommits_reserve_protected() {
        loom::model(|| {
            // capacity 8, reserve 3 ⇒ user budget 5.
            let geo = tiny_ring(2, 3);
            let core = Arc::new(journal_core::JournalCore::new(geo, 0, 0));

            let t = {
                let core = Arc::clone(&core);
                thread::spawn(move || core.try_admit(3, journal_core::AdmissionClass::User))
            };
            let mine = core.try_admit(3, journal_core::AdmissionClass::User);
            let theirs = t.join().unwrap();

            assert!(
                mine.is_some() ^ theirs.is_some(),
                "user budget 5 holds exactly one 3-byte admission of two"
            );
            assert!(
                core.admitted() + core.head() <= core.reusable_upto() + 8,
                "ring over-committed: claimed {} of 8",
                core.admitted() + core.head()
            );

            // The checkpoint task admits from the carve-out user admissions
            // must never touch (claimed 3 + 3 = 6 ≤ 8)…
            let ckpt = core
                .try_admit(3, journal_core::AdmissionClass::Checkpoint)
                .expect("checkpoint reserve must stay admissible to the checkpoint task");
            // …while a user retry is still refused (claimed 6 + 3 > 5 + 0).
            assert!(
                core.try_admit(3, journal_core::AdmissionClass::User)
                    .is_none(),
                "user admission consumed the checkpoint reserve"
            );
            assert!(
                core.admitted() + core.head() <= core.reusable_upto() + 8,
                "ring over-committed after checkpoint admission"
            );

            core.release(ckpt);
            if let Some(a) = mine {
                core.release(a);
            }
            if let Some(a) = theirs {
                core.release(a);
            }
            assert_eq!(core.admitted(), 0, "released budget must settle to zero");
        });
    }

    /// Journal-core invariant #3 (design §4.4 pts 2/5): admitted bytes are
    /// conserved across transfer (reserve) and release — a racing transfer
    /// and release settle with `admitted == 0`, the head advanced by
    /// exactly the transferred bytes, and the freed budget re-admittable
    /// to exact fit (one byte more refused).
    #[test]
    fn journal_core_budget_conserved_across_transfer_release() {
        loom::model(|| {
            let geo = tiny_ring(2, 0); // capacity 8
            let core = Arc::new(journal_core::JournalCore::new(geo, 0, 0));

            let t = {
                let core = Arc::clone(&core);
                thread::spawn(move || {
                    // Transfer path: admit → reserve.
                    let adm = core
                        .try_admit(2, journal_core::AdmissionClass::User)
                        .expect("2 of 8 must admit");
                    core.reserve(adm)
                })
            };
            // Release path: admit → give back (a tx that failed pre-reserve).
            let adm = core
                .try_admit(2, journal_core::AdmissionClass::User)
                .expect("2 more of 8 must admit");
            core.release(adm);
            let r = t.join().unwrap();

            assert_eq!(core.admitted(), 0, "transfer+release must conserve to zero");
            assert_eq!(
                core.head(),
                2,
                "head advanced by exactly the reserved bytes"
            );
            assert_eq!((r.start, r.len), (0, 2));

            // Exact fit: the remaining 6 bytes admit; one more byte does not.
            let exact = core
                .try_admit(6, journal_core::AdmissionClass::User)
                .expect("released budget must be re-admittable to exact fit");
            assert!(
                core.try_admit(1, journal_core::AdmissionClass::User)
                    .is_none(),
                "admission past exact capacity must be refused"
            );
            core.release(exact);
            assert_eq!(core.admitted(), 0);
        });
    }

    /// Extent-allocator invariant #1 (design §4.7, PR K4): two threads
    /// racing claims on a small heap never receive the same extent —
    /// including an extent whose pending-free was drained mid-race — and
    /// the free budget settles to exactly `total − live claims`.
    #[test]
    fn alloc_ext_no_double_alloc_under_concurrent_claimers() {
        loom::model(|| {
            // 3 extents, no reserve, FIFO cap 2. Seed one claim, park it
            // pending-free (tag 1), and make it durable — so the racing
            // claimers below contend on {fresh bits} ∪ {a just-drained
            // pending extent}, the exact reuse-race R3 worries about.
            let core = Arc::new(alloc_ext_core::ExtCore::new(3, 0, 2));
            let seeded = core
                .claim(alloc_ext_core::AllocClass::User)
                .expect("seed claim");
            core.free_pending(seeded, 1).expect("FIFO has room");
            let drained = core.advance_durable(1);
            assert_eq!(drained, vec![seeded], "durable tag 1 releases the seed");

            let t = {
                let core = Arc::clone(&core);
                thread::spawn(move || {
                    let a = core.claim(alloc_ext_core::AllocClass::User).ok();
                    let b = core.claim(alloc_ext_core::AllocClass::User).ok();
                    (a, b)
                })
            };
            let (c, d) = {
                let c = core.claim(alloc_ext_core::AllocClass::User).ok();
                let d = core.claim(alloc_ext_core::AllocClass::User).ok();
                (c, d)
            };
            let (a, b) = t.join().unwrap();

            let claimed: Vec<u64> = [a, b, c, d].into_iter().flatten().collect();
            let mut dedup = claimed.clone();
            dedup.sort_unstable();
            dedup.dedup();
            assert_eq!(
                claimed.len(),
                dedup.len(),
                "an extent was handed to two threads: {claimed:?}"
            );
            assert_eq!(claimed.len(), 3, "exactly the 3-extent heap is claimable");
            assert_eq!(
                core.free_extents(),
                0,
                "budget must settle to total − live claims"
            );
        });
    }

    /// Extent-allocator invariant #2 (design §4.7 CoW reuse rule — the R3
    /// root-fallback soundness gate): a pending-freed extent is NEVER
    /// claimable before its retiring checkpoint is durable. A claimer
    /// racing the durable-advance either fails (gate still closed) or
    /// succeeds — and success PROVES the watermark had covered the tag,
    /// because the drain is the only path that returns the bit.
    #[test]
    fn alloc_ext_pending_free_never_claimable_before_durable_seq() {
        loom::model(|| {
            // One extent, no reserve: the pending extent is the only
            // possible claim, so any successful claim is THE reuse.
            let core = Arc::new(alloc_ext_core::ExtCore::new(1, 0, 2));
            let e = core
                .claim(alloc_ext_core::AllocClass::User)
                .expect("the single extent claims");
            core.free_pending(e, 1).expect("FIFO has room");

            // Thread: the checkpoint task — ledger record seq 1 becomes
            // durable (post-barrier), opening the gate.
            let t = {
                let core = Arc::clone(&core);
                thread::spawn(move || {
                    core.advance_durable(1);
                })
            };

            // Main: a racing claimer.
            let raced = match core.claim(alloc_ext_core::AllocClass::User) {
                Ok(got) => {
                    assert_eq!(got, e, "the only claimable extent is the drained one");
                    assert!(
                        core.durable_seq() >= 1,
                        "extent reused before its retiring checkpoint (tag 1) was durable \
                         — the §4.7 gate is broken (R3)"
                    );
                    true
                }
                Err(alloc_ext_core::ClaimError::NoSpace) => {
                    // Gate still closed (or drain not yet run): correct.
                    false
                }
            };
            t.join().unwrap();

            // With the advance joined, the extent is claimable exactly once
            // across the whole model: whichever of the racing claim above
            // or this retry runs after the drain wins it — never both.
            let retry = core.claim(alloc_ext_core::AllocClass::User).is_ok();
            assert!(
                raced ^ retry,
                "the drained extent must be claimed exactly once (raced {raced}, retry {retry})"
            );
            assert_eq!(core.free_extents(), 0, "budget settles: one live claim");
        });
    }

    /// Extent-allocator invariant #3 (design §4.7 ENOSPC semantics):
    /// the compaction reserve is never consumable by user claims — under
    /// a race, exactly one of two users fits the user budget while the
    /// internal (compaction/SMO) claimant still drains the reserve it
    /// alone may touch; budget accounting settles exactly.
    #[test]
    fn alloc_ext_reserve_isolated_from_user_claims() {
        loom::model(|| {
            // 3 extents, reserve 2 ⇒ user budget 1.
            let core = Arc::new(alloc_ext_core::ExtCore::new(3, 2, 2));

            let t = {
                let core = Arc::clone(&core);
                thread::spawn(move || core.claim(alloc_ext_core::AllocClass::User).ok())
            };
            let mine = core.claim(alloc_ext_core::AllocClass::User).ok();
            let theirs = t.join().unwrap();

            assert!(
                mine.is_some() ^ theirs.is_some(),
                "user budget 1 holds exactly one of two racing user claims"
            );
            assert_eq!(
                core.free_extents(),
                2,
                "the reserve must survive user pressure intact"
            );

            // The internal claimant drains the reserve it alone may touch…
            let i1 = core
                .claim(alloc_ext_core::AllocClass::Internal)
                .expect("internal claim must reach the reserve");
            // …while a user retry is still refused at the floor.
            assert_eq!(
                core.claim(alloc_ext_core::AllocClass::User),
                Err(alloc_ext_core::ClaimError::NoSpace),
                "a user claim consumed the compaction reserve"
            );
            let i2 = core
                .claim(alloc_ext_core::AllocClass::Internal)
                .expect("the whole reserve is internal-claimable");
            assert_eq!(
                core.claim(alloc_ext_core::AllocClass::Internal),
                Err(alloc_ext_core::ClaimError::NoSpace),
                "an exhausted heap refuses internal claims too"
            );

            // Exact accounting: 3 claims live, none free.
            let mut live: Vec<u64> = [mine, theirs, Some(i1), Some(i2)]
                .into_iter()
                .flatten()
                .collect();
            live.sort_unstable();
            live.dedup();
            assert_eq!(live.len(), 3, "three unique live claims");
            assert_eq!(core.free_extents(), 0);
        });
    }

    /// Node-lifecycle invariant #1 (design §4.6 "supersede vs revalidate",
    /// PR K5): a commit's `mark_dirty` racing an SMO's `supersede` can
    /// never jointly lose a record — whichever RMW lands first, either the
    /// apply is refused (`Err(Superseded)`, the revalidation outcome) or
    /// the supersede outcome reports `was_dirty` so the successor build
    /// carries the delta. `Ok` + `was_dirty == false` would be a silently
    /// dropped record.
    #[test]
    fn node_state_supersede_never_loses_a_racing_apply() {
        loom::model(|| {
            let st = Arc::new(node_state_core::NodeState::new());

            let committer = {
                let st = Arc::clone(&st);
                thread::spawn(move || st.mark_dirty().is_ok())
            };
            let outcome = st.supersede().expect("first supersede wins");
            let applied = committer.join().unwrap();

            assert!(
                !(applied && !outcome.was_dirty),
                "a record was applied but the SMO saw a clean node — lost update"
            );
            assert!(
                st.is_superseded(),
                "terminal state must hold after the race"
            );
            // Post-terminal applies are always refused (the §4.6
            // lock-then-revalidate-then-retry contract).
            assert!(st.mark_dirty().is_err(), "apply accepted after supersede");
        });
    }

    /// Node-lifecycle invariant #2 (design §4.5 dirty pinning, PR K5):
    /// clock eviction (`try_evict`, a clean-only CAS) can never win
    /// against a node that just accepted dirt — `evicted && applied` is
    /// unrepresentable, and the loser of either race fails loud.
    #[test]
    fn node_state_evict_never_wins_against_accepted_dirt() {
        loom::model(|| {
            let st = Arc::new(node_state_core::NodeState::new());

            let evictor = {
                let st = Arc::clone(&st);
                thread::spawn(move || st.try_evict())
            };
            let applied = st.mark_dirty().is_ok();
            let evicted = evictor.join().unwrap();

            assert!(
                applied ^ evicted,
                "exactly one of apply/evict wins the clean word \
                 (applied {applied}, evicted {evicted})"
            );
            if applied {
                assert!(st.is_dirty(), "accepted dirt must be visible");
                assert!(!st.is_superseded(), "loser eviction must not sever");
            } else {
                assert!(st.is_superseded(), "winner eviction severs the object");
            }
        });
    }

    /// Node-lifecycle invariant #3 (design §4.6 pt 1 "freeze-swap vs
    /// concurrent apply", PR K5): the freeze atomically swaps the delta
    /// out while applies keep landing — records are conserved (frozen +
    /// open == applied), the dirty bit is exact (set iff the open delta is
    /// non-empty), and no interleaving strands a record in neither pile.
    /// The delta is modeled as a loom-checked counter cell swapped under
    /// the same lock the node cache uses (a loom Mutex standing in for the
    /// tokio per-node RwLock), so the model exercises the shipped word
    /// protocol composed exactly as `node_cache.rs` drives it.
    #[test]
    fn node_state_freeze_swap_conserves_records() {
        loom::model(|| {
            let st = Arc::new(node_state_core::NodeState::new());
            let open = Arc::new(loom::sync::Mutex::new(0u64)); // open-delta records

            // Seed one applied record so a freeze is always legal.
            {
                let mut g = open.lock().unwrap();
                st.mark_dirty().expect("seed apply");
                *g += 1;
            }

            // Committer: one more apply under the lock (§4.4 pt 1).
            let committer = {
                let st = Arc::clone(&st);
                let open = Arc::clone(&open);
                thread::spawn(move || {
                    let mut g = open.lock().unwrap();
                    if st.mark_dirty().is_ok() {
                        *g += 1;
                    }
                })
            };

            // Writeback: freeze under the lock (swap the delta out), write
            // outside it, end the freeze (§4.6 pt 1).
            let frozen: u64 = {
                let mut g = open.lock().unwrap();
                match st.begin_freeze() {
                    Ok(()) => {
                        let f = *g;
                        *g = 0;
                        f
                    }
                    Err(e) => panic!("freeze refused on a dirty node: {e:?}"),
                }
            };
            let redirtied = st.end_freeze();

            committer.join().unwrap();

            let open_now = *open.lock().unwrap();
            assert_eq!(
                frozen + open_now,
                2,
                "records conserved across the swap (frozen {frozen}, open {open_now})"
            );
            assert_eq!(
                st.is_dirty(),
                open_now > 0,
                "dirty bit must exactly track the open delta"
            );
            if redirtied {
                assert!(
                    open_now > 0,
                    "end_freeze reported re-accumulated dirt that does not exist"
                );
            }
            assert!(
                !st.is_freezing(),
                "freeze window must be closed after end_freeze"
            );
        });
    }

    /// Node-lifecycle invariant #4 (design §4.6: SMOs and writeback run
    /// serialized — a second in-flight freeze is a protocol bug the core
    /// must refuse): two racing `begin_freeze` calls on a dirty node admit
    /// exactly one winner.
    #[test]
    fn node_state_at_most_one_freeze_in_flight() {
        loom::model(|| {
            let st = Arc::new(node_state_core::NodeState::new());
            st.mark_dirty().expect("dirty");

            let racer = {
                let st = Arc::clone(&st);
                thread::spawn(move || st.begin_freeze().is_ok())
            };
            let mine = st.begin_freeze().is_ok();
            let theirs = racer.join().unwrap();

            assert!(
                mine ^ theirs,
                "exactly one freeze may win (mine {mine}, theirs {theirs})"
            );
            assert!(st.is_freezing(), "the winner's freeze is in flight");
            assert!(!st.is_dirty(), "the swap cleared the dirty bit");
        });
    }
}
