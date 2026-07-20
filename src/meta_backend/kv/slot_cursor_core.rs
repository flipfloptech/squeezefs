//! Per-slot guest ino-cursor core (PR VL5b, design-volume-lifecycle
//! §5.5.2 / KD-7: "per-slot monotonic ino allocation cursors ride the
//! A/B root ledger").
//!
//! One cell per hosted guest slot: minting draws local inos with a
//! `fetch_add` (never reused — the §4.8 monotonic discipline, per slot);
//! the checkpoint task **publishes** a snapshot into the ledger record
//! while minting continues (the latch-free-reader vs publisher edge the
//! PR-plan loom clause names). The soundness argument mirrors the
//! volume-level `next_ino` watermark:
//!
//! - a mint whose RECORD reached the RAM tree before the checkpoint's
//!   flush pass synchronizes-with the snapshot load (the flush takes the
//!   node locks the apply held), so `snapshot > minted_ino` — the ledger
//!   never under-declares an ino whose record it covers;
//! - a mint whose record commits after the snapshot rides the journal
//!   (seq ≥ tail) and is recovered by replay's per-slot max fold;
//! - a mint that never commits is burned harmlessly ("a failed create
//!   burns the ino").
//!
//! Dependency-free so `loom-models/` can `#[path]`-include it and
//! exhaustively check the mint/publish/install interleavings against
//! the invariants above. The main build never sets `cfg(loom)`.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU64, Ordering};
}

use atomic::{AtomicU64, Ordering};

/// One guest slot's monotonic local-ino cursor.
#[derive(Debug)]
pub struct SlotCursor {
    next: AtomicU64,
}

impl SlotCursor {
    /// A fresh cursor starting at `next` (2 for a virgin slot — locals
    /// below 2 are reserved, exactly like the volume watermark).
    pub fn new(next: u64) -> Self {
        Self {
            next: AtomicU64::new(next.max(2)),
        }
    }

    /// Mint one local ino: `fetch_add`, no reuse, no free-on-failure.
    pub fn mint(&self) -> u64 {
        self.next.fetch_add(1, Ordering::AcqRel)
    }

    /// The publication snapshot the checkpoint task writes into the
    /// ledger record: every mint whose record-apply happened-before this
    /// load is strictly below the returned value.
    pub fn snapshot(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }

    /// Raise the cursor to at least `floor` (mount recovery folds the
    /// ledger snapshot and the replayed per-slot maxima; migration
    /// installs the source slot's travelling cursor). Monotonic — a
    /// stale floor can never regress a fresher mint.
    pub fn install_floor(&self, floor: u64) {
        self.next.fetch_max(floor.max(2), Ordering::AcqRel);
    }
}
