//! Per-slot cutover-gate core (PR VL5b, design-volume-lifecycle
//! §5.5.2a): the admission word a mutating routed-meta op checks at op
//! entry **before any 4a `lock_many`**, and the closer/drain protocol
//! the cutover task runs against it.
//!
//! The load-bearing race is the classic store-load window: the cutover
//! task closes the gate and reads the in-flight count; a mutator
//! increments the count and reads the gate. Both sides pair their
//! write with an explicit **`fence(SeqCst)`** before their read (the
//! house Dekker idiom — the same shape as `patch_clone_core`'s §5.1
//! protocol, and the form loom models exactly; loom under-models
//! per-access `SeqCst` orderings), so at least one side observes the
//! other:
//!
//! - the mutator's increment is visible to the closer's drain read ⇒
//!   the drain waits for it (the op is an "in-flight guard-holder" and
//!   drains to terminal outcome through the conveyor);
//! - otherwise the mutator's gate load observes CLOSED ⇒ it backs out
//!   (decrement) and parks holding **zero** DLM/node locks.
//!
//! There is no third interleaving in which an admitted op escapes the
//! drain — the exact invariant the cutover's "final delta then flip"
//! sequencing rests on. Dependency-free so `loom-models/` can
//! `#[path]`-include it and exhaustively check the enter/close/drain
//! interleavings (the fences are load-bearing and verified by
//! weakening). The main build never sets `cfg(loom)`.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};
}

use atomic::{fence, AtomicBool, AtomicU64, Ordering};

/// One migrating slot's admission gate + in-flight census.
#[derive(Debug)]
pub struct SlotGate {
    closed: AtomicBool,
    inflight: AtomicU64,
}

impl Default for SlotGate {
    fn default() -> Self {
        Self::new()
    }
}

impl SlotGate {
    pub fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            inflight: AtomicU64::new(0),
        }
    }

    /// Mutator admission: increment-then-check (module docs). `true` =
    /// admitted (the caller MUST pair with [`Self::exit`]); `false` =
    /// the gate is closed and the increment was backed out — park (with
    /// nothing held) and retry.
    pub fn try_enter(&self) -> bool {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        // Dekker fence #1: publish the increment before probing the
        // gate (pairs with `close`'s fence — module docs).
        fence(Ordering::SeqCst);
        if self.closed.load(Ordering::Acquire) {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    /// Late join for an op that discovered a new ino MID-FLIGHT (a
    /// rename leg found under its locks): it must never park (it holds
    /// guards), but the drain must wait for it — unconditional entry.
    pub fn join(&self) {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        // Same publication fence as `try_enter` — the drain must see
        // the join even though joiners never probe the gate.
        fence(Ordering::SeqCst);
    }

    /// Terminal outcome reached: leave the census.
    pub fn exit(&self) {
        let prev = self.inflight.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "slot-gate exit without enter");
    }

    /// Cutover: stop admission. Idempotent. The Dekker fence #2 pairs
    /// with `try_enter`'s (module docs): the store is published before
    /// any subsequent [`Self::drained`] read.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        fence(Ordering::SeqCst);
    }

    /// Cutover abort-and-retry / completion: re-admit parked ops.
    pub fn reopen(&self) {
        self.closed.store(false, Ordering::Release);
    }

    /// Whether the gate is currently closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Drain probe — meaningful only while closed: `true` = every
    /// admitted op has reached its terminal outcome, and (by the fence
    /// pairing in the module docs) every future `try_enter` observes
    /// the closed gate.
    pub fn drained(&self) -> bool {
        debug_assert!(self.is_closed(), "drain probe on an open gate");
        fence(Ordering::SeqCst);
        self.inflight.load(Ordering::Acquire) == 0
    }
}
