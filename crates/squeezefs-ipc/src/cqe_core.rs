//! IPC **completion doorbell** protocol core (the 2026-07-28 op-economy
//! campaign, lever 2 — completion-side wake economy).
//!
//! One [`CqeDoorbell`] per session, living in the shared session header
//! (line 2 — its own cache line, away from the submit doorbell). It is
//! the completion-direction mirror of the submit direction's
//! `doorbell + daemon_parked` pair:
//!
//! - **Daemon** (any completing thread — service thread fast path, tokio
//!   handler lane, direct-drive reaper): after the slot's DONE publish
//!   ([`SlotCore::complete`]), call [`CqeDoorbell::complete`] — bump the
//!   session completion sequence, then wake the seq word **only when a
//!   reaper is parked**. An unparked-reaper completion stream costs zero
//!   wake syscalls (the pre-campaign per-completion `FUTEX_WAKE` toward
//!   parked per-slot WAITERs was the measured collect-and-wake
//!   serialization term past ~525 k IOPS).
//! - **Client reaper** (the libaio merge loop): to park, call
//!   [`CqeDoorbell::park_begin`] (register parked intent AND snapshot
//!   the expected seq, in that order), then **re-scan the pending set**
//!   (the disarm→scan law: a completion that landed before the intent
//!   was visible is found by the scan), then `FUTEX_WAIT` on
//!   [`CqeDoorbell::seq_word`] against the snapshot. On return (wake,
//!   EAGAIN, timeout) call [`CqeDoorbell::park_end`].
//!
//! ## Why no wake is ever lost
//!
//! All four accesses are `SeqCst`, so they form a single total order
//! with the slot's DONE publication. A strand would need the reaper to
//! (a) miss the DONE in its post-`park_begin` scan, (b) pass the futex
//! admission (seq unchanged), while the daemon (c) read `parked == 0`
//! (no wake). (a) requires the scan to precede the DONE publish; the
//! scan follows `park_begin`'s RMW, so the daemon's later
//! `parked.load` — which follows its own DONE publish AND seq bump —
//! observes the registration and wakes: (c) is impossible. If instead
//! the seq bump precedes the snapshot, the DONE publish (sequenced
//! before the bump) is visible to the scan: (a) is impossible. And a
//! bump between snapshot and wait fails the futex admission: (b) is
//! impossible. The `ipc_cqe_doorbell_*` loom models in `loom-models`
//! check the shipped code; removing either Dekker `fence(SeqCst)`,
//! weakening the daemon's `parked` load to `Relaxed`, or reordering
//! `park_begin`'s two accesses each fails them (verified 2026-07-28).
//!
//! ## Trust boundary (§5.3.1)
//!
//! Both words are client-writable shm. A hostile client scribbling
//! `parked` to a huge value makes the daemon pay one wake syscall per
//! completion — exactly the pre-campaign posture, bounded, self-harm
//! only. Scribbling `seq` confuses only its own reaper (its parks stop
//! admitting or expire on their §5.3.1-rule-5 bounds). The daemon never
//! waits on either word — it only bumps/loads.
//!
//! Dependency-free on purpose: `loom-models/src/lib.rs` `#[path]`-includes
//! this file (the `slot_core` house convention), so the models check the
//! shipped protocol, not a copy. The main build never sets `cfg(loom)`.
//!
//! [`SlotCore::complete`]: crate::slot_core::SlotCore::complete

#[cfg(loom)]
use loom::sync::atomic::{fence, AtomicU32, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{fence, AtomicU32, Ordering};

/// The two completion-direction wake words. `#[repr(C)]` because
/// production embeds this at a fixed offset inside the session header
/// (KD-7 version-locked, never a stable ABI).
#[repr(C)]
#[derive(Debug)]
pub struct CqeDoorbell {
    /// Session completion sequence: bumped once per completion. The
    /// futex word parked reapers sleep on.
    seq: AtomicU32,
    /// Parked-reaper count: `park_begin` increments, `park_end`
    /// decrements. Nonzero gates the daemon's wake syscall.
    parked: AtomicU32,
}

impl Default for CqeDoorbell {
    fn default() -> Self {
        Self::new()
    }
}

impl CqeDoorbell {
    pub fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            parked: AtomicU32::new(0),
        }
    }

    /// Daemon, after the slot's DONE publish: bump the completion seq;
    /// returns `true` when a parked reaper needs a `FUTEX_WAKE` on
    /// [`Self::seq_word`] (breadth `i32::MAX` — split submitter/reaper
    /// pairs may both park). `false` = elide the syscall (no one is
    /// parked; a concurrent parker's post-registration scan or failed
    /// admission covers it — module docs).
    pub fn complete(&self) -> bool {
        // Store→load across two locations (the Dekker shape): the
        // explicit fence is LOAD-BEARING, not belt-and-braces — it is
        // what makes this side's bump globally ordered against the
        // reaper's registration (the W1 §5.1 house pattern; loom's
        // SeqCst-atomics modeling is weaker than the C++ total order,
        // and the `ipc_cqe_*` models fail without it).
        self.seq.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        self.parked.load(Ordering::SeqCst) != 0
    }

    /// Client reaper: register parked intent and snapshot the expected
    /// seq — in that order (registration first is load-bearing: the
    /// daemon's `parked` gate must observe intent no later than the
    /// snapshot the wait admits against). Follow with the pending-set
    /// re-scan, then `FUTEX_WAIT(seq_word, returned snapshot)`.
    pub fn park_begin(&self) -> u32 {
        // The mirrored Dekker side — same load-bearing fence (see
        // [`Self::complete`]).
        self.parked.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        self.seq.load(Ordering::SeqCst)
    }

    /// Client reaper: deregister after the wait returns (any reason).
    pub fn park_end(&self) {
        self.parked.fetch_sub(1, Ordering::SeqCst);
    }

    /// The completion-seq futex word (cross-process — never
    /// `FUTEX_PRIVATE`).
    pub fn seq_word(&self) -> &AtomicU32 {
        &self.seq
    }

    /// Current completion seq (diagnostics / model assertions).
    pub fn seq(&self) -> u32 {
        self.seq.load(Ordering::SeqCst)
    }

    /// Current parked-reaper count (diagnostics / model assertions).
    pub fn parked(&self) -> u32 {
        self.parked.load(Ordering::SeqCst)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Sequential protocol walk: completions toward an unparked reaper
    /// elide; a parked reaper gates the wake on; park_end releases it.
    #[test]
    fn wake_gated_on_parked_count() {
        let d = CqeDoorbell::new();
        assert!(!d.complete(), "no reaper parked: elide the wake");
        assert!(!d.complete(), "still elided");
        let expected = d.park_begin();
        assert_eq!(expected, 2, "snapshot reads the completions so far");
        assert!(d.complete(), "parked reaper must be woken");
        assert_ne!(d.seq(), expected, "the bump fails the wait's admission");
        d.park_end();
        assert!(!d.complete(), "after park_end the wake elides again");
    }

    /// Two parkers (split submitter/reaper — a legal libaio shape): the
    /// wake stays on until BOTH deregister.
    #[test]
    fn wake_covers_every_parker() {
        let d = CqeDoorbell::new();
        let _e1 = d.park_begin();
        let _e2 = d.park_begin();
        assert_eq!(d.parked(), 2);
        assert!(d.complete());
        d.park_end();
        assert!(d.complete(), "second parker still needs the wake");
        d.park_end();
        assert!(!d.complete());
    }
}
