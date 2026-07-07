//! Block refcount cell protocol — acquire-from-nonzero.
//!
//! A block's refcount reaching zero is a **terminal** transition: the freeing
//! thread retires the block's incarnation and returns the offset to the free
//! list. A racing "clone" increment must therefore never resurrect a count
//! that already hit zero — a plain `fetch_add` can land after the
//! decrement-to-zero and leave the cloner holding a reference to a freed
//! (reallocatable) offset, which reads as foreign bytes after reuse.
//!
//! [`try_acquire`] only succeeds from a nonzero count (CAS loop);
//! [`release`] reports the terminal zero exactly once. Self-contained so
//! `loom-models/` can `#[path]`-include it and exhaustively check the
//! acquire/release interleavings. The main build never sets `cfg(loom)`.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU32, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU32, Ordering};
}

use atomic::{AtomicU32, Ordering};

/// Take one reference iff the count is still nonzero. `false` means the
/// block is (or is becoming) freed — the caller must treat it as gone.
pub fn try_acquire(cell: &AtomicU32) -> bool {
    let mut cur = cell.load(Ordering::Acquire);
    loop {
        if cur == 0 {
            return false;
        }
        match cell.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => cur = actual,
        }
    }
}

/// Drop one reference; `true` exactly when this call performed the terminal
/// 1 → 0 transition (the caller then frees the block). Saturates at zero so
/// an over-release can never wrap.
pub fn release(cell: &AtomicU32) -> bool {
    let mut cur = cell.load(Ordering::Acquire);
    loop {
        if cur == 0 {
            return false; // already terminal; never free twice
        }
        match cell.compare_exchange_weak(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return cur == 1,
            Err(actual) => cur = actual,
        }
    }
}
