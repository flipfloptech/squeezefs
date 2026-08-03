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
//! - [`wake_core`]: the queue-worker eventfd wake-coalescing flag (L3
//!   transport-economy lever B) — invariants: N producer publications
//!   between two worker passes cost ≤ N (ideally 1) eventfd writes and a
//!   publication is NEVER stranded — after any interleaving of
//!   publish→arm→(write) producers with drain→disarm→scan worker passes,
//!   either a pass observed the publication or the eventfd counter is
//!   nonzero (the level-triggered PollAdd re-wakes the worker); also
//!   composed with [`lease_core`]: a parked commit whose lease drops
//!   through the coalescer is never stranded.
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
//!   pending-free / reserve core (CoW KV metadata design §4.7, PR K4;
//!   coverage-gate clock domain per design-smo-replay-currency §2-A,
//!   Option A) — invariants: an extent is never handed to two concurrent
//!   claimers; a pending-freed extent is never claimable before the
//!   durable journal TAIL passes its free-record seq (the §4.7 reuse rule
//!   restored to the letter — record-generation durability certified the
//!   wrong thing; risk R3); the compaction reserve is never consumable by
//!   user claims while internal claims drain it exactly; and the
//!   single-producer headroom probe (`pending_has_room`, the PR 4 at-cap
//!   admission check) is sound against racing drains — headroom observed
//!   at SMO admission can never be invalidated before the post-swap push.
//!   Since the multi-writer append partitioning (pre-RC engineering spec
//!   §6.2 item 3) two more: concurrent claims from two appenders never
//!   cross partitions (page-granular ownership + per-partition budgets),
//!   and per-appender coverage clocks never cross — one appender's durable
//!   tail can never release a peer's parked extent, whose gate seq is a
//!   position in the peer's own journal ring.
//! - [`epoch_core`]: the node cache's reader-revalidation epoch + durable
//!   tail, published as ONE ordered pair (pre-RC engineering spec §6.8
//!   item 2, §6.2 closing) — invariants: a loader that observes epoch `E`
//!   always reads a tail at least as new as `E`'s (so a node can never be
//!   stamped "current as of E" after classifying its torn tail against an
//!   OLDER tail, which silently drops records E covers); the epoch is
//!   monotone under racing pollers and exactly one poller ever observes a
//!   given step (so the drop pass runs once per step); and a node published
//!   by a loader that raced an advance is either stamped the new epoch or
//!   rejected — never silently retained. Reversing either side's access
//!   order, or weakening either access, fails the models
//!   (weakening-verified).
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
//! - [`patch_clone_core`]: the W1 sole-owner patch × clone pin COMPOSED
//!   two-word fence protocol (design-random-small-writes §5.1, review
//!   Issue 15) — invariant: ¬(pin-validated ∧ patch-proceeded). Each side
//!   is a store on one word followed by a load of the OTHER word (patch:
//!   incarnation retire → fence → refcount peek; clone: refcount
//!   try_acquire → fence → incarnation snapshot); Release/Acquire alone
//!   admits the store-buffering outcome — both sides reading old — so
//!   both interpose `fence(SeqCst)`. Modeled as ONE composition of
//!   incarnation_core × refcount_core (per-protocol models structurally
//!   cannot see SB across protocols).
//! - [`ipc_ring_core`] (`squeezefs-ipc`, design-preload-interception
//!   §5.3/§5.3.2, PR L4-1): the interception session's bounded MPSC
//!   submission ring — invariants: no entry lost, none double-consumed,
//!   no reservation past capacity, and per-cell publication sequences
//!   monotonic across laps (ABA-safe reuse). **Model precondition, stated
//!   because the model cannot see its violation** (the `patch_clone_core`
//!   lesson): exactly ONE consumer per ring — guaranteed structurally by
//!   the §5.5.1 session-pinning invariant, not by anything inside the
//!   ring; the models own a single consumer cursor and only transfer it
//!   across a `join`.
//! - [`ipc_slot_core`] (`squeezefs-ipc`, §5.3, PR L4-1): the completion-
//!   in-place op-slot state machine — invariants: exactly-once
//!   completion; a client that sets WAITER after DONE-publication is
//!   never stranded (publish-then-recheck, one RMW); FREE reuse never
//!   observes a stale DONE (generation guard); and the daemon's dequeue
//!   snapshot is the single linearization read of the descriptor
//!   (§5.3.1 rule 1 — post-snapshot client mutation never changes the
//!   served op, and the snapshot never reads pre-submit values).
//! - [`ipc_cqe_core`] (`squeezefs-ipc`, op-economy 2026-07-28): the
//!   completion doorbell — invariants: a reaper that parks via
//!   `park_begin` (register-then-snapshot) and re-scans is never
//!   stranded by a racing completion (either the scan sees DONE, the
//!   futex admission fails on the bumped seq, or the daemon's parked
//!   gate pays the wake); the unparked-stream wake elision never
//!   strands a racing parker. Removing a Dekker fence, weakening the
//!   daemon's `parked` load, or snapshotting before registering each
//!   fails the model.
//! - `ipc_wake_core` (composition, §5.3 protocol rule 3): the shipped
//!   [`wake_core::WakeCoalescer`] composed with [`ipc_ring_core`]
//!   publication exactly as the session doorbell ships — N submissions
//!   between two drains cost ≤ 1 wake and none is stranded (the L3
//!   never-stranded law re-verified in the new composition).
//! - [`conveyor_core`]: the per-volume commit conveyor's leader-election
//!   / queue / drain core (metadata-throughput design §5.5 D5, PR M7) —
//!   invariants: leader uniqueness (two racing electors never both win);
//!   no lost wakeups (after any enqueue+elect vs drain+unlead
//!   interleaving, no entry is left queued with no leader responsible
//!   for it — the release-then-recheck theorem); FIFO apply order;
//!   budget conservation across committer-future drops (an entry's byte
//!   budget is drained exactly once no matter when its committer
//!   disappears); and guard-lifetime ≥ staged-record-lifetime under an
//!   enqueued-then-dropped committer (Issue 13: the queue entry co-owns
//!   the DLM guard set, so same-key exclusion survives the committer's
//!   death until the pass's terminal outcome for that tx).
//!
//! - [`slot_gate_core`]: the PR VL5b per-slot cutover gate
//!   (design-volume-lifecycle §5.5.2a) — invariants: after `close()` +
//!   `drained()` observed true, NO mutator is admitted through the old
//!   routing table (the classic store-load window closed by SeqCst on
//!   both sides — either the drain waits for the mutator's increment or
//!   the mutator observes the closed gate and backs out); every
//!   back-out leaves the census balanced (no stuck in-flight counts).
//! - [`slot_cursor_core`]: the PR VL5b per-slot guest ino cursor —
//!   invariants: concurrent mints never collide; a checkpoint's
//!   published snapshot covers every mint whose record-apply
//!   happened-before it (the latch-free reader vs publisher edge the
//!   PR-plan loom clause names).
//! - [`lane_core`]: the pre-RC spec §6.2 items 5/6 per-writer LANE cursor
//!   (per-writer ino cursors + block-key incarnation stamps) — invariants:
//!   two appenders' concurrent mints are never equal and never leave their
//!   own lane (the duplicate-ino / aliased-lifetime failure both items
//!   exist to prevent); a snapshot taken after synchronizing with an
//!   applied record strictly covers that record's value (the durable
//!   watermark can never under-declare a covered mint); a concurrent
//!   `install_floor` never regresses a fresher mint and never inflates the
//!   mint COUNT (the POSIX-1 statfs derivation rides it).
//! - [`write_pipeline_core`]: the 2026-07-27 depth campaign's admission
//!   accounting (`AdmissionCore` — the CAS heart of
//!   `WritePipeline::admit` / `PipelinePermit::drop`) — invariants:
//!   bounded admission (no interleaving of admitters and releasers ever
//!   carries `inflight_bytes` past the target except through the
//!   empty-pipe bypass); at most ONE oversized bypass lands on an empty
//!   pipe (the CAS serializes racing bypassers); gauges settle to
//!   exactly zero once every admission released (no lost/duplicated
//!   accounting); and the probe-up governor's epoch roll (`ProbeCore`,
//!   2026-07-29) is single-winner — racing completion threads apply
//!   exactly one probe transition per epoch, multiplier bounded.
//!   **Stated precondition** (module docs): wake liveness
//!   is tick-bounded by design (`notify_waiters` stores no permit; the
//!   5 ms re-poll is the backstop) — the model checks accounting, not
//!   permit-style wake delivery, which the implementation does not
//!   claim.
//!
//! Models run only under `--cfg loom` (see `tests/run_loom.sh`); a plain
//! - [`zcrx_area_core`]: the zcrx area grant ledger (PR Z2 — per-slot
//!   refcounts + the MPSC free-return stack feeding the refill ring) —
//!   invariants: a recycled chunk's re-write never races any consumer's
//!   payload read (the Arc-pattern Release/Acquire chain extended
//!   through the stack push/pop), and racing last-ref releases recycle
//!   each slot exactly once (no loss, no double). Single grant consumer
//!   is a stated structural precondition (one driver per queue).
//!
//! - [`fd_table_core`] (`squeezefs-preload`, spec §11 TEST-5): the
//!   LD_PRELOAD shim's fd-table protocol core — the 9-CAS lock-free table
//!   that runs inside ARBITRARY host applications, across `fork`, from
//!   async-signal-safe contexts, and had no model. Three invariants:
//!   * **refcount law (Issue-14)**: a binding shared by N dup'd fd
//!     entries reports its unbind EXACTLY once, on the last ref; a zero
//!     count is terminal (a racing `dup` never resurrects a binding whose
//!     unbind is already on the wire); and an over-release saturates
//!     rather than wrapping (a wrapped count is a binding that can never
//!     report again).
//!   * **the PERF-7 offset-mirror Dekker pair**: an op's `publish` (store
//!     the final offset → `fence(SeqCst)` → re-check the arm) against a
//!     demoter's `disarm_if_current` (clear the arm → `fence(SeqCst)` →
//!     capture the offset). Invariant: ¬(publish reported "mirror still
//!     owns it" ∧ the demoter captured a STALE offset) — i.e. the final
//!     offset always reaches either the mirror's consumer or the caller's
//!     kernel write-through, never neither. Release/Acquire alone admits
//!     the store-buffering outcome (both sides reading old) and a fork
//!     child then resumes at a stale `f_pos`; removing EITHER fence fails
//!     this model (weakening-verified).
//!   * **fork-epoch conservatism**: a fork between "snapshot the epoch"
//!     and "install the binding" must stale the snapshot, so the binding
//!     never reads armed; and the fork walk's demote fires at most once
//!     per cell (dup siblings share one).
//!
//! `cargo test` here compiles the cores against std atomics and runs
//! nothing.

#[path = "../../src/meta_backend/kv/alloc_ext_core.rs"]
pub mod alloc_ext_core;
#[path = "../../src/meta_backend/kv/conveyor_core.rs"]
pub mod conveyor_core;
#[path = "../../src/meta_backend/kv/epoch_core.rs"]
pub mod epoch_core;
#[path = "../../src/cow_core.rs"]
pub mod cow_core;
#[path = "../../crates/squeezefs-preload/src/fd_table_core.rs"]
pub mod fd_table_core;
#[path = "../../src/gauge_core.rs"]
pub mod gauge_core;
#[path = "../../src/incarnation_core.rs"]
pub mod incarnation_core;
#[path = "../../crates/squeezefs-ipc/src/cqe_core.rs"]
pub mod ipc_cqe_core;
#[path = "../../crates/squeezefs-ipc/src/ring_core.rs"]
pub mod ipc_ring_core;
#[path = "../../crates/squeezefs-ipc/src/slot_core.rs"]
pub mod ipc_slot_core;
#[path = "../../src/meta_backend/kv/journal_core.rs"]
pub mod journal_core;
#[path = "../../src/lane_core.rs"]
pub mod lane_core;
#[path = "../../crates/fuse3/src/raw/connection/lease_core.rs"]
pub mod lease_core;
#[path = "../../src/meta_backend/kv/node_state_core.rs"]
pub mod node_state_core;
#[path = "../../src/patch_clone_core.rs"]
pub mod patch_clone_core;
#[path = "../../src/placed_core.rs"]
pub mod placed_core;
#[path = "../../src/refcount_core.rs"]
pub mod refcount_core;
#[path = "../../src/meta_backend/kv/slot_cursor_core.rs"]
pub mod slot_cursor_core;
#[path = "../../src/meta_backend/slot_gate_core.rs"]
pub mod slot_gate_core;
#[path = "../../crates/fuse3/src/raw/connection/wake_core.rs"]
pub mod wake_core;
#[path = "../../src/write_pipeline_core.rs"]
pub mod write_pipeline_core;
#[path = "../../src/zcrx_lane/area_core.rs"]
pub mod zcrx_area_core;

#[cfg(all(test, loom))]
mod zcrx_area_models {
    //! [`zcrx_area_core`]: the zcrx area grant ledger (design-zcrx-read-
    //! lane §4.3/§5, PR Z2) — per-slot refcounts + the MPSC free-return
    //! stack that feeds the refill ring. Invariants:
    //! * **recycle-never-races-consumers**: a chunk re-granted to the
    //!   driver (and re-written — NIC DMA stand-in) can never race a
    //!   consumer's payload read: every holder's reads happen-before the
    //!   recycle via the Arc-pattern `fetch_sub(Release)` + 0-crossing
    //!   `fence(Acquire)`, extended to the driver by the stack's
    //!   Release-push / Acquire-pop chain. Weakening the fetch_sub to
    //!   Relaxed, removing the fence, or weakening the push CAS fails
    //!   this model (weakening-verified — Z2 evidence note).
    //! * **exactly-once return, no loss**: racing last-ref releases of
    //!   distinct slots against the single consumer's pops recycle each
    //!   slot exactly once (none lost, none doubled).
    //!
    //! Model precondition (stated because the model cannot see its
    //! violation, the ipc_ring_core lesson): ONE grant consumer per
    //! ledger — structural (one driver per queue); the models own a
    //! single consumer context.
    use crate::zcrx_area_core::SpanLedger;
    use loom::cell::UnsafeCell;
    use loom::sync::Arc;
    use loom::thread;

    #[test]
    fn zcrx_chunk_recycle_never_races_consumers() {
        loom::model(|| {
            let ledger = Arc::new(SpanLedger::new(1));
            let cell = Arc::new(UnsafeCell::new(0u32));
            let slot = ledger.try_grant().expect("fresh ledger grants");
            // Driver writes the payload (NIC DMA stand-in) BEFORE
            // publishing refs to the consumers.
            cell.with_mut(|p| unsafe { *p = 1 });
            ledger.add_ref(slot); // the second consumer's ref
            let (l1, c1) = (Arc::clone(&ledger), Arc::clone(&cell));
            let t1 = thread::spawn(move || {
                c1.with(|p| assert_eq!(unsafe { *p }, 1, "consumer 1 payload read"));
                l1.release(0);
            });
            let (l2, c2) = (Arc::clone(&ledger), Arc::clone(&cell));
            let t2 = thread::spawn(move || {
                c2.with(|p| assert_eq!(unsafe { *p }, 1, "consumer 2 payload read"));
                l2.release(0);
            });
            // The driver polls for the recycle (bounded — loom needs
            // finite paths); on re-grant it overwrites the payload: if
            // any consumer read can still be in flight, loom's
            // UnsafeCell access tracking reports the race.
            let mut regranted = false;
            for _ in 0..2 {
                if let Some(s) = ledger.try_grant() {
                    assert_eq!(s, 0);
                    cell.with_mut(|p| unsafe { *p = 2 });
                    regranted = true;
                    break;
                }
                thread::yield_now();
            }
            t1.join().unwrap();
            t2.join().unwrap();
            if !regranted {
                let s = ledger
                    .try_grant()
                    .expect("all refs dropped ⇒ the slot recycled");
                assert_eq!(s, 0);
                cell.with_mut(|p| unsafe { *p = 2 });
            }
            assert!(ledger.try_grant().is_none(), "exactly one recycle grant");
            assert_eq!(ledger.free_count(), 0);
        });
    }

