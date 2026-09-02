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
//! Always-on, READ+WRITE-opcode-only (≤ 4 `Instant` reads per op — the
//! "invisible at any credible op rate" claim is asserted, not bracketed;
//! see PERF-23); histograms are SHARDED per recording thread (PERF-3) and
//! bucket through the
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
    /// COMMIT-carrying ring-flush syscall duration (2026-08-04 kmbuf
    /// campaign): the venue where the kernel's commit-side copy
    /// machinery runs (`fuse_uring_copy_from_ring` — per-page
    /// `FR_LOCKED`/GUP on user ents, kaddr short-circuit on the kmbuf
    /// arm), so the killed term shows as a before/after delta of this
    /// phase. **Per-FLUSH sampled, not per-op**: recorded once per
    /// op-class present in the batch, and only on flushes whose syscall
    /// is provably wait-free (explicit mid-pass flushes, or loop-bottom
    /// flushes entered with the CQ already non-empty — the saturated
    /// passes, exactly where the term is measurable without paying a
    /// second syscall). Idle-pass flushes are deliberately unsampled
    /// (their duration is dominated by the park).
    CommitFlush = 4,
}

const PHASES: usize = 5;
const PHASE_NAMES: [&str; PHASES] = [
    "queue_wait",
    "dispatch_lag",
    "reply_commit",
    "transport_total",
    "commit_flush",
];

/// Op classes carrying a transport phase table. `repr(usize)` indexes the
/// outer table dimension.
#[repr(usize)]
enum OpClass {
    Read = 0,
    Write = 1,
}

const OP_CLASSES: usize = 2;

/// One phase's storage: the standard bucket set plus the exact `count` /
/// `sum_ns` words (e2e audit A, 2026-09-02 — a bucket-only table can only
/// yield midpoint-estimated means). Recording is three relaxed RMWs on
/// this thread's shard.
struct PhaseHist {
    buckets: [AtomicU64; LATENCY_BUCKETS],
    count: AtomicU64,
    sum_ns: AtomicU64,
}

impl PhaseHist {
    const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; LATENCY_BUCKETS],
            count: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
        }
    }

    #[inline]
    fn record_ns(&self, ns: u64) {
        self.buckets[latency_bucket_index(ns / 1_000)].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
    }
}

/// One phase's folded snapshot (Σ over the per-thread shards): bucket
/// counts index-aligned with `latency_core::LATENCY_BUCKET_LABELS`, plus
/// the exact `count` and `sum_ns`.
#[derive(Debug, Clone, Copy)]
pub struct PhaseSnapshot {
    pub name: &'static str,
    pub buckets: [u64; LATENCY_BUCKETS],
    pub count: u64,
    pub sum_ns: u64,
}

fn fold<'a>(name: &'static str, hists: impl Iterator<Item = &'a PhaseHist>) -> PhaseSnapshot {
    let mut snap = PhaseSnapshot {
        name,
        buckets: [0; LATENCY_BUCKETS],
        count: 0,
        sum_ns: 0,
    };
    for h in hists {
        for (b, a) in snap.buckets.iter_mut().zip(h.buckets.iter()) {
            *b += a.load(Ordering::Relaxed);
        }
        snap.count += h.count.load(Ordering::Relaxed);
        snap.sum_ns += h.sum_ns.load(Ordering::Relaxed);
    }
    snap
}

type PhaseTable = [PhaseHist; PHASES];

/// **PERF-3 — the tables are SHARDED per recording thread.**
///
/// A latency distribution is tight by construction, so nearly every op of a
/// class lands in the SAME bucket word: the phase histograms were ~5
/// process-global atomic RMWs per request on ~5 cache lines shared by every
/// queue worker and handler lane. `Align64` prevents false sharing but not
/// TRUE sharing — the line ping-pongs between cores regardless. Each thread
/// now owns a shard (one whole `PhaseTable`, so shards never share a line),
/// and `snapshot` sums across them: the counting cost becomes an
/// uncontended RMW on a core-local line, and the reported histogram is
/// unchanged (addition commutes; a snapshot taken mid-op can miss a
/// just-recorded sample exactly as before).
///
/// Shard count derives from the machine (the standing derivation law — no
/// free-floating constant): `available_parallelism` rounded up to a power of
/// two, railed to [1, 64] so a 256-core host does not spend 3 MiB on
/// histograms.
fn shard_count() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1)
            .next_power_of_two()
            .clamp(1, 64)
    })
}

