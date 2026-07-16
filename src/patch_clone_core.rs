//! W1 sole-owner patch × clone pin — the composed two-word fence protocol
//! (docs/design-random-small-writes.md §5.1, review Issues 1/15/16).
//!
//! The in-place patch is the first mutation of a mapped striped block: the
//! clone pin model ([`crate::refcount_core`]) was built on post-map
//! immutability, so the two word protocols must be composed:
//!
//! * **patch**: retire the block's incarnation word
//!   ([`crate::incarnation_core::retire`] — store W), [`cross_word_fence`],
//!   then re-read the refcount ([`crate::refcount_core::peek`] — load R);
//!   proceed in place only on `refcount == 1`, else `publish` re-stabilizes
//!   (content unchanged) and the write falls back to CoW.
//! * **clone**: pin ([`crate::refcount_core::try_acquire`] — store R),
//!   [`cross_word_fence`], then snapshot the incarnation word
//!   ([`crate::incarnation_core::snapshot`] — load W); an unstable word
//!   means a patch may be mid-flight — unpin, refetch the map, retry
//!   (bounded by the existing `attempt >= 3` loud refusal).
//!
//! **Why `SeqCst` fences (Issue 15 — the store-buffering closure)**: each
//! side is a store on one word followed by a load of the *other* word.
//! Release/Acquire (including `retire`'s trailing `Release` fence and
//! `snapshot`'s `Acquire` load) does not forbid the classic SB outcome —
//! both sides reading the old value (patch sees refcount 1 AND clone sees
//! stable) on ARM/POWER; x86/TSO masks it. With both sides interposing a
//! `SeqCst` fence between their store and their cross-word load, the fence
//! total order guarantees at least one side observes the other: the patch
//! backs off (refcount 2) or the clone retries (unstable) — never both
//! proceeding. Model-checked as a **composed two-word loom model**
//! (`loom-models`, incarnation_core × refcount_core in ONE model, both
//! interleaving orders, asserting ¬(pin-validated ∧ patch-proceeded)) —
//! per-protocol models structurally cannot see SB across protocols.
//!
//! Self-contained (no crate dependencies beyond the sibling cores) so
//! `loom-models/` can `#[path]`-include it and check the exact shipped
//! fence. The main build never sets `cfg(loom)`.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{fence, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{fence, Ordering};
}

use atomic::{fence, Ordering};

/// The cross-word `SeqCst` fence both §5.1 sides interpose between their
/// store and their cross-word load:
///
/// * patch — between `mark_incarnation_unstable` (retire) and the
///   refcount re-read (`BlockAllocator::begin_patch_sole_owner`);
/// * clone — between the pin CAS (`increment_refcount` success) and the
///   incarnation snapshot (`BlockAllocator::pin_block_validated`; placed
///   before the word *lookup* so the absent-word arm — an offset first
///   patched after remount — is fenced too).
pub fn cross_word_fence() {
    fence(Ordering::SeqCst);
}
