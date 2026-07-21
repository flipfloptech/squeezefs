//! Pure lock-free node lifecycle core (design §4.6): the
//! `clean → dirty → serializing → superseded` state machine that the node
//! cache (PR K5) drives under its per-node `RwLock`, extracted so the
//! atomic half of the protocol is dependency-free and exhaustively
//! model-checked.
//!
//! Self-contained (no crate dependencies) so the `loom-models` crate can
//! `#[path]`-include this file and check the freeze-swap / apply /
//! supersede / evict interleavings under `cfg(loom)`, exactly like
//! `alloc_ext_core.rs` and `journal_core.rs`. Nothing here touches a
//! device or a snapshot; the cache layer owns those.
//!
//! ## The four §4.6 states, as three bits
//!
//! | State | Encoding | Meaning |
//! |---|---|---|
//! | **clean** | `0` | No un-flushed records; the only evictable state (§4.5) |
//! | **dirty** | `DIRTY` | The open delta holds records not yet frozen into a bset |
//! | **serializing** | `FREEZING` (± `DIRTY`) | A writeback froze the delta and is writing it **outside** the lock (§4.6 pt 1); `DIRTY` re-set means commits landed in the fresh delta meanwhile |
//! | **superseded** | `SUPERSEDED` (terminal) | This in-RAM object no longer represents the node: an SMO swapped the cache mapping to successor node(s), or clock eviction severed a clean entry. In-flight readers keep their snapshot (§4.6); writers must lock-then-revalidate-then-retry through the cache |
//!
//! ## Why the atomics alone already forbid lost updates
//!
//! Every transition is one atomic RMW on the packed word, so the RMW total
//! order decides every race even before the node lock serializes callers:
//!
//! - **supersede vs apply**: [`NodeState::mark_dirty`] refuses once
//!   `SUPERSEDED` is set, and [`NodeState::supersede`] reports whether
//!   `DIRTY`/`FREEZING` were set at its instant. Whichever RMW lands first,
//!   an applied record is either visible to the SMO's successor build
//!   (`was_dirty == true`) or was never applied (`Err(Superseded)`, the
//!   §4.6 revalidation outcome) — never silently dropped.
//! - **evict vs apply**: [`NodeState::try_evict`] is a CAS from the exact
//!   `clean` word to `SUPERSEDED`, so it can never win against a node that
//!   just accepted dirt (§4.5 "dirty nodes are pinned until writeback"),
//!   and a writer that lost to eviction fails `mark_dirty` loud and
//!   retries through the cache.
//! - **freeze-swap vs apply**: [`NodeState::begin_freeze`] atomically
//!   clears `DIRTY` while setting `FREEZING`; a concurrent `mark_dirty`
//!   lands either in the frozen delta (before the swap) or re-sets `DIRTY`
//!   for the fresh delta (after it) — the §4.6 pt 1 snapshot-then-write
//!   discipline with no window in which dirt is unaccounted.
//!
//! The node-cache layer performs the delta/snapshot mutations these bits
//! describe under the per-node write lock (§4.4 pt 1); the loom models pin
//! the lock-free claims above plus the locked compositions.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU64, Ordering};
}

use atomic::{AtomicU64, Ordering};

/// The open delta holds records not yet frozen into a bset image.
const DIRTY: u64 = 1;
/// A writeback/SMO froze the delta and its image is in flight (§4.6 pt 1).
const FREEZING: u64 = 1 << 1;
/// Terminal: the object is severed from the cache (SMO swap or eviction).
const SUPERSEDED: u64 = 1 << 2;

/// The §4.6 lifecycle state, decoded from the packed word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// No un-flushed records; evictable (§4.5).
    Clean,
    /// Un-flushed records in the open delta; pinned in cache.
    Dirty,
    /// A frozen delta image is being written outside the lock (§4.6 pt 1);
    /// commits keep landing in a fresh open delta meanwhile.
    Serializing,
    /// Severed from the cache (SMO replacement or eviction) — writers must
    /// revalidate-and-retry; readers keep their snapshots.
    Superseded,
}

