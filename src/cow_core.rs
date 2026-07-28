//! Exclusive-owner copy-on-write cell — the active-block buffer protocol
//! core (zero-copy write-path design §5.2, PR 1).
//!
//! The striped write path accumulates 4 MiB blocks in RAM while the read
//! path hands out zero-copy views of the same memory. The old scheme
//! mutated a shared `bytes::Bytes` through a raw pointer — undefined
//! behavior whenever a reader aliased the buffer, and observable
//! read-your-own-writes instability (a reply handed to the kernel could
//! change while in flight). This cell makes that impossible by
//! construction: mutation requires provable uniqueness; a live shared
//! snapshot forces the writer to copy first (copy-on-write), so a
//! snapshot's payload is immutable for the snapshot's whole lifetime.
//!
//! Uniqueness/visibility protocol: [`Arc::get_mut`] atomically verifies
//! `strong == 1 && weak == 0` with `Acquire` ordering, pairing with the
//! `Release` decrement in `Arc::drop` — a writer that observes uniqueness
//! has synchronized with every dropped snapshot, and a snapshot cloned
//! before the check forces CoW. There is no bespoke atomic protocol here,
//! but per the house mandate ("loom for any new lock-free protocol") this
//! core is dependency-free so `loom-models/` can `#[path]`-include it and
//! exhaustively model-check the get_mut-or-copy / clone / read
//! interleavings against the exact shipped code — cheap insurance that a
//! future "optimization" does not reintroduce in-place mutation of shared
//! memory. The main build never sets `cfg(loom)`.

#[cfg(loom)]
pub(crate) mod sync {
    pub use loom::sync::Arc;
}
#[cfg(not(loom))]
pub(crate) mod sync {
    pub use std::sync::Arc;
}

use sync::Arc;

/// Exclusive-owner CoW cell over a payload `T`.
///
/// The cell itself is the single owner: it is deliberately not `Clone`.
/// Sharing happens only through [`CowCell::share`] handles, which freeze
/// the payload they reference forever.
pub struct CowCell<T> {
    inner: Arc<T>,
}

impl<T> CowCell<T> {
    pub fn new(payload: T) -> Self {
        Self {
            inner: Arc::new(payload),
        }
    }

    /// Adopt an ALREADY-SHARED payload (the placed-sever adoption,
    /// shim-parity 2026-07-28): the cell becomes the owner of a payload
    /// other handles still reference (the ring assembly + its in-flight
    /// placed payload `Bytes`). The protocol is unchanged — the cell
    /// starts non-unique, so the FIRST [`CowCell::owned_mut`] that runs
    /// while any foreign handle lives copies (exactly the live-snapshot
    /// rule); uniqueness returns when the last foreign handle drops.
    pub fn adopt(shared: Arc<T>) -> Self {
        Self { inner: shared }
    }

    /// Zero-copy shared snapshot handle. The payload it references is
    /// immutable for as long as any handle lives: a later
    /// [`CowCell::owned_mut`] that finds the cell shared copies first.
    pub fn share(&self) -> Arc<T> {
        Arc::clone(&self.inner)
    }

    /// Exclusive payload access. O(1) when the cell is provably unique;
    /// when a snapshot is still alive, `duplicate` builds the writer's
    /// private replacement payload (copy-on-write), which becomes the
    /// cell's current payload. Returns `(copied, payload)` — `copied` is
    /// true iff CoW fired (observability: `active_block_cow_copies`).
    pub fn owned_mut(&mut self, duplicate: impl FnOnce(&T) -> T) -> (bool, &mut T) {
        let copied = Arc::get_mut(&mut self.inner).is_none();
        if copied {
            let fresh = duplicate(&self.inner);
            self.inner = Arc::new(fresh);
        }
        (
            copied,
            // Uniqueness was proven by `get_mut`, or just established by
            // swapping in a fresh never-shared Arc.
            Arc::get_mut(&mut self.inner).expect("uniqueness just proven or established"),
        )
    }

    /// Read-only payload access through the owner (no refcount traffic).
    pub fn peek(&self) -> &T {
        &self.inner
    }
}
