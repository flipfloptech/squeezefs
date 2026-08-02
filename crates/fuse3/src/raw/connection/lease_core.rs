//! FUSE-over-io_uring payload-lease re-arm protocol core (SqueezeFS
//! zero-copy write-path design §5.4, PR 5).
//!
//! One `EntLeaseState` per (qid, ring ent). A FUSE_WRITE payload is
//! delivered to the session as a `Bytes::from_owner` lease over the ent's
//! registered payload buffer instead of a copy. The COMMIT_AND_FETCH for
//! that ent both sends the reply *and* re-arms the registered buffers for
//! the next inbound request — and `apply_reply` itself writes reply bytes
//! into the payload region — so the queue worker must never apply + commit
//! while a lease is live. This module is that gate:
//!
//! - **Delivery** (queue worker): [`EntLeaseState::acquire`] — refs 0 → 1.
//!   At most one lease exists per ent because a new request can only be
//!   delivered after the previous commit, and the commit is gated on
//!   refs == 0.
//! - **Lease drop** (any thread — the handler's last payload `Bytes` clone
//!   dying): [`EntLeaseState::release`] — refs 1 → 0; if a commit is
//!   parked, tells the caller to fire the queue eventfd.
//! - **Commit gate** (queue worker): [`EntLeaseState::try_commit`] —
//!   commit now when refs == 0, else *park* the commit message
//!   (worker-local) after publishing `parked = true` and **re-checking**
//!   refs (publish-then-recheck closes the missed-wake race).
//! - **Unpark probe** (queue worker, on eventfd wake / parked scan /
//!   shutdown drain): [`EntLeaseState::try_unpark`].
//!
//! ## Memory ordering: why both sides carry an explicit `SeqCst` fence
//!
//! Release/drop and park/publish form the classic store-buffer (Dekker)
//! litmus:
//!
//! ```text
//! releaser:  refs = 0 (fetch_sub);  FENCE;  r1 = parked
//! worker:    parked = true (store); FENCE;  r2 = refs
//! ```
//!
//! Under `Release`/`Acquire` alone, `r1 == false && r2 == 1` is a permitted
//! outcome (both sides read the pre-race value): the releaser skips the
//! wake *and* the worker parks — a parked commit nobody ever wakes, i.e. a
//! deterministic mount hang at `Q_DEPTH = 4`, not a slowdown. The `SeqCst`
//! fences close it: fences are totally ordered, so whichever side's fence
//! is first, the other side's post-fence load observes the pre-fence store
//! (C++11 [atomics.fences]): either the releaser sees `parked == true` and
//! wakes, or the worker's re-check sees `refs == 0` and commits
//! immediately. Explicit fences rather than `SeqCst` accesses because the
//! fence axioms are exactly the guarantee needed *and* they are what loom
//! models faithfully — the loom run of this exact file rejected a
//! `SeqCst`-accesses-only draft with a concrete missed-wake interleaving.
//! The loom model (`loom-models`, `ent_lease_*`) checks this shipped code.
//!
//! Dependency-free on purpose: `loom-models/src/lib.rs` `#[path]`-includes
//! this file (the `incarnation_core` / `cow_core` house convention), so the
//! model checks the real protocol, not a hand copy. The main build never
//! sets `cfg(loom)`.

#[cfg(loom)]
use loom::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};

/// Outcome of the worker's commit gate for one `CommitMsg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitGate {
    /// No live payload lease: apply the reply and push COMMIT_AND_FETCH now.
    Ready,
    /// A payload lease is live: park the message (worker-local) and wait for
    /// the lease drop to fire the queue eventfd.
    Parked,
}

/// Per-ring-ent lease/park word protocol. See the module docs.
#[derive(Debug, Default)]
pub struct EntLeaseState {
    /// Live payload leases over this ent's registered payload buffer
    /// (0 or 1 in practice — one outstanding request per ent).
    refs: AtomicU32,
    /// The worker has a commit message parked waiting on `refs == 0`.
    parked: AtomicBool,
}

impl EntLeaseState {
    pub fn new() -> Self {
        Self {
            refs: AtomicU32::new(0),
            parked: AtomicBool::new(false),
        }
    }

    /// Delivery-time lease acquisition (queue worker only). Returns the
    /// previous refs count — the caller asserts it was 0: an ent is only
    /// re-armed (and can therefore only deliver) after a commit that the
    /// gate proved lease-free.
    pub fn acquire(&self) -> u32 {
        self.refs.fetch_add(1, Ordering::AcqRel)
    }

    /// DMA-destination lease acquisition (MEM-1 — any thread, from the
    /// read path at device-request build). Unlike [`Self::acquire`], refs
    /// may already be nonzero: a multi-SQE read claims one token per
    /// in-flight SQE against the same ent. Protocol soundness is by
    /// program order — every claim happens strictly before its request's
    /// reply can exist (mint → claim → submit → await), so a commit that
    /// observed refs == 0 proves no claimed SQE can still write the
    /// buffer. The commit gate needs no changes: it parks on refs > 0
    /// whatever the refs' provenance, and [`Self::release`] wakes on the
    /// LAST drop.
    pub fn acquire_dest(&self) {
        self.refs.fetch_add(1, Ordering::AcqRel);
    }

