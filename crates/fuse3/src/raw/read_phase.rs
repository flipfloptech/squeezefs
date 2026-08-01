//! Transport-side residence decomposition (`read_transport_phase_ns` /
//! `write_transport_phase_ns`) — the 2026-08-01 serve-latency decomposition
//! campaign's fuse3 leg (READ), extended to WRITE by the transport-ingress
//! campaign (the `numa_local_bytes`/`fuse3_numa_*` exposure precedent:
//! statics here, `pub fn` snapshots re-exported for the daemon's stats
//! inode).
//!
//! Phases (per FUSE_READ / FUSE_WRITE delivered over the armed uring
//! transport — one shared [`TransportPhase`] set, one table per op class,
//! so the two families compose in one analyzer):
//!
//! - `queue_wait` — ring CQE reaped (inbound push) → session dispatch pop.
//!   Serialization in the per-queue dispatch loops shows HERE.
//! - `dispatch_lag` — dispatch → the handler future's first poll
//!   (tpc-lane scheduling; the write side's `detach_lag` twin).
//! - `reply_commit` — `fs.read`/`fs.write` returned → the reply handed to
//!   the transport (in-place arm: the synchronous COMMIT enqueue
//!   completed).
//! - `transport_total` — inbound push → reply committed. The daemon's
//!   whole visible span: `fio clat − transport_total` = the kernel-side
//!   residue (request formation/queueing before ring delivery +
//!   completion wake), statable by subtraction — for BOTH walls.
//!
//! Always-on, READ+WRITE-opcode-only (≤ 4 `Instant` reads per op —
//! invisible at any credible op rate); histograms bucket through the
//! SHARED `latency_core` (the same function the root `LatencyHistogram`
//! uses), so the daemon-side serve tables and these families compose
//! bucket-for-bucket. Error replies deliberately record nothing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::latency_core::{latency_bucket_index, LATENCY_BUCKETS};

/// Transport-side residence phases (READ and WRITE families share the
/// set). `repr(usize)` indexes the tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum TransportPhase {
    /// Ring CQE reaped (inbound-queue push) → session dispatch pop.
    QueueWait = 0,
    /// Dispatch → handler future first poll (tpc-lane scheduling).
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

/// Op classes carrying a transport phase table. `repr(usize)` indexes the
/// outer table dimension.
#[repr(usize)]
enum OpClass {
    Read = 0,
    Write = 1,
}

const OP_CLASSES: usize = 2;

type PhaseTable = [[AtomicU64; LATENCY_BUCKETS]; PHASES];

fn tables() -> &'static [PhaseTable; OP_CLASSES] {
    static TABLES: OnceLock<[PhaseTable; OP_CLASSES]> = OnceLock::new();
    TABLES.get_or_init(|| {
        std::array::from_fn(|_| std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))))
    })
}

#[inline]
fn record(op: OpClass, phase: TransportPhase, dur: Duration) {
    let idx = latency_bucket_index(dur.as_micros() as u64);
    tables()[op as usize][phase as usize][idx].fetch_add(1, Ordering::Relaxed);
}

fn snapshot(op: OpClass) -> [(&'static str, [u64; LATENCY_BUCKETS]); PHASES] {
    let t = &tables()[op as usize];
    std::array::from_fn(|pi| {
        (
            PHASE_NAMES[pi],
            std::array::from_fn(|bi| t[pi][bi].load(Ordering::Relaxed)),
        )
    })
}

/// Record one READ transport-phase span (always-on; see the module doc for
/// the cost contract).
#[inline]
pub fn read_transport_phase_record(phase: TransportPhase, dur: Duration) {
    record(OpClass::Read, phase, dur);
}

/// Record one WRITE transport-phase span (always-on).
#[inline]
pub fn write_transport_phase_record(phase: TransportPhase, dur: Duration) {
    record(OpClass::Write, phase, dur);
}

/// Snapshot for the daemon's stats inode (`read_transport_phase_ns`):
/// `(phase name, bucket counts)` in phase order; buckets are index-aligned
/// with `latency_core::LATENCY_BUCKET_LABELS`.
pub fn read_transport_phase_snapshot() -> [(&'static str, [u64; LATENCY_BUCKETS]); PHASES] {
    snapshot(OpClass::Read)
}

/// Snapshot for the daemon's stats inode (`write_transport_phase_ns`).
pub fn write_transport_phase_snapshot() -> [(&'static str, [u64; LATENCY_BUCKETS]); PHASES] {
    snapshot(OpClass::Write)
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

#[cfg(test)]
mod transport_phase_tests {
    use super::*;

    /// One span recorded on one family moves exactly that (family, phase)
    /// cell — the root suite pins the same law through the public API;
    /// this is the fork-local twin so the fuse3 standalone suite carries
    /// the contract too.
    #[test]
    fn families_are_phase_exact_and_independent() {
        let sum = |snap: &[(&'static str, [u64; LATENCY_BUCKETS]); PHASES]| -> Vec<u64> {
            snap.iter().map(|(_, b)| b.iter().sum::<u64>()).collect()
        };

        let r0 = sum(&read_transport_phase_snapshot());
        let w0 = sum(&write_transport_phase_snapshot());

        write_transport_phase_record(TransportPhase::QueueWait, Duration::from_micros(100));

        let r1 = sum(&read_transport_phase_snapshot());
        let w1 = sum(&write_transport_phase_snapshot());
        assert_eq!(r1, r0, "write span must not move the read family");
        for (pi, name) in PHASE_NAMES.iter().enumerate() {
            let want = u64::from(*name == "queue_wait");
            assert_eq!(
                w1[pi] - w0[pi],
                want,
                "write family phase {name} moved unexpectedly"
            );
        }

        read_transport_phase_record(TransportPhase::ReplyCommit, Duration::from_micros(100));
        let r2 = sum(&read_transport_phase_snapshot());
        let w2 = sum(&write_transport_phase_snapshot());
        assert_eq!(w2, w1, "read span must not move the write family");
        for (pi, name) in PHASE_NAMES.iter().enumerate() {
            let want = u64::from(*name == "reply_commit");
            assert_eq!(
                r2[pi] - r1[pi],
                want,
                "read family phase {name} moved unexpectedly"
            );
        }
    }
}
