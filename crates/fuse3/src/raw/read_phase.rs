//! Transport-side READ residence decomposition (`read_transport_phase_ns`)
//! — the 2026-08-01 serve-latency decomposition campaign's fuse3 leg (the
//! `numa_local_bytes`/`fuse3_numa_*` exposure precedent: statics here,
//! `pub fn` snapshots re-exported for the daemon's stats inode).
//!
//! Phases (per FUSE_READ delivered over the armed uring transport):
//!
//! - `queue_wait` — ring CQE reaped (inbound push) → session dispatch pop.
//!   Serialization in the per-queue dispatch loops shows HERE.
//! - `dispatch_lag` — dispatch → the read-handler future's first poll
//!   (tpc-lane scheduling; the write side's `detach_lag` twin).
//! - `reply_commit` — `fs.read` returned → the reply handed to the
//!   transport (in-place arm: the synchronous COMMIT enqueue completed).
//! - `transport_total` — inbound push → reply committed. The daemon's
//!   whole visible span: `fio clat − transport_total` = the kernel-side
//!   residue (request formation/queueing before ring delivery +
//!   completion wake), statable by subtraction.
//!
//! Always-on, READ-opcode-only (≤ 4 `Instant` reads per READ — invisible
//! at any credible op rate); histograms bucket through the SHARED
//! `latency_core` (the same function the root `LatencyHistogram` uses),
//! so the daemon-side serve tables and this family compose
//! bucket-for-bucket. Error replies deliberately record nothing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::latency_core::{latency_bucket_index, LATENCY_BUCKETS};

/// Transport-side READ residence phases. `repr(usize)` indexes the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ReadTransportPhase {
    /// Ring CQE reaped (inbound-queue push) → session dispatch pop.
    QueueWait = 0,
    /// Dispatch → read-handler future first poll (tpc-lane scheduling).
    DispatchLag = 1,
    /// Handler returned → reply committed to the transport.
    ReplyCommit = 2,
    /// Inbound push → reply committed (the daemon's whole visible span).
    TransportTotal = 3,
}

const PHASES: usize = 4;
const PHASE_NAMES: [&str; PHASES] = [
    "queue_wait",
    "dispatch_lag",
    "reply_commit",
    "transport_total",
];

fn table() -> &'static [[AtomicU64; LATENCY_BUCKETS]; PHASES] {
    static TABLE: OnceLock<[[AtomicU64; LATENCY_BUCKETS]; PHASES]> = OnceLock::new();
    TABLE.get_or_init(|| std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))))
}

/// Record one transport-phase span (always-on; see the module doc for the
/// cost contract).
#[inline]
pub fn read_transport_phase_record(phase: ReadTransportPhase, dur: Duration) {
    let idx = latency_bucket_index(dur.as_micros() as u64);
    table()[phase as usize][idx].fetch_add(1, Ordering::Relaxed);
}

/// Snapshot for the daemon's stats inode (`read_transport_phase_ns`):
/// `(phase name, bucket counts)` in phase order; buckets are index-aligned
/// with `latency_core::LATENCY_BUCKET_LABELS`.
pub fn read_transport_phase_snapshot() -> [(&'static str, [u64; LATENCY_BUCKETS]); PHASES] {
    let t = table();
    std::array::from_fn(|pi| {
        (
            PHASE_NAMES[pi],
            std::array::from_fn(|bi| t[pi][bi].load(Ordering::Relaxed)),
        )
    })
}

/// Process-monotonic ns clock for cross-task arrival stamps (stamps live
/// in atomics; one shared epoch so spans compose across the reap → pop →
/// handler → reply chain).
pub(crate) fn transport_now_ns() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

static READ_INPLACE_REPLIES: AtomicU64 = AtomicU64::new(0);

/// Count one in-place READ reply (the P2 armed-session arm: a synchronous
/// COMMIT enqueue from the handler task, no reply-task hop).
#[inline]
pub(crate) fn note_read_inplace_reply() {
    READ_INPLACE_REPLIES.fetch_add(1, Ordering::Relaxed);
}

/// In-place READ reply engagement gauge (stats inode
/// `fuse3_read_inplace_replies`): on an armed over-uring session this must
/// account ≈ every READ — found structurally disengaged (0 forever) by the
/// 2026-08-01 serve-decomposition campaign (the dispatch loop `take()`s the
/// session's connection handle, so the handler's in-place arm never saw
/// one; fixed alongside this gauge, which is what keeps it fixed).
pub fn read_inplace_replies() -> u64 {
    READ_INPLACE_REPLIES.load(Ordering::Relaxed)
}