/// `mark_dirty` refused: the node was superseded first — the caller's
/// records were **not** accepted and it must re-resolve through the cache
/// (§4.6 lock-then-revalidate-then-retry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superseded;

/// `begin_freeze` refused, with the reason (protocol misuse is a bug in the
/// serialized writeback/SMO task, but the core stays panic-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeRefused {
    /// Nothing to freeze: the open delta is empty.
    NotDirty,
    /// A freeze is already in flight — one at a time (the §4.6 serialized
    /// writeback/SMO task never overlaps freezes; a second caller is a bug).
    AlreadyFreezing,
    /// The node is superseded; there is nothing left to write back.
    Superseded,
}

/// What [`NodeState::supersede`] displaced — the SMO must carry both into
/// its successor build: `was_dirty` ⇒ the open delta needs the §4.6
/// "bounded second merge"; `was_freezing` ⇒ a frozen image is still in
/// flight (impossible on the serialized task, reported for the models).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupersedeOutcome {
    pub was_dirty: bool,
    pub was_freezing: bool,
}

/// The packed lock-free lifecycle word. See the module docs for the
/// protocol; `loom-models/` pins the invariants.
pub struct NodeState {
    word: AtomicU64,
}

impl Default for NodeState {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeState {
    /// A fresh `clean` node.
    pub fn new() -> Self {
        Self {
            word: AtomicU64::new(0),
        }
    }

    /// Decode the current lifecycle state (`SUPERSEDED` dominates,
    /// `FREEZING` before `DIRTY` — a serializing node with re-accumulated
    /// dirt is still *serializing*).
    pub fn state(&self) -> LifecycleState {
        let w = self.word.load(Ordering::Acquire);
        if w & SUPERSEDED != 0 {
            LifecycleState::Superseded
        } else if w & FREEZING != 0 {
            LifecycleState::Serializing
        } else if w & DIRTY != 0 {
            LifecycleState::Dirty
        } else {
            LifecycleState::Clean
        }
    }

    /// Whether the open delta holds un-flushed records.
    pub fn is_dirty(&self) -> bool {
        self.word.load(Ordering::Acquire) & DIRTY != 0
    }

    /// Whether a frozen delta image is in flight (§4.6 pt 1).
    pub fn is_freezing(&self) -> bool {
        self.word.load(Ordering::Acquire) & FREEZING != 0
    }

    /// Whether the object is severed from the cache — the §4.6
    /// revalidation predicate (checked under the node lock by writers).
    pub fn is_superseded(&self) -> bool {
        self.word.load(Ordering::Acquire) & SUPERSEDED != 0
    }