/// `shards[shard][op_class]`.
fn tables() -> &'static [[PhaseTable; OP_CLASSES]] {
    static TABLES: OnceLock<Vec<[PhaseTable; OP_CLASSES]>> = OnceLock::new();
    TABLES.get_or_init(|| {
        (0..shard_count())
            .map(|_| std::array::from_fn(|_| std::array::from_fn(|_| PhaseHist::new())))
            .collect()
    })
}

/// This thread's shard index — assigned once per thread, round-robin over
/// the shard set (the `wake_core`/pool-index precedent: no `sched_getcpu`
/// per op, and a migrating thread keeps counting into a line it owns).
fn shard_index() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    thread_local! {
        static MINE: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    }
    MINE.with(|c| {
        let mut v = c.get();
        if v == usize::MAX {
            v = NEXT.fetch_add(1, Ordering::Relaxed) % shard_count();
            c.set(v);
        }
        v
    })
}

#[inline]
fn record(op: OpClass, phase: TransportPhase, dur: Duration) {
    let ns = dur.as_nanos().min(u64::MAX as u128) as u64;
    tables()[shard_index()][op as usize][phase as usize].record_ns(ns);
}

fn snapshot(op: OpClass) -> [PhaseSnapshot; PHASES] {
    let shards = tables();
    let oc = op as usize;
    std::array::from_fn(|pi| fold(PHASE_NAMES[pi], shards.iter().map(|s| &s[oc][pi])))
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

/// Snapshot for the daemon's stats inode (`read_transport_phase_ns`), in
/// phase order; the fold across shards is exact.
pub fn read_transport_phase_snapshot() -> [PhaseSnapshot; PHASES] {
    snapshot(OpClass::Read)
}

/// Snapshot for the daemon's stats inode (`write_transport_phase_ns`).
pub fn write_transport_phase_snapshot() -> [PhaseSnapshot; PHASES] {
    snapshot(OpClass::Write)
}

/// Process-monotonic ns clock for cross-task arrival stamps (stamps live
/// in atomics; one shared epoch so spans compose across the reap → pop →
/// handler → reply chain).
pub(crate) fn transport_now_ns() -> u64 {
    transport_epoch().elapsed().as_nanos().min(u64::MAX as u128) as u64
}

fn transport_epoch() -> &'static Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now)
}