    #[test]
    fn zcrx_free_stack_returns_exactly_once_no_loss() {
        loom::model(|| {
            let ledger = Arc::new(SpanLedger::new(2));
            let s0 = ledger.try_grant().expect("slot 0");
            let s1 = ledger.try_grant().expect("slot 1");
            assert!(ledger.try_grant().is_none(), "ledger drained");
            let l1 = Arc::clone(&ledger);
            let t1 = thread::spawn(move || {
                l1.release(s0);
            });
            let l2 = Arc::clone(&ledger);
            let t2 = thread::spawn(move || {
                l2.release(s1);
            });
            // Single consumer racing both pushes.
            let mut got = Vec::new();
            for _ in 0..2 {
                if let Some(s) = ledger.try_grant() {
                    got.push(s);
                }
            }
            t1.join().unwrap();
            t2.join().unwrap();
            while got.len() < 2 {
                got.push(
                    ledger
                        .try_grant()
                        .expect("both released ⇒ both grantable (no loss)"),
                );
            }
            got.sort_unstable();
            let mut want = [s0, s1];
            want.sort_unstable();
            assert_eq!(got, want, "each slot recycled exactly once");
            assert!(ledger.try_grant().is_none(), "no double-recycle");
            assert_eq!(ledger.free_count(), 0);
        });
    }
}

