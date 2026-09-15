//! The per-block **SHARED mark** — the third word of the W1 patch × clone
//! composition (docs/design-symmetric-metadata.md §5.4.3 / §5.4.4, PR 7).
//!
//! Under the slot-tree forest a block's references live in ONE slot tree
//! unless a clone shares it across two (§5.4.3 law 1). The clone protocol
//! (§5.4.4) makes that sharing visible BEFORE the cloner holds a pin:
//! step 1 (`MarkShared`) sets the mark at the source's holder, step 3
//! publishes the cloner's own reference. Between the two the block's
//! refcount still reads 1, so the two readers that act on "sole owner" —
//! the W1 in-place patch and the terminal-free decision — read the mark
//! too and stand down on it:
//!
//! * **patch** (`BlockAllocator::begin_patch_sole_owner`): retire the
//!   incarnation word, `patch_clone_core::cross_word_fence`, then
//!   `refcount == 1 && !is_shared` — a marked block is never patched in
//!   place (`patch_ineligible_shared`), whatever its count reads.
//! * **owner free** (`BackendRouter::free_block_verdict`): the mark is
//!   read FIRST (an Acquire load, before the release) — a marked block's
//!   verdict is never taken locally whatever its count reads: the shared
//!   index's home decides (`ReleaseShared`); an unmarked block takes the
//!   shipped local verdict.
//! * **cloner** (the design's order, §5.4.4): [`mark`](crate::shared_ref_core::mark)
//!   (a store), the fence, then the pin (`refcount_core::try_acquire`),
//!   the fence, the incarnation snapshot. The PR-7 PRODUCT clone pins
//!   FIRST (`pin_block_validated`) and marks after — durable at the
//!   holder, then this word — so its composition with the patcher rests
//!   on the two-word §5.1 law plus pin-before-publish (a pin that
//!   succeeded made every concurrent release nonterminal); the mark is
//!   load-bearing where a cloner's pin is NOT in this process's RAM count
//!   — the cross-process case PR 12 lands.
//!
//! **The load-bearing ordering** is the `SeqCst` fence pair the §5.1
//! model already proves (`patch_clone_core`): each side stores one word
//! then loads the others, and the fences forbid the store-buffering
//! outcome in which the patcher reads `refcount 1 ∧ unshared` while the
//! cloner validates a pre-patch snapshot. The mark adds a third loaded
//! word on the patcher's side and a third stored word on the cloner's,
//! both on the far side of their existing fences — so the DESIGN's
//! composed protocol is one model (`loom-models::shared_ref_core_*`,
//! whose doc states what it does and does not model), and weakening the
//! patcher's fence to Release fails it exactly as it fails the two-word
//! one.
//!
//! Self-contained (no crate dependencies) so `loom-models/` can
//! `#[path]`-include it and check the exact shipped word ops.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU32, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU32, Ordering};
}

use atomic::{AtomicU32, Ordering};

/// The mark word's UNSHARED value (a fresh word; an absent word reads the
/// same — a block nobody ever cloned).
pub const UNSHARED: u32 = 0;
/// The mark word's SHARED value.
pub const SHARED: u32 = 1;

/// Set the mark (§5.4.4 step 1 at the source's holder; step 3's inherited
/// bit at the cloner). `Release`: the store is ordered before the
/// caller's fence, which is what the patcher's and freer's fenced loads
/// synchronize with.
pub fn mark(word: &AtomicU32) {
    word.store(SHARED, Ordering::Release);
}

/// Clear the mark — the holder's verdict that no clone remains (the
/// index's population reached 0 and the block frees, or every cloner's
/// reference is gone and the source is sole again).
pub fn clear(word: &AtomicU32) {
    word.store(UNSHARED, Ordering::Release);
}

/// Read the mark. Callers on the "sole owner" decision paths interpose
/// `patch_clone_core::cross_word_fence` between their own store
/// (the incarnation retire, the refcount release) and this load.
pub fn is_shared(word: &AtomicU32) -> bool {
    word.load(Ordering::Acquire) == SHARED
}

/// A fresh, unshared word.
pub fn new_word() -> AtomicU32 {
    AtomicU32::new(UNSHARED)
}