    /// A commit applied records to the open delta: set `DIRTY` — unless
    /// the node was superseded first, in which case **nothing was
    /// accepted** and the caller retries through the cache (§4.6). One
    /// atomic RMW: the superseded check and the dirty set cannot be split
    /// by a racing [`Self::supersede`] / [`Self::try_evict`].
    pub fn mark_dirty(&self) -> Result<(), Superseded> {
        let prev = self
            .word
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                if w & SUPERSEDED != 0 {
                    None
                } else {
                    Some(w | DIRTY)
                }
            });
        match prev {
            Ok(_) => Ok(()),
            Err(_) => Err(Superseded),
        }
    }

    /// Begin the §4.6 pt 1 freeze-swap: atomically clear `DIRTY` and set
    /// `FREEZING`. The caller (holding the node write lock) swaps the open
    /// delta out as the frozen image, then releases the lock and writes it
    /// — never I/O under the lock. Concurrent `mark_dirty` calls after the
    /// swap re-set `DIRTY` for the fresh delta.
    pub fn begin_freeze(&self) -> Result<(), FreezeRefused> {
        let mut w = self.word.load(Ordering::Acquire);
        loop {
            if w & SUPERSEDED != 0 {
                return Err(FreezeRefused::Superseded);
            }
            if w & FREEZING != 0 {
                return Err(FreezeRefused::AlreadyFreezing);
            }
            if w & DIRTY == 0 {
                return Err(FreezeRefused::NotDirty);
            }
            match self.word.compare_exchange_weak(
                w,
                (w & !DIRTY) | FREEZING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(cur) => w = cur,
            }
        }
    }

    /// PR VL7 (§5.7 D4 — the forced-compaction nudge): enter `FREEZING`
    /// from a node with **no open delta to swap** (clean, or dirty with
    /// an already-empty overlay). The SMO's supersede/`end_freeze`
    /// bookkeeping then sees exactly the state an ordinary freeze-borne
    /// source carries — with an EMPTY frozen delta by construction.
    /// Same serialization owner as [`Self::begin_freeze`] (the per-volume
    /// SMO task, under the node write lock); refuses on `SUPERSEDED`
    /// and on an in-flight freeze.
    pub fn begin_forced_freeze(&self) -> Result<(), FreezeRefused> {
        let mut w = self.word.load(Ordering::Acquire);
        loop {
            if w & SUPERSEDED != 0 {
                return Err(FreezeRefused::Superseded);
            }
            if w & FREEZING != 0 {
                return Err(FreezeRefused::AlreadyFreezing);
            }
            match self.word.compare_exchange_weak(
                w,
                (w & !DIRTY) | FREEZING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(cur) => w = cur,
            }
        }
    }

    /// The frozen image reached its destination (append committed, or the
    /// SMO consumed it): clear `FREEZING`. Returns whether `DIRTY`
    /// re-accumulated during the write — the writeback task's re-enqueue
    /// signal. Callable on a superseded node (the SMO tidies up after the
    /// swap); the terminal bit is untouched.
    pub fn end_freeze(&self) -> bool {
        let prev = self.word.fetch_and(!FREEZING, Ordering::AcqRel);
        debug_assert!(prev & FREEZING != 0, "end_freeze without begin_freeze");
        prev & DIRTY != 0
    }

    /// The frozen image could not be written and the caller restored it to
    /// the open delta: clear `FREEZING`, re-set `DIRTY` — the records are
    /// accounted dirty again and a later cycle retries.
    pub fn abort_freeze(&self) {
        let prev = self
            .word
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                Some((w & !FREEZING) | DIRTY)
            });
        debug_assert!(
            prev.unwrap_or(0) & FREEZING != 0,
            "abort_freeze without begin_freeze"
        );
    }

    /// Terminal §4.6 transition: the SMO swapped the cache mapping to the
    /// successor node(s). Reports what was displaced at the RMW instant —
    /// `was_dirty` open-delta records the successor build must carry
    /// (the "bounded second merge"), `was_freezing` a frozen image still
    /// in flight. Errors if already superseded (double-swap is an SMO
    /// serialization bug, surfaced loud but panic-free).
    pub fn supersede(&self) -> Result<SupersedeOutcome, Superseded> {
        let prev = self.word.fetch_or(SUPERSEDED, Ordering::AcqRel);
        if prev & SUPERSEDED != 0 {
            return Err(Superseded);
        }
        Ok(SupersedeOutcome {
            was_dirty: prev & DIRTY != 0,
            was_freezing: prev & FREEZING != 0,
        })
    }

    /// Clock eviction's gate (§4.5): sever the object **only** from the
    /// exact `clean` state — one CAS `0 → SUPERSEDED`, so eviction can
    /// never win against a node that just accepted dirt (dirty pinning) or
    /// holds an in-flight freeze, and a writer that lost the race fails
    /// [`Self::mark_dirty`] loud and re-resolves through the cache.
    pub fn try_evict(&self) -> bool {
        self.word
            .compare_exchange(0, SUPERSEDED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}