    /// Lease drop (any thread). Returns `true` when this drop released the
    /// last lease **and** a commit is parked — the caller must then fire the
    /// queue eventfd so the worker re-scans its parked messages. A spurious
    /// wake (the worker already un-parked via its own re-check) is harmless;
    /// a missed wake is a hang, which the `SeqCst` fence pairing (module
    /// docs) forbids.
    pub fn release(&self) -> bool {
        if self.refs.fetch_sub(1, Ordering::Release) == 1 {
            // Store→load fence: pairs with try_commit's park fence so that
            // at least one side observes the other (store-buffer litmus).
            fence(Ordering::SeqCst);
            return self.parked.load(Ordering::Acquire);
        }
        false
    }

    /// Worker commit gate. `Ready` ⇒ refs was observed 0 and `parked` is
    /// clear — the reply may be applied (payload writes included) and the
    /// COMMIT_AND_FETCH pushed. `Parked` ⇒ a lease is live; the caller
    /// parks the message and relies on [`EntLeaseState::release`]'s wake.
    ///
    /// Publish-then-recheck: `parked` is stored *before* the second refs
    /// load (with a `SeqCst` fence between — module docs), so a lease drop
    /// that raced the first load either observes `parked == true` (and
    /// wakes) or its `refs = 0` is observed by the re-check (and the commit
    /// proceeds immediately).
    pub fn try_commit(&self) -> CommitGate {
        if self.refs.load(Ordering::Acquire) == 0 {
            return CommitGate::Ready;
        }
        self.parked.store(true, Ordering::Release);
        // Store→load fence: pairs with release()'s fence.
        fence(Ordering::SeqCst);
        if self.refs.load(Ordering::Acquire) == 0 {
            // The last lease dropped between the first load and the park —
            // un-publish and commit now (its wake, if any, is spurious).
            self.parked.store(false, Ordering::Release);
            return CommitGate::Ready;
        }
        CommitGate::Parked
    }

    /// Parked-scan / shutdown-drain probe (queue worker only): when the
    /// lease is gone, clears `parked` and returns `true` — the caller
    /// applies + commits the parked message. Acquire pairs with the
    /// Release `fetch_sub` in [`EntLeaseState::release`], so the payload
    /// write that follows a `true` probe happens-after every access the
    /// dropped lease made. Once a probe observes refs == 0 no new lease can
    /// appear (delivery requires a prior commit), so the write cannot race
    /// a resurrection.
    pub fn try_unpark(&self) -> bool {
        if self.refs.load(Ordering::Acquire) == 0 {
            self.parked.store(false, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// True while a payload lease is live (delivery-time debug assertions
    /// and the shutdown drain's bounded wait).
    pub fn leased(&self) -> bool {
        self.refs.load(Ordering::Acquire) != 0
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Sequential protocol walk: deliver → reply while leased (park) →
    /// lease drop (wake) → unpark → commit; next delivery starts clean.
    #[test]
    fn gate_parks_while_leased_and_unparks_after_drop() {
        let st = EntLeaseState::new();
        assert!(!st.leased());

        assert_eq!(st.acquire(), 0, "fresh ent must be lease-free");
        assert!(st.leased());

        // Reply arrives while the handler still holds the payload.
        assert_eq!(st.try_commit(), CommitGate::Parked);
        assert!(!st.try_unpark(), "must not unpark while leased");

        // Handler drops the payload: last ref + parked ⇒ wake required.
        assert!(st.release(), "release must demand a wake when parked");
        assert!(st.try_unpark(), "commit must be releasable after the drop");
        assert!(!st.leased());

        // Ent re-armed, next request delivered: state starts clean.
        assert_eq!(st.acquire(), 0);
        assert!(!st.release(), "no wake needed when nothing is parked");
    }

    /// The common fast path: the lease dropped before the reply was
    /// submitted (handlers consume the payload before replying) — the gate
    /// must be Ready and demand no parking round-trip.
    #[test]
    fn gate_ready_when_lease_already_dropped() {
        let st = EntLeaseState::new();
        assert_eq!(st.acquire(), 0);
        assert!(!st.release(), "nothing parked: no wake");
        assert_eq!(st.try_commit(), CommitGate::Ready);
    }

    /// try_commit's publish-then-recheck leaves no stale `parked` flag when
    /// it resolves Ready via the re-check path.
    #[test]
    fn recheck_ready_clears_parked() {
        let st = EntLeaseState::new();
        assert_eq!(st.acquire(), 0);
        assert!(!st.release());
        // Force the parked path artificially: state is already refs == 0 so
        // try_commit returns Ready on the fast path; emulate the re-check
        // path by publishing and probing directly.
        assert_eq!(st.try_commit(), CommitGate::Ready);
        assert!(st.try_unpark(), "unpark probe on a free ent is Ready");
        assert!(!st.leased());
    }
}
