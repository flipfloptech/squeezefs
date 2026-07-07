//! Block-key incarnation seqlock — word-level protocol core.
//!
//! One `AtomicU64` per block offset encodes `gen << 1 | stable`. Writers
//! *retire* the word (gen+1, stable=0) before mutating device data under a
//! (possibly reused) block key, and *publish* (stable=1) only after the
//! durable device write. Cache fills snapshot the word before their device
//! read and publish into shared caches only if the word was stable and is
//! unchanged afterwards — a fill can never poison caches with bytes from a
//! dead incarnation of a reused key (the ABA fixed in the striped
//! concurrent-write work).
//!
//! Self-contained (no crate dependencies) so `loom-models/` can
//! `#[path]`-include it and exhaustively model-check the retire / publish /
//! snapshot / validate interleavings. The main build never sets `cfg(loom)`.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{fence, AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{fence, AtomicU64, Ordering};
}

use atomic::{fence, AtomicU64, Ordering};

/// Initial word for an offset first observed by a writer (unstable gen 1).
pub const UNSTABLE_FIRST: u64 = 1 << 1;
/// Initial word for an offset first observed by a publish (stable gen 0).
pub const STABLE_FIRST: u64 = 1;
/// Snapshot sentinel for offsets with no recorded incarnation (written
/// before this process / by another node): treated as stable.
pub const UNKNOWN_STABLE: u64 = u64::MAX;

/// gen+1, stable=0 — the offset is owned by a writer whose data is not yet
/// on the device (or was retired by a free). Fills must not publish.
///
/// The trailing `Release` fence is the writer half of the seqlock pairing
/// (found by the loom models): it orders this word-write before the
/// caller's subsequent data writes, so a reader whose data read observed
/// post-retire bytes and then runs `still`'s `Acquire` fence is guaranteed
/// to observe the retired word and fail validation. Without it, a fill
/// could read mid-flight bytes yet validate against the pre-retire word.
/// (In production the device-write syscall between retire and publish adds
/// stronger ordering; the protocol must not depend on that.)
pub fn retire(word: &AtomicU64) {
    let _ = word.fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
        Some(((cur >> 1) + 1) << 1)
    });
    fence(Ordering::Release);
}

/// The writer's durable device write for this incarnation completed.
pub fn publish(word: &AtomicU64) {
    word.fetch_or(1, Ordering::AcqRel);
}

/// Snapshot for a cache fill. `None` while unstable — the fill must not
/// publish its bytes.
pub fn snapshot(word: &AtomicU64) -> Option<u64> {
    let w = word.load(Ordering::Acquire);
    if w & 1 == 1 {
        Some(w)
    } else {
        None
    }
}

/// True if the incarnation is unchanged since [`snapshot`] (no
/// retire/publish transitioned the offset during the fill's device read).
///
/// Seqlock-reader ordering (found by the loom models): the caller's data
/// read between [`snapshot`] and this call is unordered against this load —
/// an `Acquire` load only prevents *later* accesses from moving up, so the
/// CPU may satisfy the data read *after* validation succeeds and serve a
/// dead incarnation's bytes. The `Acquire` **fence** orders every prior read
/// (the payload) before this validation load (Boehm, "Can seqlocks get
/// along with programming language memory models?"). x86/TSO masks this;
/// ARM/POWER do not.
pub fn still(word: &AtomicU64, before: u64) -> bool {
    fence(Ordering::Acquire);
    word.load(Ordering::Relaxed) == before
}