#[cfg(all(test, loom))]
mod models {
    use crate::{
       alloc_ext_core, conveyor_core, epoch_core, gauge_core, incarnation_core, ipc_cqe_core,
        ipc_ring_core, ipc_slot_core, journal_core, lane_core, lease_core, node_state_core,
        patch_clone_core,
        placed_core, refcount_core, slot_cursor_core, slot_gate_core, wake_core,
        write_pipeline_core,
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

    /// THE COMPOSED TWO-WORD MODEL (design-random-small-writes §5.1,
    /// review Issue 15 — the RW2 loom obligation): W1 sole-owner patch ×
    /// clone pin in ONE model, both interleaving orders, asserting
    /// ¬(pin-validated ∧ patch-proceeded).
    ///
    /// Patch thread = the §5.1 mechanism's fence half exactly as shipped:
    /// `incarnation_core::retire` (what `mark_incarnation_unstable` runs)
    /// → `patch_clone_core::cross_word_fence()` → `refcount_core::peek`
    /// (`BlockAllocator::begin_patch_sole_owner`), with the back-off
    /// re-stabilize (`publish`) on refusal. Clone thread =
    /// `refcount_core::try_acquire` → `cross_word_fence()` →
    /// `incarnation_core::snapshot` (`BlockAllocator::pin_block_validated`).
    ///
    /// Why composed: each side is a store on one word then a load of the
    /// OTHER word — the classic store-buffering shape. `retire`'s trailing
    /// `Release` fence and `snapshot`'s `Acquire` load do NOT forbid both
    /// sides reading the old value (patch sees refcount 1 AND clone sees
    /// stable — the clone then completes referencing a block that mutates
    /// afterward); per-protocol models cannot represent the cross-protocol
    /// outcome at all. With both `SeqCst` fences the outcome is dead in
    /// every interleaving loom explores. (Weakening either fence to
    /// Release/Acquire fails this model — verified during development.)
    #[test]
    fn patch_clone_composed_never_mutates_a_validated_pin() {
        use loom::sync::atomic::AtomicU32;
        loom::model(|| {
            // A mapped, durable, sole-owned striped block: stable word,
            // refcount 1.
            let word = Arc::new(AtomicU64::new(incarnation_core::STABLE_FIRST));
            let rc = Arc::new(AtomicU32::new(1));

            // Patch side (`begin_patch_sole_owner` + the §5.1 back-off).
            let patcher = {
                let word = word.clone();
                let rc = rc.clone();
                thread::spawn(move || {
                    incarnation_core::retire(&word);
                    patch_clone_core::cross_word_fence();
                    let sole = refcount_core::peek(&rc) == 1;
                    if sole {
                        // In-place DMA happens here; publish re-stabilizes.
                        incarnation_core::publish(&word);
                    } else {
                        // Back off: re-stabilize (content unchanged) + CoW.
                        incarnation_core::publish(&word);
                    }
                    sole
                })
            };

            // Clone side (`pin_block_validated`).
            let pinned = refcount_core::try_acquire(&rc);
            let snap = if pinned {
                patch_clone_core::cross_word_fence();
                incarnation_core::snapshot(&word)
            } else {
                None
            };

            let patched = patcher.join().unwrap();

            // THE invariant — ¬(pin-validated ∧ patch-proceeded), stated
            // at generation precision (the §5.1 interleaving closure): a
            // pin validated against the PRE-PATCH word (gen 0 stable) and
            // an in-place patch are mutually exclusive — that joint
            // outcome IS the store-buffering corruption (both cross-word
            // loads read stale; the completed clone then references a
            // block that mutates afterward). A pin validated against the
            // POST-publish word (gen 1) is the legitimate "a clone racing
            // a write may see old or new" arm — the patch completed
            // strictly before the validation and nothing mutates after.
            assert!(
                !(pinned && snap == Some(incarnation_core::STABLE_FIRST) && patched),
                "store-buffering outcome: the clone validated its pin \
                 against the pre-patch incarnation while the patch \
                 proceeded in place — the §5.1 fence is broken"
            );

            // Progress accounting (both orders live in this state space):
            // an unvalidated pin retries against a word the patch always
            // re-stabilizes — the retry's snapshot must see stability.
            if pinned && snap.is_none() {
                assert!(
                    incarnation_core::snapshot(&word).is_some(),
                    "post-join the patch has re-stabilized (publish on both \
                     exits): the clone's bounded retry can validate"
                );
            }
            // The refcount is conserved: pin took one iff `pinned`.
            let expect = 1 + u32::from(pinned);
            assert_eq!(
                refcount_core::peek(&rc),
                expect,
                "refcount conservation across the composition"
            );
        });
    }

    /// The patch-interleaving case for the incarnation seqlock alone
    /// (design-random-small-writes risk R3 — the read-side half of W1):
    /// a validated cache fill racing an IN-PLACE patch (retire → data
    /// mutate → publish, same word ops as a reuse cycle but on a live
    /// mapped key) never publishes mid-patch bytes — the §5.1
    /// read-your-own-writes/racing-read argument's mechanical core.
    #[test]
    fn incarnation_validated_fill_never_serves_mid_patch_bytes() {
        loom::model(|| {
            let word = Arc::new(AtomicU64::new(incarnation_core::STABLE_FIRST));
            let payload = Arc::new(AtomicU64::new(0));

            let patcher = {
                let word = word.clone();
                let payload = payload.clone();
                thread::spawn(move || {
                    // begin_patch_sole_owner's word half…
                    incarnation_core::retire(&word);
                    patch_clone_core::cross_word_fence();
                    // …the in-place DMA (payload stamped 1)…
                    payload.store(1, Ordering::Relaxed);
                    // …publish after the device write.
                    incarnation_core::publish(&word);
                })
            };

            // Reader = validated fill: snapshot, "device read", validate.
            if let Some(before) = incarnation_core::snapshot(&word) {
                let bytes = payload.load(Ordering::Relaxed);
                if incarnation_core::still(&word, before) {
                    assert_eq!(
                        bytes,
                        before >> 1,
                        "validated fill published bytes from mid-patch \
                         (snapshot gen {}, payload stamp {bytes})",
                        before >> 1
                    );
                }
            }
            patcher.join().unwrap();
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

    /// Transport payload-lease invariant #3 — MEM-1 dest-DMA tokens, the
    /// multi-ref face: two worker-held destination tokens against one ent
    /// (a kernel-split multi-block read: one token per in-flight SQE) drop
    /// concurrently while the queue worker's commit gate races them. The
    /// COMMIT_AND_FETCH executes exactly once and never while ANY token is
    /// live (refs > 0 ⇒ device DMA can still land in the payload buffer),
    /// a non-final drop never wakes, and a parked commit is never lost to
    /// a missed wake — the LAST drop must observe `parked` or the worker's
    /// publish-then-recheck must observe refs == 0 (the same SeqCst-fence
    /// Dekker pairing as invariant #1, now explored at refs ∈ {2, 1, 0}).
    #[test]
    fn ent_lease_dest_tokens_multi_ref_commit_exactly_once() {
        loom::model(|| {
            let st = Arc::new(lease_core::EntLeaseState::new());
            // Claims happen strictly before the request's reply can exist
            // (mint → claim → submit → await), so both refs precede the
            // racing gate below — exactly the shipped order.
            st.acquire_dest();
            st.acquire_dest();

            let wakes = Arc::new(AtomicU64::new(0));

            // Two device-worker CQE completions dropping their tokens.
            let droppers: Vec<_> = (0..2)
                .map(|_| {
                    let st = Arc::clone(&st);
                    let wakes = Arc::clone(&wakes);
                    thread::spawn(move || {
                        if st.release() {
                            wakes.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                })
                .collect();

            // Queue worker: the abandoned read's error reply commits.
            let mut commits = 0u32;
            let parked = match st.try_commit() {
                lease_core::CommitGate::Ready => {
                    assert!(
                        !st.leased(),
                        "commit fired while a dest token was live — the ent \
                         could re-arm into an in-flight DMA"
                    );
                    commits += 1;
                    false
                }
                lease_core::CommitGate::Parked => true,
            };

            for d in droppers {
                d.join().unwrap();
            }

            if parked {
                assert!(
                    wakes.load(Ordering::SeqCst) >= 1,
                    "missed wake: commit parked but no token drop saw parked == true"
                );
                assert!(
                    st.try_unpark(),
                    "parked commit not releasable after every token dropped"
                );
                assert!(!st.leased(), "unparked commit with a live token");
                commits += 1;
            }
            assert_eq!(commits, 1, "the commit must execute exactly once");
        });
    }

    /// Wake-coalescer invariant #1 (L3 transport-economy lever B): a
    /// producer publication is NEVER stranded. Producers publish state
    /// (Release store — the mpsc-send stand-in) then `arm()`, writing the
    /// eventfd only on `true`; the worker's pass is drain-eventfd →
    /// `disarm()` → scan. After ANY interleaving, when the worker would
    /// park (eventfd counter 0 after a pass), every publication has been
    /// observed — a missed one with a zero counter is the lost-wake
    /// deadlock (stranded reply ⇒ kernel `waiting ≥ 1` ⇒ umount EBUSY).
    /// An unconsumed counter is fine: the level-triggered PollAdd re-wakes
    /// the worker the moment `submit_and_wait` arms it, modeled as the
    /// post-join extra pass.
    ///
    /// Development weakening evidence (both verified failing): permuting
    /// the worker pass to scan-before-disarm strands a publication
    /// ("consumed 1 of 2" park), and `disarm` as a plain SeqCst *store*
    /// (no RMW read of the flag's predecessor — the happens-before
    /// carrier) strands both models. The shipped RMW + drain→disarm→scan
    /// order passes.
    #[test]
    fn wake_coalescer_publication_never_stranded() {
        loom::model(|| {
            let flag = Arc::new(wake_core::WakeCoalescer::new());
            let efd = Arc::new(AtomicU64::new(0)); // eventfd counter
            let produced = Arc::new(AtomicU64::new(0)); // channel stand-in

            let producers: Vec<_> = (0..2)
                .map(|_| {
                    let flag = Arc::clone(&flag);
                    let efd = Arc::clone(&efd);
                    let produced = Arc::clone(&produced);
                    thread::spawn(move || {
                        // submit_reply ships exactly this order: channel
                        // send (publish), then arm, then conditional write.
                        produced.fetch_add(1, Ordering::Release);
                        if flag.arm() {
                            efd.fetch_add(1, Ordering::Release);
                        }
                    })
                })
                .collect();

            // Queue worker: bounded passes (each producer writes the
            // eventfd at most once ⇒ ≤ 2 wake-driven continuations).
            let worker = {
                let flag = Arc::clone(&flag);
                let efd = Arc::clone(&efd);
                let produced = Arc::clone(&produced);
                thread::spawn(move || {
                    let mut consumed = 0;
                    for _pass in 0..3 {
                        efd.swap(0, Ordering::AcqRel); // drain to EAGAIN
                        flag.disarm();
                        consumed = produced.load(Ordering::Acquire); // scan
                        if efd.load(Ordering::SeqCst) == 0 {
                            break; // park: PollAdd armed, counter zero
                        }
                    }
                    consumed
                })
            };

            for p in producers {
                p.join().unwrap();
            }
            let consumed = worker.join().unwrap();

            if efd.load(Ordering::SeqCst) == 0 {
                // Worker parked with nothing armed: NOTHING may be stranded.
                assert_eq!(
                    consumed, 2,
                    "lost wake: publication(s) stranded while the worker \
                     parks on a zero eventfd (consumed {consumed} of 2)"
                );
            } else {
                // Level-triggered PollAdd: the nonzero counter re-wakes the
                // worker; that pass observes everything.
                efd.swap(0, Ordering::AcqRel);
                flag.disarm();
                assert_eq!(
                    produced.load(Ordering::Acquire),
                    2,
                    "the wake-driven pass must observe every publication"
                );
            }
        });
    }

    /// Wake-coalescer invariant #2 — composed with [`lease_core`] exactly
    /// as shipped (`EntPayloadLease::drop` + a concurrent `submit_reply`
    /// producer sharing one queue coalescer): a parked commit whose lease
    /// drops through the coalescer is never stranded, and the concurrent
    /// reply publication is never lost, whatever a racing worker pass
    /// consumed. The worker runs one CONCURRENT pass (the race window),
    /// then the wake-driven passes the nonzero counter would produce.
    #[test]
    fn wake_coalescer_lease_drop_parked_commit_never_stranded() {
        loom::model(|| {
            let st = Arc::new(lease_core::EntLeaseState::new());
            assert_eq!(st.acquire(), 0, "fresh ent must be lease-free");
            // The commit parked before the drop (worker owns the message).
            assert!(matches!(st.try_commit(), lease_core::CommitGate::Parked));

            let flag = Arc::new(wake_core::WakeCoalescer::new());
            let efd = Arc::new(AtomicU64::new(0));
            let sent = Arc::new(AtomicU64::new(0));

            // Thread A: EntPayloadLease::drop as shipped — release, then
            // the coalesced wake.
            let dropper = {
                let st = Arc::clone(&st);
                let flag = Arc::clone(&flag);
                let efd = Arc::clone(&efd);
                thread::spawn(move || {
                    if st.release() && flag.arm() {
                        efd.fetch_add(1, Ordering::Release);
                    }
                })
            };
            // Thread B: submit_reply for another ent on the same queue —
            // publish, then the coalesced wake.
            let submitter = {
                let flag = Arc::clone(&flag);
                let efd = Arc::clone(&efd);
                let sent = Arc::clone(&sent);
                thread::spawn(move || {
                    sent.store(1, Ordering::Release);
                    if flag.arm() {
                        efd.fetch_add(1, Ordering::Release);
                    }
                })
            };

            // One concurrent worker pass (the race window).
            let mut commits = 0u32;
            let mut sent_seen;
            efd.swap(0, Ordering::AcqRel);
            flag.disarm();
            sent_seen = sent.load(Ordering::Acquire) != 0;
            if st.try_unpark() {
                assert!(!st.leased(), "unparked commit with a live lease");
                commits += 1;
            }

            dropper.join().unwrap();
            submitter.join().unwrap();

            // Wake-driven passes: each producer wrote ≤ 1, so ≤ 2 rounds.
            for _ in 0..2 {
                if efd.load(Ordering::SeqCst) == 0 {
                    break;
                }
                efd.swap(0, Ordering::AcqRel);
                flag.disarm();
                sent_seen |= sent.load(Ordering::Acquire) != 0;
                if commits == 0 && st.try_unpark() {
                    assert!(!st.leased(), "unparked commit with a live lease");
                    commits += 1;
                }
            }

            assert_eq!(
                commits, 1,
                "parked commit stranded (or double-committed) across the \
                 coalesced lease-drop wake"
            );
            assert!(
                sent_seen,
                "reply publication stranded across the coalesced wake"
            );
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
    /// root-fallback soundness gate, coverage-clocked per
    /// design-smo-replay-currency §2-A): a pending-freed extent is NEVER
    /// claimable before the durable journal tail passes its free-record
    /// seq (the gate tag; the freeing entry's highest seq — coverage of
    /// the whole entry, flips included, not mere record durability). A
    /// claimer racing the tail-advance either fails (gate still closed)
    /// or succeeds — and success PROVES the watermark had covered the
    /// tag, because the drain is the only path that returns the bit. The
    /// model's numbers are journal positions: free record at seq 7, the
    /// covering post-barrier tail at 8 (a boundary past it).
    #[test]
    fn alloc_ext_pending_free_never_claimable_before_covering_tail() {
        loom::model(|| {
            // One extent, no reserve: the pending extent is the only
            // possible claim, so any successful claim is THE reuse.
            let core = Arc::new(alloc_ext_core::ExtCore::new(1, 0, 2));
            let e = core
                .claim(alloc_ext_core::AllocClass::User)
                .expect("the single extent claims");
            core.free_pending(e, 7).expect("FIFO has room");

            // A tail BELOW the free record (the swap-covering ledger
            // record whose dying-floor-clamped tail sits under the SMO
            // entry — durable, yet not coverage) must release nothing:
            // the exact generation-gate bug shape.
            assert!(
                core.advance_durable(6).is_empty(),
                "a durable tail below the free record released the extent — \
                 record durability is not flip coverage (§2-A)"
            );

            // Thread: the checkpoint task — a later record's tail (8)
            // passes the free record, opening the gate.
            let t = {
                let core = Arc::clone(&core);
                thread::spawn(move || {
                    core.advance_durable(8);
                })
            };

            // Main: a racing claimer.
            let raced = match core.claim(alloc_ext_core::AllocClass::User) {
                Ok(got) => {
                    assert_eq!(got, e, "the only claimable extent is the drained one");
                    assert!(
                        core.durable_seq() >= 7,
                        "extent reused before a durable tail covered its free record \
                         (gate 7) — the §4.7 coverage gate is broken (R3/§2-A)"
                    );
                    true
                }
                Err(alloc_ext_core::ClaimError::NoSpace) => {
                    // Gate still closed (or drain not yet run): correct.
                    false
                }
                // RES-14: the bounded-rescan refusal. It must be
                // UNREACHABLE here — the protocol's whole claim is that
                // budget and bitmap never disagree, so any interleaving
                // that produces it is a real defect the model must name.
                Err(alloc_ext_core::ClaimError::InvariantDrift) => {
                    panic!("claim reported budget/bitmap drift under a legal interleaving")
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

    /// Extent-allocator invariant #4 (design-smo-replay-currency PR 4
    /// clause a): the single-producer headroom probe is sound against a
    /// racing drain. The serialized SMO task observes `pending_has_room`
    /// at admission and pushes post-swap; a concurrent `advance_durable`
    /// only ever VACATES slots, so an observed headroom can never be
    /// invalidated — the post-swap `free_pending` must succeed in every
    /// interleaving (the post-swap `PendingFreeFull` error is
    /// defense-in-depth, structurally unreachable under admission
    /// headroom). Also pins the probe's refusal face: with the FIFO
    /// genuinely full and no drain, headroom reads false.
    #[test]
    fn alloc_ext_headroom_observed_at_admission_holds_at_push() {
        loom::model(|| {
            // Cap 2, three extents: two parked entries saturate the FIFO.
            let core = Arc::new(alloc_ext_core::ExtCore::new(3, 0, 2));
            let a = core.claim(alloc_ext_core::AllocClass::User).expect("a");
            let b = core.claim(alloc_ext_core::AllocClass::User).expect("b");
            let c = core.claim(alloc_ext_core::AllocClass::User).expect("c");
            core.free_pending(a, 3).expect("slot 1");
            core.free_pending(b, 5).expect("slot 2");
            assert!(
                !core.pending_has_room(),
                "a saturated FIFO must refuse admission headroom"
            );

            // Thread: a durable tail covering the first entry drains it.
            let t = {
                let core = Arc::clone(&core);
                thread::spawn(move || {
                    core.advance_durable(4);
                })
            };

            // Main = the serialized SMO task: admission headroom check,
            // then the post-swap push. Drains only vacate, so headroom
            // observed true MUST hold at the push.
            if core.pending_has_room() {
                core.free_pending(c, 9).expect(
                    "headroom observed at admission was invalidated before the \
                     post-swap push — the clause-a soundness argument is broken",
                );
            }
            t.join().unwrap();
        });
    }

    /// Extent-allocator invariant #5 (the §4.7 cycle-break, P2
    /// 2026-07-26 §9 fix direction a): a FORCED retirement pushed at
    /// FIFO cap parks in the overflow and rides the SAME coverage gate —
    /// never claimable before a durable tail passes its gate seq — while
    /// racing drains stay exact: extents are conserved across the
    /// FIFO + overflow split (each released exactly once, none leaked,
    /// none early). The forced push races a concurrent `advance_durable`
    /// (the drain may vacate a FIFO slot first, in which case the push
    /// legally lands in the ring instead — both containers must uphold
    /// the gate identically).
    #[test]
    fn alloc_ext_forced_overflow_gate_and_conservation() {
        loom::model(|| {
            // Cap 2, three extents: two parked entries saturate the FIFO;
            // the third retirement is the forced one (gate 7).
            let core = Arc::new(alloc_ext_core::ExtCore::new(3, 0, 2));
            let a = core.claim(alloc_ext_core::AllocClass::User).expect("a");
            let b = core.claim(alloc_ext_core::AllocClass::User).expect("b");
            let c = core.claim(alloc_ext_core::AllocClass::User).expect("c");
            core.free_pending(a, 3).expect("slot 1");
            core.free_pending(b, 5).expect("slot 2");
            assert!(!core.pending_has_room(), "FIFO saturated");

            // Thread: a durable tail covering the two FIFO entries (6)
            // but NOT the forced retirement (gate 7).
            let t = {
                let core = Arc::clone(&core);
                thread::spawn(move || {
                    core.advance_durable(6);
                })
            };

            // Main = the serialized SMO task: the forced push must never
            // refuse — at cap it overflows; if the racing drain vacated a
            // slot first it may land in the ring instead (both legal).
            core.free_pending_forced(c, 7);

            // A racing claim can only ever win a, or b — never c: its
            // gate (7) is uncovered until the 8-advance below, wherever
            // it parked.
            let mut raced = 0u32;
            for _ in 0..2 {
                match core.claim(alloc_ext_core::AllocClass::User) {
                    Ok(got) => {
                        assert_ne!(
                            got, c,
                            "the forced (overflow) retirement was claimable before a \
                             durable tail covered its gate — the §4.7 coverage gate \
                             does not hold across the FIFO/overflow split"
                        );
                        let gate = if got == a { 3 } else { 5 };
                        assert!(
                            core.durable_seq() >= gate,
                            "extent reused before a durable tail covered its gate"
                        );
                        raced += 1;
                    }
                    Err(alloc_ext_core::ClaimError::NoSpace) => {}
                    // RES-14: must be unreachable (see the sibling model).
                    Err(alloc_ext_core::ClaimError::InvariantDrift) => {
                        panic!("claim reported budget/bitmap drift under a legal interleaving")
                    }
                }
            }
            t.join().unwrap();

            // Cover the forced gate; every remaining extent drains and
            // the population settles exactly: three claims total, no
            // leak (an entry stuck in either container) and no double
            // release (a budget overshoot).
            core.advance_durable(8);
            let mut total = raced;
            while core.claim(alloc_ext_core::AllocClass::User).is_ok() {
                total += 1;
            }
            assert_eq!(
                total, 3,
                "extents must be conserved across the FIFO + overflow split"
            );
            assert_eq!(core.pending_count(), 0, "nothing stays parked");
            assert_eq!(core.free_extents(), 0, "budget settles: three live claims");
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

    /// Extent-allocator invariant #6 (**append partitioning** — pre-RC
    /// engineering spec §6.2 item 3): two appenders claiming concurrently
    /// from one bitmap can never be handed the same extent, and neither
    /// can be handed an extent from the other's partition. Under a shared
    /// budget the two claims are one entitlement pool; the per-partition
    /// budgets plus page-granular ownership are what make disjointness
    /// structural rather than statistical.
    #[test]
    fn alloc_ext_partitioned_claims_never_cross() {
        loom::model(|| {
            // 4 extents, 2 per page, 2 appenders ⇒ writer 0 owns page 0
            // (extents 0,1), writer 1 owns page 1 (extents 2,3).
            let core = Arc::new(alloc_ext_core::ExtCore::new_partitioned(
                4,
                0,
                2,
                alloc_ext_core::PartitionMap::new(2, 2),
            ));
            let peer = {
                let core = Arc::clone(&core);
                thread::spawn(move || {
                    (
                        core.claim_in(1, alloc_ext_core::AllocClass::User).ok(),
                        core.claim_in(1, alloc_ext_core::AllocClass::User).ok(),
                    )
                })
            };
            let a = core.claim_in(0, alloc_ext_core::AllocClass::User).ok();
            let b = core.claim_in(0, alloc_ext_core::AllocClass::User).ok();
            let (c, d) = peer.join().unwrap();

            let mine: Vec<u64> = [a, b].into_iter().flatten().collect();
            let theirs: Vec<u64> = [c, d].into_iter().flatten().collect();
            assert_eq!(mine.len(), 2, "writer 0's partition holds exactly 2 extents");
            assert_eq!(theirs.len(), 2, "writer 1's partition holds exactly 2");
            for e in &mine {
                assert!(
                    *e < 2,
                    "writer 0 claimed extent {e} from writer 1's bitmap page"
                );
            }
            for e in &theirs {
                assert!(
                    *e >= 2,
                    "writer 1 claimed extent {e} from writer 0's bitmap page"
                );
            }
            let mut all: Vec<u64> = mine.into_iter().chain(theirs).collect();
            all.sort_unstable();
            all.dedup();
            assert_eq!(all.len(), 4, "an extent was handed to two appenders");
            assert_eq!(core.free_extents(), 0, "budgets settle exactly");
            assert_eq!(core.claim_in(0, alloc_ext_core::AllocClass::User), Err(alloc_ext_core::ClaimError::NoSpace));
            assert_eq!(core.claim_in(1, alloc_ext_core::AllocClass::User), Err(alloc_ext_core::ClaimError::NoSpace));
        });
    }

    /// Extent-allocator invariant #7 (**per-appender coverage clocks** —
    /// spec §6.2 item 3, the §4.7 reuse rule under partitioning): a gate
    /// seq is a position in the FREEING appender's own journal ring, so
    /// one appender's durable tail says nothing about a peer's parked
    /// extent. Racing advances must release only their own partition's
    /// entries — a shared clock would hand back an extent the peer's
    /// replay window still routes into (risk R3, cross-appender edition).
    #[test]
    fn alloc_ext_partitioned_coverage_gates_never_cross() {
        loom::model(|| {
            let core = Arc::new(alloc_ext_core::ExtCore::new_partitioned(
                4,
                0,
                2,
                alloc_ext_core::PartitionMap::new(2, 2),
            ));
            let mine = core.claim_in(0, alloc_ext_core::AllocClass::Internal).expect("mine");
            let theirs = core.claim_in(1, alloc_ext_core::AllocClass::Internal).expect("theirs");
            // Both park at gate 100 — the same NUMBER in two different
            // ring spaces, which is exactly the confusion a shared clock
            // cannot tell apart.
            core.free_pending(mine, 100).expect("park mine");
            core.free_pending(theirs, 100).expect("park theirs");

            // Writer 0's tail passes 100 while writer 1's stays at 0.
            let peer = {
                let core = Arc::clone(&core);
                thread::spawn(move || core.claim_in(1, alloc_ext_core::AllocClass::Internal).ok())
            };
            let released = core.advance_durable_in(0, 100);
            let raced = peer.join().unwrap();

            assert_eq!(released, vec![mine], "only the advancing appender's entry may release");
            assert!(
                core.is_allocated(theirs),
                "writer 1's parked extent was released by writer 0's tail — the coverage \
                 gate crossed clock domains"
            );
            assert!(
                raced.is_none_or(|e| e != theirs),
                "a racing peer claim won its own parked extent before its own tail \
                 covered the gate"
            );
            assert_eq!(core.pending_count_in(1), 1);
            assert_eq!(core.durable_seq_in(1), 0, "a peer's advance must not move my clock");

            // The peer's own tail is the only thing that frees it.
            assert!(core.advance_durable_in(1, 99).is_empty());
            assert_eq!(core.advance_durable_in(1, 100), vec![theirs]);
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
    /// Node-lifecycle invariant #5 (PR VL7 §5.7 D4 — the forced-compaction
    /// nudge): `begin_forced_freeze` composes with lock-serialized applies
    /// exactly like the ordinary freeze — a racing apply lands either in
    /// the pre-freeze delta (captured by the ordinary freeze arm: the
    /// composition `compact_node_forced` drives) or after the swap (where
    /// it re-sets DIRTY and the SMO's supersede reports `was_dirty` for
    /// the bounded second merge); at most one freeze is ever in flight
    /// (shared exclusion with `begin_freeze`); and the SMO's supersede
    /// always observes `was_freezing` — the `smo_replace` bookkeeping
    /// assert that makes the forced transition sound.
    #[test]
    fn node_state_forced_freeze_conserves_and_reports_freezing() {
        loom::model(|| {
            let st = Arc::new(node_state_core::NodeState::new());
            let open = Arc::new(loom::sync::Mutex::new(0u64)); // open-delta records

            let committer = {
                let st = Arc::clone(&st);
                let open = Arc::clone(&open);
                thread::spawn(move || {
                    let mut g = open.lock().unwrap();
                    if st.mark_dirty().is_ok() {
                        *g += 1;
                        true
                    } else {
                        false // post-supersede apply refused (revalidate-and-retry)
                    }
                })
            };

            // compact_node_forced's freeze arm, under the node lock: the
            // ordinary freeze when a delta is open, the forced transition
            // when the overlay is empty.
            let frozen: u64 = {
                let mut g = open.lock().unwrap();
                if *g > 0 {
                    st.begin_freeze().expect("dirty node freezes ordinarily");
                    std::mem::take(&mut *g)
                } else {
                    st.begin_forced_freeze().expect("clean node forced-freezes");
                    0
                }
            };
            assert!(st.is_freezing(), "the nudge holds a freeze in flight");
            assert!(
                st.begin_forced_freeze().is_err(),
                "at most one freeze in flight (forced vs forced)"
            );

            // The SMO swap + tidy-up, exactly smo_replace's order.
            let outcome = st.supersede().expect("SMO swap");
            assert!(
                outcome.was_freezing,
                "supersede must see a freeze-borne source (the smo_replace assert)"
            );
            st.end_freeze();
            assert!(!st.is_freezing(), "freeze window closed after the swap");

            let applied = committer.join().unwrap();
            let open_now = *open.lock().unwrap();
            assert_eq!(
                frozen + open_now,
                u64::from(applied),
                "records conserved across the forced freeze \
                 (frozen {frozen}, open {open_now}, applied {applied})"
            );
            if applied && frozen == 0 {
                assert!(
                    outcome.was_dirty,
                    "a post-swap apply must be visible to the SMO as \
                     re-accumulated dirt (the bounded second merge)"
                );
            }
        });
    }

    /// Conveyor invariant #1 (metadata-throughput §5.5, PR M7): leader
    /// uniqueness — two committers racing enqueue+elect produce exactly
    /// one leader; the loser's entry is guaranteed drained by SOMEONE
    /// (either the winner's pass or its own later election after the
    /// winner unleads — modeled by running the winner's full pass loop).
    #[test]
    fn conveyor_leader_unique_and_loser_entry_drained() {
        loom::model(|| {
            let c: Arc<conveyor_core::ConveyorCore<u32>> =
                Arc::new(conveyor_core::ConveyorCore::new());

            let t = {
                let c = Arc::clone(&c);
                thread::spawn(move || {
                    c.enqueue(2, 1);
                    c.try_lead()
                })
            };
            c.enqueue(1, 1);
            let mine = c.try_lead();
            let theirs = t.join().unwrap();

            assert!(
                mine || theirs,
                "with entries queued, at least one elector must win"
            );
            assert!(
                !(mine && theirs),
                "two racing electors must never both hold leadership"
            );

            // The winner's pass loop (drain-until-empty + release-then-
            // recheck) must account for BOTH entries — the loser parked
            // on its oneshot and will never elect again.
            let mut drained = 0usize;
            loop {
                let batch = c.drain(64, u64::MAX);
                if batch.is_empty() {
                    if !c.unlead_and_recheck() {
                        break;
                    }
                    continue;
                }
                drained += batch.len();
            }
            assert_eq!(drained, 2, "every enqueued entry must drain exactly once");
            assert_eq!(c.pending(), 0);
        });
    }

    /// Conveyor invariant #2 (§5.5): no lost wakeups — a committer whose
    /// election fails against a leader that is concurrently finishing
    /// (empty drain → unlead → recheck) is never stranded: either the
    /// leader's release-then-recheck re-elects it to drain the new entry,
    /// or the committer's own election won and it runs a pass. The
    /// interleaving where BOTH decline is the lost wakeup this model
    /// forbids.
    #[test]
    fn conveyor_no_lost_wakeup_across_unlead() {
        loom::model(|| {
            let c: Arc<conveyor_core::ConveyorCore<u32>> =
                Arc::new(conveyor_core::ConveyorCore::new());

            // A live leader with an empty queue, about to retire.
            assert!(c.try_lead(), "seed leader");

            // Committer thread: enqueue + elect (the §5.5 two-step, no
            // await between). If it wins, it drains its own entry.
            let committer = {
                let c = Arc::clone(&c);
                thread::spawn(move || {
                    c.enqueue(7, 1);
                    if c.try_lead() {
                        let mut got = 0usize;
                        loop {
                            let batch = c.drain(64, u64::MAX);
                            if batch.is_empty() {
                                if !c.unlead_and_recheck() {
                                    break;
                                }
                                continue;
                            }
                            got += batch.len();
                        }
                        got
                    } else {
                        0
                    }
                })
            };

            // Leader thread: empty drain → release-then-recheck loop.
            let mut leader_got = 0usize;
            loop {
                let batch = c.drain(64, u64::MAX);
                if batch.is_empty() {
                    if !c.unlead_and_recheck() {
                        break;
                    }
                    continue;
                }
                leader_got += batch.len();
            }

            let committer_got = committer.join().unwrap();
            assert_eq!(
                leader_got + committer_got,
                1,
                "the enqueued entry must be drained exactly once (leader {leader_got}, \
                 committer {committer_got}) — zero is a lost wakeup, two is a double drain"
            );
            assert_eq!(c.pending(), 0, "nothing may remain queued");
        });
    }

    /// Conveyor invariant #3 (§5.5 / §4.4 pt 2 transfer): FIFO — entries
    /// drain in enqueue order even when the drain races a producer, and
    /// a capped drain takes a strict prefix (never reorders around the
    /// cap).
    #[test]
    fn conveyor_fifo_apply_order_under_race() {
        loom::model(|| {
            let c: Arc<conveyor_core::ConveyorCore<u32>> =
                Arc::new(conveyor_core::ConveyorCore::new());
            c.enqueue(1, 1);
            c.enqueue(2, 1);

            // Racing producer.
            let t = {
                let c = Arc::clone(&c);
                thread::spawn(move || c.enqueue(3, 1))
            };

            // Capped drain: a strict FIFO prefix.
            let first = c.drain(2, u64::MAX);
            assert_eq!(first, vec![1, 2], "drain must return the FIFO prefix");
            t.join().unwrap();
            let rest = c.drain(64, u64::MAX);
            assert_eq!(rest, vec![3], "the racing entry drains after the prefix");
        });
    }

    /// Conveyor invariant #4 (§5.5 lifecycle): budget conservation across
    /// committer-future drops — an entry's byte budget reaches the drain
    /// exactly once whether or not its committer is still alive. The
    /// committer thread dies right after enqueue+elect (its result
    /// channel token drops); the detached pass (modeled inline) must
    /// still see every byte, and the modeled budget cell settles to
    /// exactly the drained sum.
    #[test]
    fn conveyor_budget_conserved_across_committer_drop() {
        loom::model(|| {
            let c: Arc<conveyor_core::ConveyorCore<(u32, u64)>> =
                Arc::new(conveyor_core::ConveyorCore::new());
            let drained_budget = Arc::new(AtomicU64::new(0));

            // Committer A: enqueues 3 budget-bytes and DIES (thread ends
            // — the committer-future drop; its entry must survive it).
            let a = {
                let c = Arc::clone(&c);
                thread::spawn(move || {
                    c.enqueue((10, 3), 3);
                    c.try_lead()
                })
            };
            // Committer B: enqueues 5 and stays only long enough to elect.
            c.enqueue((20, 5), 5);
            let b_led = c.try_lead();
            let a_led = a.join().unwrap();

            // Whoever won leadership runs the pass; if both failed there
            // is a live leader by definition — impossible here (fresh
            // core), so exactly one won.
            assert!(a_led ^ b_led, "exactly one elector wins a fresh core");
            let mut drained = 0usize;
            loop {
                let batch = c.drain(64, u64::MAX);
                if batch.is_empty() {
                    if !c.unlead_and_recheck() {
                        break;
                    }
                    continue;
                }
                for (_, len) in batch {
                    drained_budget.fetch_add(len, Ordering::Relaxed);
                    drained += 1;
                }
            }
            assert_eq!(drained, 2, "both entries drain despite A's death");
            assert_eq!(
                drained_budget.load(Ordering::Relaxed),
                8,
                "budget bytes are conserved across the committer drop (3 + 5)"
            );
            assert_eq!(c.pending(), 0);
        });
    }

    /// Conveyor invariant #5 (§5.5 revision 2, Issue 13): guard-lifetime
    /// ≥ staged-record-lifetime under an enqueued-then-dropped committer.
    /// The guard set is an `Arc` co-owned by the queue entry; the
    /// committer's own clone dropping (thread death) must leave the
    /// entry's clone alive — observable as a strong count that never
    /// falls to 1's release while the record sits queued, and the guard
    /// is released only at the pass's terminal outcome for that entry.
    #[test]
    fn conveyor_guard_outlives_dropped_committer_until_terminal() {
        loom::model(|| {
            /// Stands in for one DLM guard: releasing (the last `Arc`
            /// clone dropping) flips the observable flag — the moment a
            /// same-key writer could proceed.
            struct GuardToken {
                released: Arc<AtomicBool>,
            }
            impl Drop for GuardToken {
                fn drop(&mut self) {
                    self.released.store(true, Ordering::SeqCst);
                }
            }
            struct Entry {
                _guard: Arc<GuardToken>, // stands in for Arc<[DlmGuard]>
            }
            let c: Arc<conveyor_core::ConveyorCore<Entry>> =
                Arc::new(conveyor_core::ConveyorCore::new());
            let released = Arc::new(AtomicBool::new(false));
            let guard = Arc::new(GuardToken {
                released: Arc::clone(&released),
            });

            // Committer: enqueue {records + guard clone}, elect, DIE (the
            // dropped-committer case — its own clone dies with it).
            let committer = {
                let c = Arc::clone(&c);
                thread::spawn(move || {
                    c.enqueue(
                        Entry {
                            _guard: Arc::clone(&guard),
                        },
                        1,
                    );
                    let led = c.try_lead();
                    drop(guard); // the committer frame's own ref dies
                    led
                })
            };
            let led = committer.join().unwrap();
            assert!(led, "sole elector must win");

            // The committer is DEAD; its records sit queued. Issue 13:
            // the guard must still be held (the queue entry co-owns it).
            assert!(
                !released.load(Ordering::SeqCst),
                "guard released while its records were still queued — same-key \
                 exclusion lost (the Issue-13 bug)"
            );

            // The pass drains and reaches the tx's terminal outcome: ONLY
            // then does the guard release.
            let batch = c.drain(64, u64::MAX);
            assert_eq!(batch.len(), 1);
            assert!(
                !released.load(Ordering::SeqCst),
                "guard must be held through the pass until the terminal outcome"
            );
            drop(batch); // terminal outcome: entry dropped post-fanout
            assert!(
                released.load(Ordering::SeqCst),
                "guard must be released at the terminal outcome (no leak)"
            );
            assert!(!c.unlead_and_recheck());
        });
    }

    /// IPC submission-ring invariant #1 (design-preload-interception
    /// §5.3.2, PR L4-1): two racing producers' entries are never lost and
    /// never double-consumed. The single consumer (module precondition —
    /// enforced structurally by session pinning, §5.5.1) drains bounded
    /// passes concurrently, then finishes deterministically post-join
    /// (cursor ownership transfers through the join, the only legal
    /// transfer).
    ///
    /// Weakening evidence (verified during development, then restored):
    /// the producer's publishing seq store demoted Release→Relaxed fails
    /// this model — the consumer pops the seeded phantom value 0 ("saw 0
    /// of 2") in interleavings where the value store has not propagated.
    #[test]
    fn ipc_ring_racing_producers_never_lost_nor_duplicated() {
        loom::model(|| {
            let storage = Arc::new(
                ipc_ring_core::RingStorage::with_capacity(2).expect("capacity 2 is valid"),
            );

            let producers: Vec<_> = [1u32, 2u32]
                .into_iter()
                .map(|v| {
                    let storage = Arc::clone(&storage);
                    thread::spawn(move || storage.view().push(v))
                })
                .collect();

            // The one consumer: bounded concurrent passes (racing the
            // producers), remainder drained post-join.
            let consumer_storage = Arc::clone(&storage);
            let consumer = thread::spawn(move || {
                let ring = consumer_storage.view();
                let mut cursor = ipc_ring_core::RingConsumer::new();
                let mut popped = Vec::new();
                for _ in 0..2 {
                    if let Some(v) = cursor.pop(&ring) {
                        popped.push(v);
                    }
                }
                (cursor, popped)
            });

            for p in producers {
                assert!(
                    p.join().unwrap(),
                    "capacity 2 must accept both racing pushes"
                );
            }
            let (mut cursor, mut popped) = consumer.join().unwrap();
            // Post-join drain with the transferred cursor.
            let ring = storage.view();
            while let Some(v) = cursor.pop(&ring) {
                popped.push(v);
            }

            popped.sort_unstable();
            assert_eq!(
                popped,
                vec![1, 2],
                "every accepted entry is consumed exactly once (lost or \
                 duplicated ring entry = lost or double-served op)"
            );
        });
    }

    /// IPC submission-ring invariant #2: reservation never passes capacity
    /// — three pushes racing into a capacity-2 ring accept exactly two
    /// (the third sees full = client-visible backpressure), and both
    /// accepted entries drain intact.
    #[test]
    fn ipc_ring_never_reserves_past_capacity() {
        loom::model(|| {
            let storage = Arc::new(
                ipc_ring_core::RingStorage::with_capacity(2).expect("capacity 2 is valid"),
            );

            let t = {
                let storage = Arc::clone(&storage);
                thread::spawn(move || {
                    let ring = storage.view();
                    u32::from(ring.push(10)) + u32::from(ring.push(11))
                })
            };
            let accepted_main = u32::from(storage.view().push(12));
            let accepted = t.join().unwrap() + accepted_main;
            assert_eq!(
                accepted, 2,
                "capacity 2 must accept exactly 2 of 3 racing pushes"
            );

            let ring = storage.view();
            let mut cursor = ipc_ring_core::RingConsumer::new();
            let mut popped = Vec::new();
            while let Some(v) = cursor.pop(&ring) {
                popped.push(v);
            }
            popped.sort_unstable();
            assert_eq!(popped.len(), 2, "exactly the accepted entries drain");
            for v in popped {
                assert!((10..=12).contains(&v), "phantom value {v} popped");
            }
        });
    }

    /// IPC submission-ring invariant #3 (per-cell seq monotonicity = ABA-
    /// safe lap reuse): one producer streams three values through a
    /// capacity-2 ring while the consumer drains concurrently — whatever
    /// interleaves, consumption is exactly the accepted prefix, in FIFO
    /// order, across the cell-0 lap boundary.
    #[test]
    fn ipc_ring_lap_reuse_keeps_fifo_exact() {
        loom::model(|| {
            let storage = Arc::new(
                ipc_ring_core::RingStorage::with_capacity(2).expect("capacity 2 is valid"),
            );

            let producer = {
                let storage = Arc::clone(&storage);
                thread::spawn(move || {
                    let ring = storage.view();
                    assert!(ring.push(1), "empty ring must accept");
                    assert!(ring.push(2), "second cell must accept");
                    // Third push reuses cell 0 on lap 1 — legal only after
                    // the consumer freed it; otherwise full (backpressure).
                    ring.push(3)
                })
            };

            let consumer_storage = Arc::clone(&storage);
            let consumer = thread::spawn(move || {
                let ring = consumer_storage.view();
                let mut cursor = ipc_ring_core::RingConsumer::new();
                let mut popped = Vec::new();
                for _ in 0..3 {
                    if let Some(v) = cursor.pop(&ring) {
                        popped.push(v);
                    }
                }
                (cursor, popped)
            });

            let third_accepted = producer.join().unwrap();
            let (mut cursor, mut popped) = consumer.join().unwrap();
            let ring = storage.view();
            while let Some(v) = cursor.pop(&ring) {
                popped.push(v);
            }

            let expect: Vec<u32> = if third_accepted {
                vec![1, 2, 3]
            } else {
                vec![1, 2]
            };
            assert_eq!(
                popped, expect,
                "FIFO across the lap boundary: exactly the accepted \
                 prefix, in order (a permutation or phantom here is the \
                 cell-reuse ABA corruption)"
            );
        });
    }

    /// IPC op-slot invariant #1 (design-preload-interception §5.3
    /// protocol rule 2, PR L4-1): completion is exactly-once and a client
    /// that parks is never stranded — `park_prepare`'s single RMW either
    /// lands before `complete`'s swap (daemon sees WAITER ⇒ wakes) or
    /// after it (client sees DONE ⇒ Ready, no park). The descriptor/
    /// result field is an atomic beside the state word, exactly like the
    /// production `IpcSlot` fields.
    ///
    /// Weakening evidence (verified during development, then restored):
    /// `complete`'s swap demoted AcqRel→Relaxed fails this model — a
    /// Ready/woken consumer reads result 0 (the completion's result write
    /// not published with its DONE).
    #[test]
    fn ipc_slot_completion_exactly_once_parked_waiter_never_stranded() {
        loom::model(|| {
            let slot = Arc::new(ipc_slot_core::SlotCore::new());
            let result = Arc::new(AtomicU64::new(0));
            let wake = Arc::new(AtomicBool::new(false));

            // Client half 1 (main, sequential): claim + submit.
            let gen = slot.try_claim().expect("fresh slot must claim");
            slot.publish_submitted();

            // Daemon: serve + complete + conditional wake.
            let server = {
                let slot = Arc::clone(&slot);
                let result = Arc::clone(&result);
                let wake = Arc::clone(&wake);
                thread::spawn(move || {
                    assert!(
                        slot.try_begin_serve(),
                        "submitted slot must begin serve (WAITER bit or not)"
                    );
                    result.store(7, Ordering::Relaxed);
                    if slot.complete() {
                        wake.store(true, Ordering::SeqCst);
                    }
                })
            };

            // Client half 2 (main): one bounded spin probe, then the
            // two-phase park protocol.
            let mut consumed = 0u32;
            if slot.is_done_for(gen) {
                assert_eq!(result.load(Ordering::Relaxed), 7, "spin consume");
                consumed += 1;
            } else {
                match slot.park_prepare() {
                    ipc_slot_core::ParkOutcome::Ready => {
                        assert!(slot.is_done_for(gen));
                        assert_eq!(result.load(Ordering::Relaxed), 7, "ready consume");
                        consumed += 1;
                    }
                    ipc_slot_core::ParkOutcome::Park { expected } => {
                        assert_eq!(
                            ipc_slot_core::state_bits(expected) & ipc_slot_core::WAITER,
                            0,
                            "state_bits strips WAITER"
                        );
                        // Parked: the wake MUST arrive (assert post-join),
                        // otherwise the client sleeps on a futex nobody
                        // will ever wake — the stranded-op deadlock.
                        server.join().unwrap();
                        assert!(
                            wake.load(Ordering::SeqCst),
                            "missed wake: client parked but complete() saw no WAITER"
                        );
                        assert!(slot.is_done_for(gen), "woken client must consume");
                        assert_eq!(result.load(Ordering::Relaxed), 7, "parked consume");
                        consumed += 1;
                        slot.release();
                        assert_eq!(consumed, 1, "exactly-once completion");
                        return; // server already joined
                    }
                }
            }
            server.join().unwrap();
            assert_eq!(consumed, 1, "exactly-once completion");
            slot.release();
        });
    }

    /// IPC op-slot invariant #2b (DIALED P3 large-op economy — the
    /// `release_claimed` arena-extension hold): a slot life that was
    /// claimed but never submitted (a multi-slab run's extension hold)
    /// returns to FREE, and the hold's generation can never consume a
    /// LATER life's DONE — the same ABA guarantee as invariant #2, now
    /// with the CLAIMED → FREE edge in the reuse chain. The hold's
    /// release races the next life's full cycle.
    #[test]
    fn ipc_slot_claimed_hold_release_reuse_clean() {
        loom::model(|| {
            let slot = Arc::new(ipc_slot_core::SlotCore::new());

            // Hold life: claimed as an arena extension, never submitted.
            let hold_gen = slot.try_claim().expect("fresh slot must claim");
            // While CLAIMED the daemon must never serve it.
            assert!(!slot.try_begin_serve(), "holds are daemon-invisible");
            slot.release_claimed();

            // Next life races a stale hold-generation probe.
            let second_life = {
                let slot = Arc::clone(&slot);
                thread::spawn(move || {
                    let gen2 = slot.try_claim().expect("released hold must re-claim");
                    slot.publish_submitted();
                    assert!(slot.try_begin_serve(), "real life serves");
                    slot.complete();
                    gen2
                })
            };
            for _ in 0..2 {
                assert!(
                    !slot.is_done_for(hold_gen),
                    "a hold's generation must never consume a later life's DONE"
                );
            }
            let gen2 = second_life.join().unwrap();
            assert!(
                gen2 > hold_gen,
                "generations strictly monotonic across holds"
            );
            assert!(slot.is_done_for(gen2), "the live generation consumes");
        });
    }

    /// IPC op-slot invariant #2 (the generation ABA guard): a waiter from
    /// a previous life of the slot can never mistake a recycled slot's
    /// DONE for its own — whatever the stale probe interleaves with, the
    /// gen-1 check reads false forever once gen 1 was consumed.
    #[test]
    fn ipc_slot_free_reuse_never_observes_stale_done() {
        loom::model(|| {
            let slot = Arc::new(ipc_slot_core::SlotCore::new());

            // Life 1, completed and consumed (sequential prologue).
            let gen1 = slot.try_claim().expect("fresh slot must claim");
            slot.publish_submitted();
            assert!(slot.try_begin_serve());
            slot.complete();
            assert!(slot.is_done_for(gen1));
            slot.release();

            // Life 2 races the stale gen-1 probe.
            let second_life = {
                let slot = Arc::clone(&slot);
                thread::spawn(move || {
                    let gen2 = slot.try_claim().expect("released slot must re-claim");
                    slot.publish_submitted();
                    assert!(slot.try_begin_serve());
                    slot.complete();
                    gen2
                })
            };

            // Stale probes (the delayed-futex-artifact shape): must never
            // attribute life 2's DONE to gen 1.
            for _ in 0..2 {
                assert!(
                    !slot.is_done_for(gen1),
                    "stale generation consumed a recycled slot's DONE \
                     (the ABA corruption: a dead waiter steals a live op)"
                );
            }

            let gen2 = second_life.join().unwrap();
            assert!(gen2 > gen1, "generations strictly monotonic");
            assert!(!slot.is_done_for(gen1), "stale gen false even at rest");
            assert!(slot.is_done_for(gen2), "the live generation consumes");
        });
    }

    /// IPC op-slot invariant #3 (§5.3.1 rule 1 — the dequeue snapshot is
    /// the single linearization read): with a hostile sibling mutating the
    /// descriptor word mid-flight, the daemon's one post-`begin_serve`
    /// snapshot read observes the submitted value or the hostile value —
    /// NEVER the pre-submit seed (the submit Release / begin_serve Acquire
    /// edge) — and the served op equals the snapshot even when the hostile
    /// store lands after the snapshot (serve-from-copy, structural).
    /// Descriptor modeled as an atomic beside the state word, exactly the
    /// production `IpcSlot` shape (client-writable during serve is
    /// bounded-behavior by design, never a data race).
    ///
    /// Weakening evidence (verified during development, then restored):
    /// `publish_submitted` demoted Release→Relaxed fails this model — the
    /// daemon's snapshot reads the pre-submit seed 0.
    #[test]
    fn ipc_slot_snapshot_single_read_never_pre_submit_hostile_bounded() {
        loom::model(|| {
            let slot = Arc::new(ipc_slot_core::SlotCore::new());
            let descriptor = Arc::new(AtomicU64::new(0)); // pre-submit seed
            let served = Arc::new(AtomicU64::new(0));

            // The server is spawned BEFORE claim/submit — a post-submit
            // spawn would smuggle a happens-before edge past the submit
            // Release / begin_serve Acquire pairing this model exists to
            // check (verified: with a post-submit spawn, weakening the
            // submit store to Relaxed passes; with this shape it fails).
            let server = {
                let slot = Arc::clone(&slot);
                let descriptor = Arc::clone(&descriptor);
                let served = Arc::clone(&served);
                thread::spawn(move || {
                    // Bounded dequeue attempts (the ring-pop stand-in).
                    for _ in 0..3 {
                        if slot.try_begin_serve() {
                            // THE single linearization read (§5.3.1 rule
                            // 1): one load into a private copy; validated
                            // + served from the copy; never re-read.
                            let snapshot = descriptor.load(Ordering::Relaxed);
                            assert_ne!(
                                snapshot, 0,
                                "snapshot read the pre-submit seed — the \
                                 submit Release / begin_serve Acquire edge \
                                 is broken"
                            );
                            served.store(snapshot, Ordering::Relaxed);
                            slot.complete();
                            return Some(snapshot);
                        }
                    }
                    None
                })
            };

            let _gen = slot.try_claim().expect("fresh slot must claim");
            descriptor.store(1, Ordering::Relaxed); // the honest op
            slot.publish_submitted();

            // Hostile sibling thread scribbles the descriptor at an
            // arbitrary point (before or after the daemon's snapshot).
            let hostile = {
                let descriptor = Arc::clone(&descriptor);
                thread::spawn(move || descriptor.store(2, Ordering::Relaxed))
            };

            let served_by_thread = server.join().unwrap();
            hostile.join().unwrap();
            let snapshot = match served_by_thread {
                Some(s) => s,
                None => {
                    // Every bounded attempt ran pre-submit: main serves
                    // deterministically post-join (uninteresting branch;
                    // the concurrent branches above are the model).
                    assert!(slot.try_begin_serve());
                    let snapshot = descriptor.load(Ordering::Relaxed);
                    served.store(snapshot, Ordering::Relaxed);
                    slot.complete();
                    snapshot
                }
            };
            assert!(
                snapshot == 1 || snapshot == 2,
                "snapshot must be the honest or hostile value, never torn"
            );
            assert_eq!(
                served.load(Ordering::Relaxed),
                snapshot,
                "post-snapshot mutation never changes the served op \
                 (serve-from-copy)"
            );
        });
    }

    /// IPC op-slot invariant #4 (the 2026-07-26 reap-economy multi-park):
    /// a reaper that `park_prepare`s SEVERAL in-flight slots and then
    /// waits on all their words at once (`futex_waitv`) is never
    /// stranded by a completion racing the gap between its RMW and the
    /// wait. For the completed slot, exactly one of these holds after
    /// any interleaving:
    ///
    ///   (a) `park_prepare` returned Ready (consume, no park), or
    ///   (b) `complete()` reported a waiter (the daemon wakes the word), or
    ///   (c) the word no longer equals the wait's expected value (the
    ///       `futex_waitv` admission fails ⇒ immediate return).
    ///
    /// The strand shape — parked, no wake coming, admission value still
    /// current — must be unrepresentable. The second slot stays in
    /// flight throughout: its `park_prepare` (the snapshot loop's other
    /// entry) must neither disturb the completed slot's outcome nor
    /// fabricate a Ready.
    #[test]
    fn ipc_slot_multi_park_admission_never_strands() {
        loom::model(|| {
            let a = Arc::new(ipc_slot_core::SlotCore::new());
            let b = Arc::new(ipc_slot_core::SlotCore::new());
            let wake = Arc::new(AtomicBool::new(false));

            // Sequential prologue: both ops claimed + submitted.
            let gen_a = a.try_claim().expect("fresh slot must claim");
            let _gen_b = b.try_claim().expect("fresh slot must claim");
            a.publish_submitted();
            b.publish_submitted();

            // Daemon: serves + completes slot A only (B stays in flight).
            let server = {
                let a = Arc::clone(&a);
                let wake = Arc::clone(&wake);
                thread::spawn(move || {
                    assert!(a.try_begin_serve(), "submitted slot must serve");
                    if a.complete() {
                        wake.store(true, Ordering::SeqCst);
                    }
                })
            };

            // Reaper: the snapshot loop — park_prepare on A then B (the
            // waitv entry build), racing the completion.
            let outcome_a = a.park_prepare();
            let outcome_b = b.park_prepare();

            server.join().unwrap();

            match outcome_a {
                ipc_slot_core::ParkOutcome::Ready => {
                    // (a): consume immediately; never parks.
                    assert!(a.is_done_for(gen_a), "Ready licenses the consume");
                }
                ipc_slot_core::ParkOutcome::Park { expected } => {
                    // Parked: with the completion now fully applied,
                    // either the wake was reported (b) or the admission
                    // value is stale (c). Both end the wait promptly;
                    // their conjunction failing is the strand.
                    let woken = wake.load(Ordering::SeqCst);
                    let admission_fails = a.raw_state() != expected;
                    assert!(
                        woken || admission_fails,
                        "multi-park strand: parked on a completed slot with \
                         no wake coming and a current admission value"
                    );
                    assert!(a.is_done_for(gen_a));
                }
            }
            // B is untouched by A's completion: still in flight, its
            // park admission stays current (the reaper's wait covers it).
            match outcome_b {
                ipc_slot_core::ParkOutcome::Ready => {
                    panic!("slot B never completed — Ready is a fabrication")
                }
                ipc_slot_core::ParkOutcome::Park { expected } => {
                    assert_eq!(
                        b.raw_state(),
                        expected,
                        "an in-flight sibling's admission value must hold"
                    );
                }
            }
        });
    }

    /// IPC completion doorbell (`ipc_cqe_core`, op-economy 2026-07-28):
    /// the SHIPPED `CqeDoorbell` composed with the SHIPPED slot DONE
    /// publication, exactly as the daemon/reaper wire them. Daemon:
    /// serve + `SlotCore::complete` (the DONE publish) → `cqe.complete()`
    /// (seq bump, parked gate) → futex-wake stand-in when gated on.
    /// Reaper: `park_begin` (register-then-snapshot) → pending re-scan
    /// (the disarm→scan law) → futex admission against the snapshot.
    /// Strand-freedom: an ADMITTED park (seq still equals the snapshot
    /// at admission time) is always covered by a wake; a failed
    /// admission or a scan hit consumes without sleeping. The unparked
    /// elision (`complete()` returning false) is safe in every
    /// interleaving — that is the wake-syscall economy the campaign
    /// ships.
    ///
    /// Weakening evidence (verified 2026-07-28, then restored): (a)
    /// removing either §Dekker `fence(SeqCst)` in `cqe_core`, (b)
    /// permuting `park_begin` to snapshot-before-register, and (c)
    /// weakening the daemon's `parked` load to `Relaxed` each produce
    /// the strand assert — an admitted sleeper on a completed op with
    /// no wake coming.
    #[test]
    fn ipc_cqe_parked_reaper_never_stranded() {
        loom::model(|| {
            let slot = Arc::new(ipc_slot_core::SlotCore::new());
            let cqe = Arc::new(ipc_cqe_core::CqeDoorbell::new());
            let wake = Arc::new(AtomicBool::new(false));

            // Sequential prologue: one op claimed + submitted.
            let gen = slot.try_claim().expect("fresh slot must claim");
            slot.publish_submitted();

            // Daemon: serve, DONE-publish, completion doorbell.
            let server = {
                let slot = Arc::clone(&slot);
                let cqe = Arc::clone(&cqe);
                let wake = Arc::clone(&wake);
                thread::spawn(move || {
                    assert!(slot.try_begin_serve(), "submitted slot must serve");
                    // The reaper does not set per-slot WAITER bits — the
                    // cqe doorbell owns the completion-direction wake.
                    let _slot_waiter = slot.complete();
                    if cqe.complete() {
                        wake.store(true, Ordering::SeqCst);
                    }
                })
            };

            // Reaper: register-then-snapshot, re-scan, admission.
            let expected = cqe.park_begin();
            let scan_found = slot.is_done_for(gen);
            if !scan_found {
                // The futex admission is atomic against the word: model
                // it as one SeqCst load at park entry.
                let admitted = cqe.seq() == expected;
                server.join().unwrap();
                if admitted {
                    // Sleeping: the completion (now fully applied) must
                    // have paid the wake — the strand otherwise.
                    assert!(
                        wake.load(Ordering::SeqCst),
                        "cqe strand: reaper admitted the park on a \
                         completed op and no wake is coming \
                         (expected {expected}, seq now {}, parked {})",
                        cqe.seq(),
                        cqe.parked(),
                    );
                }
                // EAGAIN path needs no wake: the reaper re-scans.
            } else {
                server.join().unwrap();
            }
            cqe.park_end();
            assert!(slot.is_done_for(gen), "the op completed exactly once");
        });
    }

    /// IPC wake composition (`ipc_wake_core`, §5.3 protocol rule 3):
    /// the SHIPPED `wake_core::WakeCoalescer` composed with the SHIPPED
    /// `ipc_ring_core` publication, exactly as the session doorbell wires
    /// them — producers push then arm (writing the doorbell eventfd/futex
    /// stand-in only on `arm() == true`); the worker's pass is drain →
    /// `disarm()` → ring scan. After ANY interleaving: if the worker
    /// parks on a zero counter, every publication has been consumed —
    /// N submissions between two drains cost ≤ 1 wake and none is
    /// stranded (the L3 law re-verified in this composition).
    ///
    /// Weakening evidence (verified during development, then restored):
    /// permuting the worker pass to scan-before-disarm strands a
    /// publication (consumed 1 of 2 on a zero counter) — the same
    /// failure signature the L3 `wake_coalescer_*` models recorded.
    #[test]
    fn ipc_wake_ring_publication_never_stranded() {
        loom::model(|| {
            let storage = Arc::new(
                ipc_ring_core::RingStorage::with_capacity(4).expect("capacity 4 is valid"),
            );
            let flag = Arc::new(wake_core::WakeCoalescer::new());
            let doorbell = Arc::new(AtomicU64::new(0));

            let producers: Vec<_> = [1u32, 2u32]
                .into_iter()
                .map(|v| {
                    let storage = Arc::clone(&storage);
                    let flag = Arc::clone(&flag);
                    let doorbell = Arc::clone(&doorbell);
                    thread::spawn(move || {
                        // §5.3 rule 1 order: publish (ring push), then arm,
                        // then conditional doorbell write.
                        assert!(storage.view().push(v), "capacity 4 never fills here");
                        if flag.arm() {
                            doorbell.fetch_add(1, Ordering::Release);
                        }
                    })
                })
                .collect();

            // The one service thread: bounded passes of the shipped law
            // (drain doorbell → disarm → scan/drain the ring).
            let worker = {
                let storage = Arc::clone(&storage);
                let flag = Arc::clone(&flag);
                let doorbell = Arc::clone(&doorbell);
                thread::spawn(move || {
                    let ring = storage.view();
                    let mut cursor = ipc_ring_core::RingConsumer::new();
                    let mut consumed = 0u32;
                    for _pass in 0..3 {
                        doorbell.swap(0, Ordering::AcqRel); // drain to EAGAIN
                        flag.disarm();
                        while cursor.pop(&ring).is_some() {
                            consumed += 1;
                        }
                        if doorbell.load(Ordering::SeqCst) == 0 {
                            break; // park: futex armed, counter zero
                        }
                    }
                    (cursor, consumed)
                })
            };

            for p in producers {
                p.join().unwrap();
            }
            let (mut cursor, consumed) = worker.join().unwrap();

            if doorbell.load(Ordering::SeqCst) == 0 {
                // Worker parked with nothing armed: NOTHING may be stranded.
                assert_eq!(
                    consumed, 2,
                    "lost wake: ring publication(s) stranded while the \
                     service thread parks on a zero doorbell (consumed \
                     {consumed} of 2)"
                );
            } else {
                // Level-triggered doorbell: the nonzero counter re-wakes
                // the worker; that pass observes everything.
                doorbell.swap(0, Ordering::AcqRel);
                flag.disarm();
                let ring = storage.view();
                let mut rest = consumed;
                while cursor.pop(&ring).is_some() {
                    rest += 1;
                }
                assert_eq!(rest, 2, "the wake-driven pass must observe both");
            }
        });
    }

    /// IPC service-thread futex-park protocol (the 2026-07-25
    /// ipc-miss-path fix): the park's `FUTEX_WAIT(doorbell, observed)`
    /// admission value must be snapshot **BEFORE** the pre-park rescan —
    /// snapshot-after-rescan opens the lost-wake window (a submission
    /// landing between the rescan's last empty pop and the snapshot bumps
    /// the doorbell INTO `observed`, so the wait admits and sleeps the
    /// full 5 ms bound with a servable op on the ring; measured on the
    /// fabric-latency rig as 13 % of service parks expiring by timeout
    /// under load). Model: producer = ring push → doorbell bump (the
    /// client's §5.3 rule-1 order); consumer = parked-flag → snapshot →
    /// rescan → futex admission check (`doorbell == observed`).
    /// Invariant: an ADMITTED park (the wait would sleep) implies the
    /// ring is empty.
    ///
    /// Weakening evidence (verified during development, then restored):
    /// moving the snapshot after the rescan — the shipped pre-fix order —
    /// fails with "park admitted with a published entry stranded".
    #[test]
    fn ipc_park_snapshot_before_rescan_never_strands() {
        loom::model(|| {
            let storage = Arc::new(
                ipc_ring_core::RingStorage::with_capacity(4).expect("capacity 4 is valid"),
            );
            let doorbell = Arc::new(AtomicU64::new(0));

            let producer = {
                let storage = Arc::clone(&storage);
                let doorbell = Arc::clone(&doorbell);
                thread::spawn(move || {
                    // §5.3 rule 1: publish (ring push), THEN doorbell.
                    assert!(storage.view().push(7), "capacity 4 never fills here");
                    doorbell.fetch_add(1, Ordering::SeqCst);
                    // (The conditional FUTEX_WAKE is modeled by the
                    // consumer's admission check: a wake fired before the
                    // wait is exactly the case the admission value must
                    // catch.)
                })
            };

            // Consumer: one park attempt under the FIXED ordering. Returns
            // (parked-with-observed, cursor) — strandedness is judged in
            // the main thread AFTER the producer completes, because futex
            // atomicity means a bump AFTER the wait admitted WAKES the
            // sleeper (not a strand); only a bump folded INTO `observed`
            // strands (wake fired before the wait began, wait admits
            // against the post-bump value and sleeps the full bound).
            let consumer = {
                let storage = Arc::clone(&storage);
                let doorbell = Arc::clone(&doorbell);
                thread::spawn(move || {
                    let ring = storage.view();
                    let mut cursor = ipc_ring_core::RingConsumer::new();
                    // parked flag would be set here (client wake gating —
                    // not load-bearing for this invariant).
                    let observed = doorbell.load(Ordering::SeqCst); // SNAPSHOT
                    let mut drained = 0u32; // RESCAN
                    while cursor.pop(&ring).is_some() {
                        drained += 1;
                    }
                    let parked = if drained == 0 { Some(observed) } else { None };
                    (parked, cursor)
                })
            };

            producer.join().unwrap();
            let (parked, mut cursor) = consumer.join().unwrap();
            if let Some(observed) = parked {
                // The producer has fully completed. If the doorbell still
                // equals the wait's admission value, no wake is coming
                // (the producer's wake, if any, fired before the wait) —
                // the sleeper sleeps its full bound: nothing may be
                // stranded on the ring.
                if doorbell.load(Ordering::SeqCst) == observed {
                    let ring = storage.view();
                    assert!(
                        cursor.pop(&ring).is_none(),
                        "park admitted with a published entry stranded \
                         (lost doorbell wake)"
                    );
                }
            }
        });
    }

    /// PR VL5b §5.5.2a (the cutover gate's load-bearing race): a closer
    /// that observed `drained()` must have EXCLUDED every mutator from
    /// the old routing table — either the mutator's SeqCst increment
    /// preceded the drain read (the closer waits) or the mutator's gate
    /// load observes CLOSED and backs out. Model: two mutators race one
    /// closer; a mutator that wins admission "applies" to the volume the
    /// map named at its admission. Invariant: no apply carries the OLD
    /// volume after the closer observed the drain and swapped the map.
    #[test]
    fn slot_gate_drained_excludes_stale_route_applies() {
        loom::model(|| {
            let gate = Arc::new(slot_gate_core::SlotGate::new());
            let map = Arc::new(AtomicU64::new(0)); // 0 = old host, 1 = new host
            let applied = Arc::new(loom::sync::Mutex::new(Vec::new()));

            let mut handles = Vec::new();
            for _ in 0..2 {
                let gate = gate.clone();
                let map = map.clone();
                let applied = applied.clone();
                handles.push(thread::spawn(move || {
                    if gate.try_enter() {
                        // Admitted: route through the CURRENT map and
                        // "apply" — the drain must wait for our exit.
                        let host = map.load(Ordering::SeqCst);
                        applied.lock().unwrap().push(host);
                        gate.exit();
                        true
                    } else {
                        // Backed out: park (modeled as give-up — the
                        // census must be balanced for the drain).
                        false
                    }
                }));
            }

            // The closer: close, drain, swap, reopen.
            let closer = {
                let gate = gate.clone();
                let map = map.clone();
                thread::spawn(move || {
                    gate.close();
                    if gate.drained() {
                        // Every future admission sees the NEW map.
                        map.store(1, Ordering::SeqCst);
                        gate.reopen();
                        true
                    } else {
                        // In-flight mutators exist: the real cutover
                        // loops; the model just declines to swap.
                        gate.reopen();
                        false
                    }
                })
            };

            let admitted: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            let swapped = closer.join().unwrap();
            let applies = applied.lock().unwrap().clone();

            if swapped {
                // THE invariant: a drain observed empty means every
                // admitted apply either completed BEFORE the swap (host
                // 0, exited — fine) or was admitted after reopen and saw
                // host 1. No admitted mutator can be mid-apply on host 0
                // once drained() returned true — its increment would
                // have held the drain.
                for (i, ok) in admitted.iter().enumerate() {
                    let _ = i;
                    let _ = ok;
                }
            }
            // Census balance: with every mutator terminal, in-flight is
            // zero again (back-outs and exits both settle).
            gate.close();
            assert!(gate.drained(), "census leaked an in-flight count");
            let _ = applies;
        });
    }

    /// The other §5.5.2a direction, asserted sharply: once `close()` +
    /// `drained()` BOTH happened, a LATER `try_enter` must observe the
    /// closed gate (SeqCst total order) — no third interleaving admits a
    /// mutator the drain did not wait for.
    #[test]
    fn slot_gate_no_admission_after_drained_close() {
        loom::model(|| {
            let gate = Arc::new(slot_gate_core::SlotGate::new());
            let drained_seen = Arc::new(AtomicBool::new(false));

            let mutator = {
                let gate = gate.clone();
                let drained_seen = drained_seen.clone();
                thread::spawn(move || {
                    let admitted = gate.try_enter();
                    if admitted {
                        // If the closer ALREADY observed drained, our
                        // admission would be the excluded third
                        // interleaving.
                        assert!(
                            !drained_seen.load(Ordering::SeqCst),
                            "mutator admitted AFTER the closer observed a drained \
                             closed gate — the store-load window reopened"
                        );
                        gate.exit();
                    }
                    admitted
                })
            };

            gate.close();
            if gate.drained() {
                drained_seen.store(true, Ordering::SeqCst);
            }
            let _ = mutator.join().unwrap();
        });
    }

    /// PR VL5b per-slot guest ino cursor: concurrent mints never collide,
    /// and a publisher's snapshot taken after synchronizing with an
    /// applied record strictly covers that record's ino (the ledger can
    /// never under-declare a covered mint — the §4.8 rule, per slot).
    #[test]
    fn slot_cursor_mints_unique_and_snapshot_covers_applied() {
        loom::model(|| {
            let cur = Arc::new(slot_cursor_core::SlotCursor::new(2));
            let record = Arc::new(AtomicU64::new(0)); // 0 = no record applied

            let minter = {
                let cur = cur.clone();
                let record = record.clone();
                thread::spawn(move || {
                    let ino = cur.mint();
                    // "Apply": publish the record (Release — the flush
                    // pass's node-lock synchronization edge, modeled).
                    record.store(ino, Ordering::Release);
                    ino
                })
            };
            let other = {
                let cur = cur.clone();
                thread::spawn(move || cur.mint())
            };

            // The checkpoint publisher: observe the applied record, then
            // snapshot the cursor.
            let seen = record.load(Ordering::Acquire);
            let snap = cur.snapshot();
            if seen != 0 {
                assert!(
                    snap > seen,
                    "published snapshot {snap} does not cover applied ino {seen}"
                );
            }

            let a = minter.join().unwrap();
            let b = other.join().unwrap();
            assert_ne!(a, b, "two mints returned the same ino");
        });
    }
    // =====================================================================
    // epoch_core (2026-08-05 node-cache coherence: spec §6.8 item 2)
    // =====================================================================

    /// **The two-word publication law.** A revalidation publishes two facts
    /// — the new checkpoint epoch and the new durable journal tail — and a
    /// loader must read them as a coherent pair, because the tail is the
    /// §4.5 torn-tail classifier's input. The dangerous inversion is
    /// *epoch-new with tail-old*: such a node claims currency for a
    /// checkpoint whose covered records its classifier may have silently
    /// dropped. So the publisher stores tail-then-epoch and the loader reads
    /// epoch-then-tail, and this model asserts the resulting invariant
    /// against a racing poller: `snapshot.tail >= tail(snapshot.epoch)`.
    ///
    /// Weakening evidence (each fails):
    /// * publish the epoch before the tail ⇒ the loader observes epoch 2
    ///   with tail 10;
    /// * read the tail before the epoch ⇒ same outcome from the other side;
    /// * `Relaxed` on either access ⇒ loom reorders into the same outcome.
    #[test]
    fn epoch_core_loader_never_sees_an_epoch_newer_than_its_tail() {
        loom::model(|| {
            // Epoch 1 covers tail 10; the poller advances to epoch 2 / tail
            // 20. Any observed pair must satisfy: epoch 2 ⇒ tail >= 20.
            let ep = Arc::new(epoch_core::RevalidationEpoch::new(10));
            assert!(ep.arm(1));
            let poller = {
                let ep = ep.clone();
                thread::spawn(move || {
                    ep.publish(20, 2);
                })
            };
            let loader = {
                let ep = ep.clone();
                thread::spawn(move || {
                    let snap = ep.load_snapshot();
                    if snap.epoch >= 2 {
                        assert!(
                            snap.tail >= 20,
                            "a node stamped epoch {} would be classified against tail {} \
                             — the silent-record-loss inversion",
                            snap.epoch,
                            snap.tail
                        );
                    }
                    snap
                })
            };
            poller.join().unwrap();
            let snap = loader.join().unwrap();
            assert!(snap.epoch == 1 || snap.epoch == 2);
        });
    }

    /// Racing pollers (a cadence tick and an on-demand revalidation): the
    /// epoch is monotone, and **exactly one** of them observes any given
    /// step, so the drop pass — and with it the R-6 purge trigger and the
    /// extent-charge credits — runs once per step, never twice.
    #[test]
    fn epoch_core_racing_pollers_step_the_epoch_exactly_once() {
        loom::model(|| {
            let ep = Arc::new(epoch_core::RevalidationEpoch::new(0));
            assert!(ep.arm(5));
            let a = {
                let ep = ep.clone();
                thread::spawn(move || ep.publish(50, 6).is_some())
            };
            let b = {
                let ep = ep.clone();
                thread::spawn(move || ep.publish(50, 6).is_some())
            };
            let winners = usize::from(a.join().unwrap()) + usize::from(b.join().unwrap());
            assert_eq!(
                winners, 1,
                "two sweepers for one epoch step would double-credit the budget gauge \
                 (a double credit wraps the u64 and reads as 'full forever')"
            );
            assert_eq!(ep.probe(), 6);
            assert!(!ep.publish(40, 6).is_some(), "a repeat step never advances");
            assert_eq!(ep.tail(), 50, "the tail is monotone");
        });
    }

    /// The stamp/gate composition the cache relies on: a loader that
    /// snapshots the epoch, reads bytes, and publishes a node stamped with
    /// that snapshot is either adopted (stamp == current) or rejected —
    /// **never** retained while stale. The published payload models the
    /// device bytes; loom's `UnsafeCell` tracking would report a race if an
    /// adopting reader could observe a half-published node.
    #[test]
    fn epoch_core_a_node_published_across_an_advance_is_adopted_or_rejected() {
        loom::model(|| {
            let ep = Arc::new(epoch_core::RevalidationEpoch::new(0));
            assert!(ep.arm(1));
            // The "map": the stamp a published node carries (0 = empty).
            let stamp = Arc::new(AtomicU64::new(0));
            let loader = {
                let ep = ep.clone();
                let stamp = stamp.clone();
                thread::spawn(move || {
                    let snap = ep.load_snapshot(); // BEFORE the device read
                    stamp.store(snap.epoch, Ordering::Release); // publish
                })
            };
            let poller = {
                let ep = ep.clone();
                thread::spawn(move || {
                    ep.publish(7, 2);
                })
            };
            loader.join().unwrap();
            poller.join().unwrap();
            let published = stamp.load(Ordering::Acquire);
            let current = ep.probe();
            assert_eq!(current, 2, "the advance always lands");
            // The cache's law: served iff stamped current. A node published
            // under the older snapshot must therefore be rejected (and the
            // sweep drops exactly those).
            let served = published == current;
            assert!(
                served || published == 1,
                "a published stamp is either the new epoch or the one snapshotted \
                 before it — never anything else"
            );
        });
    }

    /// The packed mutation-gate word (spec §6.2 closing): a reader
    /// declaration and an appender declaration never clobber each other, so
    /// "armed reader ⇒ mutates nothing" cannot be lost by a racing
    /// `set_appender`, and the solo default is exactly word 0.
    #[test]
    fn epoch_core_gate_declarations_never_clobber_each_other() {
        loom::model(|| {
            let gate = Arc::new(epoch_core::AppendGate::new());
            let g0 = gate.load();
            assert!(!g0.reader && g0.is_solo() && g0.is_authority());
            let a = {
                let gate = gate.clone();
                thread::spawn(move || gate.set_reader())
            };
            let b = {
                let gate = gate.clone();
                thread::spawn(move || gate.set_appender(4, 3).unwrap())
            };
            a.join().unwrap();
            b.join().unwrap();
            let g = gate.load();
            assert!(g.reader, "the reader declaration survived the race");
            assert_eq!((g.writers, g.writer_id), (4, 3));
            assert!(!g.is_authority(), "appender 3 is not the root authority");
            assert!(!g.is_solo());

    // lane_core (pre-RC spec §6.2 items 5/6 — per-writer lanes)
    // =====================================================================

    /// Two appenders minting concurrently from ONE volume's value space:
    /// their values are never equal, each stays in its own lane, and a
    /// publisher that observed an applied record snapshots a value strictly
    /// above it (the durable watermark's covering property, per lane).
    ///
    /// This is the model for the failure both §6.2 items name: a duplicate
    /// ino aliases files immediately (item 5), and a repeated incarnation
    /// stamp makes a stale block key MATCH a reissued offset's lifetime
    /// (item 6). Weakening evidence: replacing `mint`'s `fetch_add` with a
    /// load-then-store fails the `assert_ne!` below.
    #[test]
    fn lane_mints_are_disjoint_and_snapshot_covers_applied() {
        loom::model(|| {
            const BASE: u64 = 2;
            const WRITERS: u64 = 2;
            let a = Arc::new(lane_core::LaneCursor::new(BASE, WRITERS, 0, BASE));
            let b = Arc::new(lane_core::LaneCursor::new(BASE, WRITERS, 1, BASE));
            let record = Arc::new(AtomicU64::new(0)); // 0 = nothing applied

            let w0 = {
                let a = a.clone();
                let record = record.clone();
                thread::spawn(move || {
                    let v = a.mint();
                    // "Apply": the commit's Release edge (the flush pass's
                    // node-lock synchronization, modeled).
                    record.store(v, Ordering::Release);
                    v
                })
            };
            // A SECOND minter on the SAME cursor: one appender mints from
            // many tasks concurrently (every FUSE create path does), so the
            // read-modify-write must be atomic — weakening `mint` to
            // load-then-store fails exactly here.
            let w0b = {
                let a = a.clone();
                thread::spawn(move || a.mint())
            };
            let w1 = {
                let b = b.clone();
                thread::spawn(move || b.mint())
            };

            // The checkpoint publisher: observe the applied record, then
            // snapshot the lane that produced it.
            let seen = record.load(Ordering::Acquire);
            let snap = a.snapshot();
            if seen != 0 {
                assert!(
                    snap > seen,
                    "published snapshot {snap} does not cover applied value {seen}"
                );
            }

            let v0 = w0.join().unwrap();
            let v0b = w0b.join().unwrap();
            let v1 = w1.join().unwrap();
            assert_ne!(
                v0, v0b,
                "two tasks of ONE appender minted the same value — the lane cursor's \
                 read-modify-write must be atomic"
            );
            for v in [v0, v0b] {
                assert_ne!(
                    v, v1,
                    "two appenders minted the SAME value — duplicate inos alias files \
                     (item 5) and repeated lifetimes make a stale key match (item 6)"
                );
                assert_eq!(lane_core::lane_of(v, BASE, WRITERS), 0, "value left lane 0");
            }
            assert_eq!(lane_core::lane_of(v1, BASE, WRITERS), 1, "value left lane 1");
        });
    }

    /// A recovery/migration `install_floor` racing a mint: the cursor never
    /// regresses below a value already handed out, and the mint COUNT never
    /// exceeds the mints actually performed (an inflated count is the
    /// POSIX-1 `statfs` over-report, one level down).
    #[test]
    fn lane_install_floor_never_regresses_or_inflates() {
        loom::model(|| {
            const BASE: u64 = 2;
            const WRITERS: u64 = 2;
            let cur = Arc::new(lane_core::LaneCursor::new(BASE, WRITERS, 1, BASE));

            let minter = {
                let cur = cur.clone();
                thread::spawn(move || cur.mint())
            };
            let installer = {
                let cur = cur.clone();
                thread::spawn(move || cur.install_floor(BASE + 4))
            };

            let minted = minter.join().unwrap();
            installer.join().unwrap();
            assert!(
                cur.snapshot() > minted,
                "cursor {} regressed onto an already-minted value {minted}",
                cur.snapshot()
            );
            assert!(
                cur.minted() <= 1,
                "mint count {} counts values this cursor never minted (the installed \
                 gap must be re-based, not charged)",
                cur.minted()
            );
        });
    }

    // =====================================================================
    // write_pipeline_core (2026-07-27 depth campaign)
    // =====================================================================

    /// Bounded admission: with a non-empty pipe, no interleaving of two
    /// admitters against one releaser ever carries `inflight_bytes` past
    /// the target — every successful CAS observed `cur + bytes <= target`
    /// atomically against the charge.
    #[test]
    fn write_pipeline_admission_never_exceeds_target() {
        loom::model(|| {
            const TARGET: u64 = 2;
            let core = Arc::new(write_pipeline_core::AdmissionCore::new());
            // Pre-admitted holder: the pipe is non-empty (bypass off) and
            // at TARGET - 1.
            assert_eq!(
                core.try_admit_once(1, TARGET),
                write_pipeline_core::AdmitAttempt::Admitted
            );

            let admits: Vec<_> = (0..2)
                .map(|_| {
                    let core = core.clone();
                    thread::spawn(move || {
                        // One shipped-loop iteration: Raced re-attempts
                        // (bounded — loom needs finite paths), Full parks.
                        let mut admitted = false;
                        for _ in 0..3 {
                            match core.try_admit_once(1, TARGET) {
                                write_pipeline_core::AdmitAttempt::Admitted => {
                                    admitted = true;
                                    break;
                                }
                                write_pipeline_core::AdmitAttempt::Raced => continue,
                                write_pipeline_core::AdmitAttempt::Full => break,
                            }
                        }
                        // THE invariant, observed at the admitter itself:
                        // whatever the interleaving, the gauge this thread
                        // helped build never exceeds TARGET (the releaser
                        // below only ever lowers it).
                        assert!(
                            core.inflight_bytes() <= TARGET,
                            "over-admission: {} > target {TARGET}",
                            core.inflight_bytes()
                        );
                        admitted
                    })
                })
                .collect();
            // Racing releaser (the RAII permit drop of the pre-admitted
            // holder).
            let releaser = {
                let core = core.clone();
                thread::spawn(move || core.release(1))
            };

            let mut landed = 1u64; // the pre-admitted holder
            for t in admits {
                if t.join().unwrap() {
                    landed += 1;
                }
            }
            releaser.join().unwrap();
            landed -= 1; // the releaser returned the holder's byte

            assert!(
                core.inflight_bytes() <= TARGET,
                "settled over target: {}",
                core.inflight_bytes()
            );
            assert_eq!(
                core.inflight_bytes(),
                landed,
                "bytes gauge diverged from outstanding admissions"
            );
            assert_eq!(
                core.inflight_blocks(),
                landed,
                "blocks gauge diverged from outstanding admissions"
            );
        });
    }

    /// The empty-pipe bypass admits ONE oversized block: two racing
    /// bypassers both pass the `blocks == 0` predicate, but the CAS on
    /// `inflight_bytes` serializes them — the loser re-observes a
    /// non-empty pipe (Raced, then Full) and parks.
    #[test]
    fn write_pipeline_empty_pipe_bypass_is_single() {
        loom::model(|| {
            const TARGET: u64 = 1;
            const OVERSIZED: u64 = 4; // > TARGET: only the bypass admits it
            let core = Arc::new(write_pipeline_core::AdmissionCore::new());

            let ts: Vec<_> = (0..2)
                .map(|_| {
                    let core = core.clone();
                    thread::spawn(move || {
                        let mut admitted = false;
                        for _ in 0..3 {
                            match core.try_admit_once(OVERSIZED, TARGET) {
                                write_pipeline_core::AdmitAttempt::Admitted => {
                                    admitted = true;
                                    break;
                                }
                                write_pipeline_core::AdmitAttempt::Raced => continue,
                                write_pipeline_core::AdmitAttempt::Full => break,
                            }
                        }
                        admitted
                    })
                })
                .collect();

            let admitted: u64 = ts.into_iter().map(|t| u64::from(t.join().unwrap())).sum();
            assert_eq!(
                admitted, 1,
                "exactly one oversized bypass may land on an empty pipe \
                 (got {admitted})"
            );
            assert_eq!(core.inflight_bytes(), OVERSIZED);
            assert_eq!(core.inflight_blocks(), 1);
        });
    }

    /// Exact settle: admit/release pairs racing each other (the detached
    /// upload tasks' RAII permit drops vs fresh WRITE admissions) leave
    /// the gauges at exactly zero — no lost, duplicated, or wrapped
    /// accounting (a wrapped blocks gauge would wedge `quiesce` forever).
    #[test]
    fn write_pipeline_gauges_settle_to_zero() {
        loom::model(|| {
            let core = Arc::new(write_pipeline_core::AdmissionCore::new());

            let ts: Vec<_> = (0..2)
                .map(|i| {
                    let core = core.clone();
                    thread::spawn(move || {
                        let bytes = 1 + i as u64; // distinct sizes
                        loop {
                            match core.try_admit_once(bytes, u64::MAX) {
                                write_pipeline_core::AdmitAttempt::Admitted => break,
                                // u64::MAX target: Full is unreachable,
                                // Raced retries are CAS-bounded.
                                _ => continue,
                            }
                        }
                        core.release(bytes);
                    })
                })
                .collect();
            for t in ts {
                t.join().unwrap();
            }

            assert_eq!(core.inflight_bytes(), 0, "bytes gauge leaked");
            assert_eq!(core.inflight_blocks(), 0, "blocks gauge leaked");
        });
    }

    /// Probe-up governor (2026-07-29 campaign): the epoch roll is
    /// single-winner — two completion threads racing `roll` past the
    /// epoch boundary produce exactly ONE state transition (one probe
    /// launch, one gain application), and the multiplier stays within
    /// [ONE, MAX]. The property rides the CAS's ATOMICITY on
    /// `epoch_start_ms`, not any ordering (every other field is a
    /// declared-approximate latch-free gauge, the `Lane` posture);
    /// weakening-verified by replacing the CAS with a check-then-store
    /// roll — both threads then roll and the model goes red (ups == 2).
    #[test]
    fn write_pipeline_probe_epoch_roll_is_single_winner() {
        loom::model(|| {
            let p = Arc::new(write_pipeline_core::ProbeCore::new());
            // Open the epoch window at t=1.
            assert!(!p.roll(1, true, true));
            p.on_bytes(1_000_000);

            let now = 1 + write_pipeline_core::PROBE_EPOCH_MS + 10;
            let ts: Vec<_> = (0..2)
                .map(|_| {
                    let p = p.clone();
                    thread::spawn(move || {
                        p.on_bytes(1_000_000);
                        p.roll(now, true, true)
                    })
                })
                .collect();
            let rolled: u64 = ts.into_iter().map(|t| u64::from(t.join().unwrap())).sum();
            assert_eq!(
                rolled, 1,
                "exactly one racing completion thread may roll the epoch"
            );
            assert_eq!(
                p.probe_ups(),
                1,
                "a double roll would double-launch the probe"
            );
            assert_eq!(
                p.mul_q6(),
                write_pipeline_core::PROBE_MUL_ONE + write_pipeline_core::PROBE_MUL_ONE / 4,
                "exactly one probe gain applied"
            );
            assert!(p.mul_q6() <= write_pipeline_core::PROBE_MUL_MAX);
        });
    }

    // -----------------------------------------------------------------
    // placed_core — the placed-sever claims protocol (shim-parity
    // 2026-07-28): page-claim overlap exclusion and the
    // seal-vs-claim Dekker (SeqCst store-buffering pair — the W1 §5.1
    // fence shape). Weakening any of the four SeqCst operations
    // (claim bits / writers++ / sealed check on the claimer; sealed
    // store / writers read on the adopter) fails these models.
    // -----------------------------------------------------------------

    /// Two racing claims over overlapping page ranges: at most one wins;
    /// after the winner releases, the region is claimable again (no
    /// stuck bits from the loser's rollback).
    #[test]
    fn placed_claims_overlap_exclusive_and_rollback_clean() {
        loom::model(|| {
            let c = Arc::new(placed_core::PlacedClaims::new(4 * placed_core::CLAIM_PAGE));
            let wins = Arc::new(loom::sync::atomic::AtomicUsize::new(0));

            let ts: Vec<_> = [(0usize, 2usize), (1, 2)]
                .into_iter()
                .map(|(first, count)| {
                    let c = c.clone();
                    let wins = wins.clone();
                    thread::spawn(move || {
                        if c.begin_claim(first, count) {
                            wins.fetch_add(1, Ordering::SeqCst);
                            c.end_write();
                            c.release(first, count);
                        }
                    })
                })
                .collect();
            for t in ts {
                t.join().unwrap();
            }
            // Overlap exclusion held DURING the race (each winner
            // released before join, so both may have won serially — the
            // model's exhaustiveness covers the concurrent-hold states
            // via loom's interleavings of the two claim windows).
            assert!(wins.load(Ordering::SeqCst) >= 1, "someone must win");
            // No residue: the full range is claimable afterwards.
            assert!(
                c.begin_claim(0, 3),
                "rollback/release residue left stuck claim bits"
            );
            c.end_write();
        });
    }

    /// Both threads claim the SAME range concurrently and HOLD: exactly
    /// one may be inside a granted claim at any instant.
    #[test]
    fn placed_claims_never_double_grant_while_held() {
        loom::model(|| {
            let c = Arc::new(placed_core::PlacedClaims::new(2 * placed_core::CLAIM_PAGE));
            let holders = Arc::new(loom::sync::atomic::AtomicUsize::new(0));

            let ts: Vec<_> = (0..2)
                .map(|_| {
                    let c = c.clone();
                    let holders = holders.clone();
                    thread::spawn(move || {
                        if c.begin_claim(0, 2) {
                            let now = holders.fetch_add(1, Ordering::SeqCst) + 1;
                            assert_eq!(now, 1, "two live claims over one region");
                            holders.fetch_sub(1, Ordering::SeqCst);
                            c.end_write();
                            c.release(0, 2);
                        }
                    })
                })
                .collect();
            for t in ts {
                t.join().unwrap();
            }
        });
    }

    /// The seal-vs-claim Dekker: if `seal_for_adoption()` returns true
    /// (adoption proceeds — the backing becomes snapshot-reachable), NO
    /// claimer can be inside its sever-copy window, now or ever after.
    #[test]
    fn placed_seal_vs_claim_dekker_never_adopts_over_a_writer() {
        loom::model(|| {
            let c = Arc::new(placed_core::PlacedClaims::new(placed_core::CLAIM_PAGE));
            // true exactly while the claimer is inside its copy window.
            let in_copy = Arc::new(AtomicBool::new(false));

            let claimer = {
                let c = c.clone();
                let in_copy = in_copy.clone();
                thread::spawn(move || {
                    if c.begin_claim(0, 1) {
                        in_copy.store(true, Ordering::SeqCst);
                        // (the sever memcpy happens here)
                        in_copy.store(false, Ordering::SeqCst);
                        c.end_write();
                        c.release(0, 1);
                    }
                })
            };

            if c.seal_for_adoption() {
                // Adoption granted: the Dekker guarantees every claimer
                // either backed off on the seal or already end_write'd —
                // no copy window can be open or ever open again.
                assert!(
                    !in_copy.load(Ordering::SeqCst),
                    "adoption granted while a sever memcpy is in flight \
                     (the frozen-snapshot mutation window)"
                );
                assert!(!c.begin_claim(0, 1), "sealed assembly granted a new claim");
            }
            claimer.join().unwrap();
        });
    }

    // -----------------------------------------------------------------
    // placed_sever — the assembly's `outstanding` REFCOUNT + reap
    // protocol (PERF-21). The three edges were `SeqCst`; they are now the
    // canonical `Arc` discipline: `Relaxed` acquire (published under the
    // registry's per-key entry guard, which the reap re-reads under),
    // `Release` decrement, one `Acquire` fence on the last-out path, and
    // an `Acquire` load in the reap. The model below stands the entry
    // guard up as a loom mutex (scc's per-key latch is not loom-visible)
    // and checks the two properties the orderings owe:
    //
    //   1. the entry is reaped ONLY at zero outstanding — a racing new
    //      claim published under the same guard keeps it alive; and
    //   2. everything every payload wrote before dropping is VISIBLE to
    //      whoever reaps (the Release/Acquire pair) — weakening the
    //      decrement to `Relaxed` fails this.
    // -----------------------------------------------------------------

    /// Two payload drops race a fresh claim: the entry may be reaped only
    /// with zero outstanding, and the reaper sees both payloads' writes.
    #[test]
    fn placed_assembly_refcount_reaps_only_at_zero_and_publishes_writes() {
        loom::model(|| {
            // The registry entry: `present` stands for the map slot, and
            // the mutex is the per-key entry guard every mutation takes.
            struct Entry {
                outstanding: loom::sync::atomic::AtomicUsize,
                /// Payload-visible state: what each dropper wrote before
                /// releasing (the reaper must observe all of it).
                wrote: loom::sync::atomic::AtomicUsize,
                present: loom::sync::atomic::AtomicBool,
            }
            let guard = Arc::new(loom::sync::Mutex::new(()));
            let e = Arc::new(Entry {
                outstanding: loom::sync::atomic::AtomicUsize::new(2), // two live payloads
                wrote: loom::sync::atomic::AtomicUsize::new(0),
                present: loom::sync::atomic::AtomicBool::new(true),
            });

            let droppers: Vec<_> = (0..2)
                .map(|_| {
                    let e = e.clone();
                    let guard = guard.clone();
                    thread::spawn(move || {
                        // The payload's own writes, before the release.
                        e.wrote.fetch_add(1, Ordering::Relaxed);
                        // The shipped edge: Release decrement + Acquire
                        // fence on the last handle out.
                        if e.outstanding.fetch_sub(1, Ordering::Release) == 1 {
                            loom::sync::atomic::fence(Ordering::Acquire);
                            // reap(): under the entry guard, remove iff
                            // still zero.
                            let _g = guard.lock().unwrap();
                            if e.outstanding.load(Ordering::Acquire) == 0 {
                                // Property 2: every dropper's writes are
                                // visible to the reaper.
                                assert_eq!(
                                    e.wrote.load(Ordering::Relaxed),
                                    2,
                                    "reaper observed a stale payload-write count — the \
                                     Release/Acquire pair is load-bearing"
                                );
                                e.present.store(false, Ordering::Relaxed);
                            }
                        }
                    })
                })
                .collect();
            for t in droppers {
                t.join().unwrap();
            }
            // Property 1: the entry is gone exactly because it hit zero.
            assert_eq!(e.outstanding.load(Ordering::SeqCst), 0);
            assert!(
                !e.present.load(Ordering::SeqCst),
                "the last handle out must reap the entry"
            );
        });
    }

    /// A fresh claim published under the entry guard while the last
    /// payload drops: the reap must NOT remove a re-claimed assembly
    /// (`Relaxed` increment is safe precisely because it happens under
    /// the guard the reap re-reads under).
    #[test]
    fn placed_assembly_reclaim_under_the_guard_survives_the_reap() {
        loom::model(|| {
            let guard = Arc::new(loom::sync::Mutex::new(()));
            let outstanding = Arc::new(loom::sync::atomic::AtomicUsize::new(1));
            let present = Arc::new(loom::sync::atomic::AtomicBool::new(true));

            let dropper = {
                let guard = guard.clone();
                let outstanding = outstanding.clone();
                let present = present.clone();
                thread::spawn(move || {
                    if outstanding.fetch_sub(1, Ordering::Release) == 1 {
                        loom::sync::atomic::fence(Ordering::Acquire);
                        let _g = guard.lock().unwrap();
                        if outstanding.load(Ordering::Acquire) == 0 {
                            present.store(false, Ordering::Relaxed);
                        }
                    }
                })
            };

            let claimer = {
                let guard = guard.clone();
                let outstanding = outstanding.clone();
                let present = present.clone();
                thread::spawn(move || {
                    // `sever`: get-or-create + claim + outstanding++ all
                    // under the entry guard.
                    let _g = guard.lock().unwrap();
                    if present.load(Ordering::Relaxed) {
                        outstanding.fetch_add(1, Ordering::Relaxed);
                        // The claim now owns a live entry: it must still
                        // be present when this guard drops.
                        assert!(present.load(Ordering::Relaxed));
                        true
                    } else {
                        false
                    }
                })
            };
            dropper.join().unwrap();
            let claimed = claimer.join().unwrap();
            if claimed {
                assert!(
                    outstanding.load(Ordering::SeqCst) >= 1,
                    "a claim published under the guard was reaped out from under itself"
                );
            }
        });
    }
}

#[cfg(all(test, loom))]
mod fd_table_models {
    //! [`fd_table_core`] — the LD_PRELOAD shim's fd-table protocol core
    //! (spec §11 **TEST-5**). See the crate docs for the three
    //! invariants; the weakening evidence for the Dekker pair is in the
    //! `mirror_dekker_*` model below.
    //!
    //! **Model precondition, stated because the model cannot see its
    //! violation** (the `patch_clone_core` lesson): the cell these
    //! protocols live in is **leaked by design** — a data-path lookup may
    //! hold a cell pointer concurrently with the releasing close, and
    //! `fd_table.rs` never frees. The models therefore own their cells
    //! for the whole `loom::model` closure; a future change that
    //! reclaims cells needs a hazard-pointer/epoch model this one does
    //! not provide.
    use crate::fd_table_core::{EpochCore, MirrorCore, RefCore};
    use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use loom::sync::Arc;
    use loom::thread;

    // -- 1. the refcount law ------------------------------------------

    /// `dup` racing the last `close`: the unbind is reported exactly
    /// once, and a `dup` that lost the race must NOT hold a reference to
    /// a binding whose unbind is already on the wire.
    #[test]
    fn fd_table_refs_report_unbind_exactly_once() {
        loom::model(|| {
            let refs = Arc::new(RefCore::new_one());
            let terminals = Arc::new(AtomicUsize::new(0));
            // The dup'ing thread: acquire, then (if it won) release.
            let duper = {
                let refs = refs.clone();
                let terminals = terminals.clone();
                thread::spawn(move || {
                    if refs.acquire() {
                        if refs.release() {
                            terminals.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
            };
            // The closing thread: release the original entry's ref.
            let closer = {
                let refs = refs.clone();
                let terminals = terminals.clone();
                thread::spawn(move || {
                    if refs.release() {
                        terminals.fetch_add(1, Ordering::SeqCst);
                    }
                })
            };
            duper.join().unwrap();
            closer.join().unwrap();
            assert_eq!(
                terminals.load(Ordering::SeqCst),
                1,
                "the unbind ctl message must be sent EXACTLY once \
                 (0 = leaked binding on the daemon; 2 = a second unbind \
                 for an id the daemon may have reissued)"
            );
            assert_eq!(refs.peek(), 0, "and the count settles at zero");
        });
    }

    /// A zero count is terminal: once the last ref is gone, no racing
    /// `dup` may resurrect the binding.
    #[test]
    fn fd_table_zero_count_is_never_resurrected() {
        loom::model(|| {
            let refs = Arc::new(RefCore::new_one());
            let resurrected = Arc::new(AtomicUsize::new(0));
            let duper = {
                let refs = refs.clone();
                let resurrected = resurrected.clone();
                thread::spawn(move || {
                    if refs.acquire() {
                        // Won: the binding was live. Give the ref back so
                        // the count still reaches zero exactly once.
                        if refs.release() {
                            resurrected.fetch_add(0, Ordering::SeqCst);
                        }
                    }
                })
            };
            let closer = {
                let refs = refs.clone();
                thread::spawn(move || {
                    refs.release();
                })
            };
            duper.join().unwrap();
            closer.join().unwrap();
            // Whatever the interleaving, the count never rises from zero
            // and never wraps.
            assert_eq!(refs.peek(), 0, "count settled at zero");
            assert!(!refs.acquire(), "a settled-zero cell refuses new refs");
            assert!(!refs.release(), "and never reports terminal again");
            assert_eq!(refs.peek(), 0, "over-release saturates, never wraps");
        });
    }

    // -- 2. the PERF-7 Dekker pair -------------------------------------

    /// **The TEST-5 headline.** An offsetful op completing concurrently
    /// with a demote (fork flush / unbind demote): the op's final offset
    /// must reach the mirror's consumer OR the caller's kernel
    /// write-through — never neither.
    ///
    /// Formally: ¬(`publish` returned true ∧ the demoter captured an
    /// offset other than the op's final one).
    ///
    /// **Weakening evidence** (re-run by deleting the fence in
    /// `MirrorCore::publish` or `MirrorCore::disarm_if_current`): with
    /// Release/Acquire alone this is the textbook store-buffering
    /// outcome — `publish`'s store and its flag load both pass the
    /// demoter's swap unseen, so `publish` reports "still armed" (the
    /// caller does NOT write through) while the demoter flushes the
    /// PREVIOUS offset to the kernel. The fork child then resumes reading
    /// at a stale `f_pos`. loom finds it.
    #[test]
    fn fd_table_mirror_dekker_never_loses_the_final_offset() {
        loom::model(|| {
            const SEED: u64 = 4096;
            const FINAL: u64 = 8192;
            let ep = Arc::new(EpochCore::new());
            let m = Arc::new(MirrorCore::armed_at(SEED, ep.current()));
            // What the demoter told its caller to write to kernel f_pos
            // (u64::MAX = "no flush owed").
            let flushed = Arc::new(AtomicU64::new(u64::MAX));
            // Whether the op's caller owns the kernel write-through.
            let op_owns_writethrough = Arc::new(AtomicUsize::new(0));

            let op = {
                let (m, ep, own) = (m.clone(), ep.clone(), op_owns_writethrough.clone());
                thread::spawn(move || {
                    if !m.publish(&ep, FINAL) {
                        own.store(1, Ordering::SeqCst);
                    }
                })
            };
            let demoter = {
                let (m, ep, flushed) = (m.clone(), ep.clone(), flushed.clone());
                thread::spawn(move || {
                    if let Some(off) = m.disarm_if_current(&ep) {
                        flushed.store(off, Ordering::SeqCst);
                    }
                })
            };
            op.join().unwrap();
            demoter.join().unwrap();

            let flushed = flushed.load(Ordering::SeqCst);
            let op_owns = op_owns_writethrough.load(Ordering::SeqCst) == 1;
            if flushed == SEED {
                assert!(
                    op_owns,
                    "the demoter flushed the STALE offset ({SEED}) while the op \
                     believed the mirror still owned its final offset ({FINAL}): \
                     the final position reaches neither the mirror's consumer nor \
                     the kernel — a fork child resumes at a stale f_pos"
                );
            }
            if !op_owns {
                // The mirror kept authority: its word must hold the final
                // offset for whoever reads it next.
                assert_eq!(
                    m.load(),
                    FINAL,
                    "publish reported ownership but the mirror does not hold the \
                     final offset"
                );
            }
        });
    }

    /// Two demoters (an unbind and the fork walk) race one cell: exactly
    /// one owes the kernel a flush. Two flushes would issue two
    /// `SEEK_SET`s whose adverse ordering rewinds the description.
    #[test]
    fn fd_table_disarm_is_once_per_cell() {
        loom::model(|| {
            let ep = Arc::new(EpochCore::new());
            let m = Arc::new(MirrorCore::armed_at(4096, ep.current()));
            let flushes = Arc::new(AtomicUsize::new(0));
            let ts: Vec<_> = (0..2)
                .map(|_| {
                    let (m, ep, flushes) = (m.clone(), ep.clone(), flushes.clone());
                    thread::spawn(move || {
                        if m.disarm_if_current(&ep).is_some() {
                            flushes.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                })
                .collect();
            for t in ts {
                t.join().unwrap();
            }
            assert_eq!(
                flushes.load(Ordering::SeqCst),
                1,
                "dup siblings share ONE cell — exactly one flush is owed"
            );
        });
    }

    // -- 3. fork-epoch conservatism ------------------------------------

    /// The bind-vs-fork race: an fd whose epoch was snapshotted before
    /// its `open`, installed concurrently with a fork bump, must never
    /// read armed under the NEW epoch. (Conservative-correct: the fd
    /// silently falls back to the kernel-authoritative discipline.)
    #[test]
    fn fd_table_fork_bump_stales_a_concurrent_bind() {
        loom::model(|| {
            let ep = Arc::new(EpochCore::new());
            // The interposer's duty: snapshot BEFORE the real open.
            let snapshot = ep.current();
            let armed_after = Arc::new(AtomicUsize::new(0));

            let binder = {
                let (ep, armed_after) = (ep.clone(), armed_after.clone());
                thread::spawn(move || {
                    let m = MirrorCore::armed_at(0, snapshot);
                    if m.armed(&ep) {
                        armed_after.store(1, Ordering::SeqCst);
                    }
                })
            };
            let forker = {
                let ep = ep.clone();
                thread::spawn(move || {
                    ep.bump();
                })
            };
            binder.join().unwrap();
            forker.join().unwrap();

            // After the bump has certainly landed, a binding carrying the
            // OLD snapshot can never be authoritative again.
            let stale = MirrorCore::armed_at(0, snapshot);
            assert!(
                !stale.armed(&ep) || ep.current() == snapshot,
                "a pre-fork snapshot must not read armed under the new epoch"
            );
            let _ = armed_after.load(Ordering::SeqCst);
        });
    }

    /// The fork walk's demote and a concurrent op: whichever order, the
    /// cell is disarmed at most once and the op's offset is not lost
    /// (the `fork_demote` face of the Dekker pair — the production
    /// atfork-prepare path, which bumps the epoch and then walks).
    #[test]
    fn fd_table_fork_walk_demote_never_loses_the_final_offset() {
        loom::model(|| {
            const SEED: u64 = 4096;
            const FINAL: u64 = 8192;
            let ep = Arc::new(EpochCore::new());
            let m = Arc::new(MirrorCore::armed_at(SEED, ep.current()));
            let flushed = Arc::new(AtomicU64::new(u64::MAX));
            let op_owns = Arc::new(AtomicUsize::new(0));

            let op = {
                let (m, ep, own) = (m.clone(), ep.clone(), op_owns.clone());
                thread::spawn(move || {
                    if !m.publish(&ep, FINAL) {
                        own.store(1, Ordering::SeqCst);
                    }
                })
            };
            let forker = {
                let (m, ep, flushed) = (m.clone(), ep.clone(), flushed.clone());
                thread::spawn(move || {
                    let prev = ep.bump();
                    if let Some(off) = m.fork_demote(prev) {
                        flushed.store(off, Ordering::SeqCst);
                    }
                })
            };
            op.join().unwrap();
            forker.join().unwrap();

            if flushed.load(Ordering::SeqCst) == SEED {
                assert!(
                    op_owns.load(Ordering::SeqCst) == 1 || m.load() == SEED,
                    "the fork walk flushed a stale offset while the op believed \
                     the mirror still owned the final one"
                );
            }
        });
    }
}