/// The `Instant` a [`transport_now_ns`] stamp names (epoch + ns — pure
/// arithmetic, no clock read): the form the op-trace hooks take, so a
/// stamp already read for a phase histogram is passed through instead
/// of read again (audit A2's one-clock-read law).
#[inline]
pub(crate) fn transport_instant(ns: u64) -> Instant {
    *transport_epoch() + Duration::from_nanos(ns)
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

static WRITE_INPLACE_REPLIES: AtomicU64 = AtomicU64::new(0);

/// Count one in-place WRITE reply (the READ P2 arm's WRITE twin: a
/// synchronous COMMIT enqueue from the handler task — the reply-channel +
/// reply-task hop, whose wake pays the pinned main-runtime hostage class,
/// is gone).
#[inline]
pub(crate) fn note_write_inplace_reply() {
    WRITE_INPLACE_REPLIES.fetch_add(1, Ordering::Relaxed);
}

/// In-place WRITE reply engagement gauge (stats inode
/// `fuse3_write_inplace_replies`): on an armed over-uring session this
/// must account ≈ every successful WRITE — the gauge is what keeps the
/// arm wired (the READ arm sat structurally disengaged for weeks because
/// nothing measured it; see [`read_inplace_replies`]).
pub fn write_inplace_replies() -> u64 {
    WRITE_INPLACE_REPLIES.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The FUSED-op timeline (write-IOPS campaign, 2026-08-11): three engaged
// levers (guard convoy, pass funnel, purge economy) left rand-4k pinned at
// ~390k while CPUs sat 45 % idle and devices at 1 % — the residual ~2.2 ms
// of per-op daemon queueing needs NAMED stages before another lever. Two
// always-on instruments decompose a fused write's life:
//
// * `wake_to_poll` — run-queue push (spawn OR waker) → the drain's poll of
//   that task: the scheduling quantum every hop pays. Recorded per poll.
// * `bridge_rtt` — zc bridge SQE issue (`BridgeDeadlines::stamp`) → its
//   resolution reaching the handler's oneshot (`done.send`): the device
//   round trip AS THE HANDLER EXPERIENCES IT, venue-split by the reap
//   counters below (a mid-pass reap resolves in-window; a pass-bottom reap
//   ate a park + re-drain first).
//
// The reap venue counters are the funnel fix's ENGAGEMENT instrument —
// shipped late (the fix landed without one, against the house rule; this
// is the correction): `midpass / (midpass + passbottom)` ≈ the share of
// bridge DMAs the interleave resolved without waiting out the pass.
// ---------------------------------------------------------------------------

static FUSED_WAKE_TO_POLL: PhaseHist = PhaseHist::new();
static FUSED_BRIDGE_RTT: PhaseHist = PhaseHist::new();
static FUSED_MIDPASS_REAPS: AtomicU64 = AtomicU64::new(0);
static FUSED_PASSBOTTOM_REAPS: AtomicU64 = AtomicU64::new(0);

/// Record one run-queue push → poll span (ns, transport epoch).
#[inline]
pub(crate) fn note_fused_wake_to_poll(ns: u64) {
    FUSED_WAKE_TO_POLL.record_ns(ns);
}

/// Record one bridge issue → oneshot-resolution span (ns) plus its venue.
#[inline]
pub(crate) fn note_fused_bridge_resolved(ns: u64, midpass: bool) {
    FUSED_BRIDGE_RTT.record_ns(ns);
    if midpass {
        FUSED_MIDPASS_REAPS.fetch_add(1, Ordering::Relaxed);
    } else {
        FUSED_PASSBOTTOM_REAPS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Stats-inode snapshot: the fused timeline histograms.
pub fn fused_timeline_snapshot() -> [PhaseSnapshot; 2] {
    [
        fold("wake_to_poll", std::iter::once(&FUSED_WAKE_TO_POLL)),
        fold("bridge_rtt", std::iter::once(&FUSED_BRIDGE_RTT)),
    ]
}

/// Mid-pass bridge resolutions (the funnel-fix engagement gauge).
pub fn fused_midpass_reaps() -> u64 {
    FUSED_MIDPASS_REAPS.load(Ordering::Relaxed)
}

/// Pass-bottom bridge resolutions (the park-then-resolve venue).
pub fn fused_passbottom_reaps() -> u64 {
    FUSED_PASSBOTTOM_REAPS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod transport_phase_tests {
    use super::*;

    /// The tables are process-global; delta tests serialize.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// One span recorded on one family moves exactly that (family, phase)
    /// cell — the root suite pins the same law through the public API;
    /// this is the fork-local twin so the fuse3 standalone suite carries
    /// the contract too.
    #[test]
    fn families_are_phase_exact_and_independent() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let sum = |snap: &[PhaseSnapshot; PHASES]| -> Vec<u64> {
            snap.iter().map(|p| p.buckets.iter().sum::<u64>()).collect()
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

    /// e2e audit A: the per-thread shards fold to EXACT count / sum_ns
    /// (the root suite pins the same law through the daemon's JSON).
    #[test]
    fn shard_fold_is_exact_to_the_ns() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        const THREADS: u64 = 5;
        const PER_THREAD: u64 = 200;
        let pi = TransportPhase::DispatchLag as usize;
        let before = read_transport_phase_snapshot()[pi];
        let hs: Vec<_> = (0..THREADS)
            .map(|t| {
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        read_transport_phase_record(
                            TransportPhase::DispatchLag,
                            Duration::from_nanos(3_000 * (t + 1) + 11 * i),
                        );
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let after = read_transport_phase_snapshot()[pi];
        let want_sum: u64 = (0..THREADS)
            .flat_map(|t| (0..PER_THREAD).map(move |i| 3_000 * (t + 1) + 11 * i))
            .sum();
        assert_eq!(after.count - before.count, THREADS * PER_THREAD);
        assert_eq!(after.sum_ns - before.sum_ns, want_sum);
        let bucket_delta: u64 = after
            .buckets
            .iter()
            .zip(before.buckets.iter())
            .map(|(a, b)| a - b)
            .sum();
        assert_eq!(bucket_delta, THREADS * PER_THREAD, "Σ buckets ≡ count");
    }
}
