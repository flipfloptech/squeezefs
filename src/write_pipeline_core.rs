//! Write-pipeline admission accounting core (2026-07-27 depth campaign).
//!
//! The lock-free heart of [`crate::write_pipeline::WritePipeline`]: the
//! in-flight byte/block gauges and the single-CAS admission attempt the
//! `admit` loop drives. Self-contained so `loom-models/` can
//! `#[path]`-include it and exhaustively check the admission/release
//! interleavings (over-admission, empty-pipe bypass, release underflow,
//! settle-to-zero). The main build never sets `cfg(loom)`.
//!
//! **Wake-liveness precondition (stated because the model cannot see its
//! violation):** the shipped `admit` parks on `tokio::sync::Notify::
//! notified()` raced against a 5 ms tick. `notify_waiters` stores no
//! permit, so a release's wake CAN be lost to a not-yet-parked waiter —
//! BY DESIGN the tick is the liveness backstop (the waiter re-polls the
//! predicate at most 5 ms late; Red/target changes are observed the same
//! way). The loom model therefore treats parking as a spurious-wake loop
//! (tick semantics) and checks the ACCOUNTING invariants exhaustively;
//! it deliberately does not certify permit-style wake delivery, which
//! the implementation does not claim.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU64, Ordering};
}

use atomic::{AtomicU64, Ordering};

/// Outcome of one admission attempt (one predicate evaluation + at most
/// one CAS) against a caller-supplied byte target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitAttempt {
    /// Charged: the caller owns `bytes` of in-flight custody (release
    /// exactly once).
    Admitted,
    /// The CAS raced a concurrent admission/completion — re-evaluate
    /// (the caller's loop recomputes the target first).
    Raced,
    /// The pipe is at target — park and retry.
    Full,
}

/// In-flight admission gauges. Invariants (loom-checked):
///
/// 1. **Bounded admission**: every successful CAS observed
///    `cur + bytes <= target` (or the empty-pipe bypass below), so at the
///    instant of admission `inflight_bytes <= max(target, bypass_bytes)`.
/// 2. **Single oversized bypass**: `blocks == 0` admits one block larger
///    than the target (progress guarantee); the CAS on `inflight_bytes`
///    serializes racing bypassers — at most ONE oversized admission can
///    land on an empty pipe, the loser re-observes a non-empty pipe.
/// 3. **Exact settle**: blocks/bytes match outstanding admissions at all
///    times and return to exactly zero once every admission released.
#[derive(Default)]
pub struct AdmissionCore {
    inflight_bytes: AtomicU64,
    inflight_blocks: AtomicU64,
}

impl AdmissionCore {
    pub fn new() -> Self {
        Self {
            inflight_bytes: AtomicU64::new(0),
            inflight_blocks: AtomicU64::new(0),
        }
    }

    /// One admission attempt for `bytes` against `target` (see
    /// [`AdmitAttempt`]). The empty-pipe bypass keeps oversized blocks
    /// admissible (progress guarantee: an empty pipe always admits).
    ///
    /// The bypass predicate is `cur == 0` on the BYTES gauge — the same
    /// word the CAS charges — never the blocks counter: the counter is
    /// incremented after the CAS, so it LAGS, and a predicate reading it
    /// admitted TWO racing oversized bypassers onto an empty pipe (loom
    /// `write_pipeline_empty_pipe_bypass_is_single`, red against the
    /// blocks-counter shape this replaced). With `cur == 0` the CAS
    /// itself serializes bypassers: the loser re-observes a non-zero
    /// gauge and parks.
    pub fn try_admit_once(&self, bytes: u64, target: u64) -> AdmitAttempt {
        let cur = self.inflight_bytes.load(Ordering::Relaxed);
        if cur != 0 && cur.saturating_add(bytes) > target {
            return AdmitAttempt::Full;
        }
        if self
            .inflight_bytes
            .compare_exchange(cur, cur + bytes, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            self.inflight_blocks.fetch_add(1, Ordering::AcqRel);
            AdmitAttempt::Admitted
        } else {
            AdmitAttempt::Raced
        }
    }

    /// Return `bytes` of custody (exactly once per admission; the RAII
    /// permit in `write_pipeline.rs` owns the exactly-once).
    pub fn release(&self, bytes: u64) {
        self.inflight_bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.inflight_blocks.fetch_sub(1, Ordering::AcqRel);
    }

    pub fn inflight_bytes(&self) -> u64 {
        self.inflight_bytes.load(Ordering::Relaxed)
    }

    pub fn inflight_blocks(&self) -> u64 {
        self.inflight_blocks.load(Ordering::Acquire)
    }
}
