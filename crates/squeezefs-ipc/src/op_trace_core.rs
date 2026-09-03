//! Per-op trace-ring CORE (e2e audit A2, `docs/design-e2e-perf-audit.md`
//! §1 honesty precondition 2 / Appendix D item 2): the pure, dependency-
//! free half of the instrument that joins the stages of ONE op on one
//! timeline — the stage vocabulary spanning every plane, the fixed-size
//! sample, the per-thread SPSC ring, and the sampling law.
//!
//! Canonical file in the `squeezefs-ipc` tree, `#[path]`-included by the
//! fuse3 fork (`fuse3::op_trace_core`, whose `op_trace` module owns the
//! STORAGE — ring pool, arm state, the task-scoped current op — and which
//! the root crate reaches through the normal dependency, so exactly ONE
//! ring set exists per process) and by `loom-models` (the SPSC ring is a
//! lock-free core; its model checks this file, not a copy). No statics
//! here: storage belongs to the including crate, exactly like
//! `latency_core.rs`.
//!
//! ## The sample
//!
//! `(op_id: u64, stage: u16, mono_ns: u64)` — three words (24 B) per
//! slot, pre-allocated at arm time; the hot path never allocates. `stage`
//! is a [`Stage`] id; `mono_ns` is CLOCK_MONOTONIC ns — the same clock
//! `bpftrace`'s `nsecs` (`bpf_ktime_get_ns`) and `perf record -k
//! CLOCK_MONOTONIC` stamp kernel tracepoints with, which is what makes a
//! `unique`-keyed join a MEASUREMENT rather than a cross-clock guess.
//!
//! ## The ring
//!
//! [`TraceRing`] is a Lamport SPSC ring over `[AtomicU64; 3]` slots: the
//! owning thread pushes (Release on `head`), the stats-inode drain pops
//! (Acquire on `head`, Release on `tail`). A full ring DROPS the push and
//! the caller counts it — a trace is a diagnostic and may never park,
//! spill or allocate on the data path. The producer's Release store of
//! `head` publishes the three slot words to the consumer's Acquire load
//! — weakening either side drains an unwritten `(0, 0, 0)` slot, which
//! the `op_trace_ring_*` loom models catch (weakening-verified
//! 2026-09-02). The producer's Acquire load of `tail` against the
//! consumer's Release store is the reuse edge (a slot is overwritten only
//! after its sample was read); its failure is a load-store reordering
//! loom's stale-read model cannot express, so that pair stands on the
//! Lamport SPSC argument, not on a model.
//!
//! ## The sampling law
//!
//! One op is either fully traced or not at all: every hook decides from
//! the op id alone through [`selected`] — Fibonacci hashing against a
//! threshold, so the choice is deterministic per id, stride-independent
//! (FUSE uniques step by `FUSE_REQ_ID_STEP = 2`, il tickets by 1), and
//! costs one multiply, never a division.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// One trace stage — the END boundary of the phase it is named beside
/// (a sample at stage X carries the instant the phase-X histogram
/// `record()` fired), so per-op stage deltas ARE the phase spans and
/// the stitch tool's containment check reads `Σ deltas ≈ histogram sum`
/// over the same op population. `repr(u16)`; explicit ids are the wire
/// vocabulary (`.trace` ships the id→name table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum Stage {
    // -- transport (fuse3, `read/write_transport_phase_ns`) -----------
    /// Ring CQE reaped — the request's arrival in the daemon
    /// (`arrived_ns`; queue_wait's start).
    TransportRecv = 1,
    /// Session dispatch pop (queue_wait end).
    Dispatch = 2,
    /// Handler future's first poll (dispatch_lag end).
    HandlerEntry = 3,
    /// `fs.<op>` returned (reply_commit's start).
    HandlerReturn = 4,
    /// Reply committed to the transport (reply_commit / transport_total
    /// end).
    ReplyCommit = 5,
    /// READ fast-dispatch (e2e perf audit R-2): the reaping queue worker
    /// ran the filesystem's sync probe and served this READ inline —
    /// stamped at the SAME instant as `transport_recv`, and standing in
    /// for `dispatch` AND `handler_entry` (the stitch aliases it), so
    /// `queue_wait` and `dispatch_lag` read 0 on the served op. A demoted
    /// READ stamps the ordinary `dispatch` at its mint instead.
    FastDispatch = 6,

    // -- read serve (root, `read_serve_phase_ns` ends) -----------------
    /// prelude end: handler → router dispatch.
    ReadRouted = 16,
    /// meta_resolve end.
    MetaResolved = 17,
    /// key_resolve end.
    KeysResolved = 18,
    /// classify_probe end (the serve-or-fetch decision).
    TierProbed = 19,
    /// sf_wait end (cohort waiter served).
    CohortServed = 20,
    /// block_fetch end.
    BlockFetched = 21,
    /// binding_check end.
    BindingChecked = 22,
    /// slice_out end (bytes in the reply payload).
    ServeCopied = 23,
    /// post_validate end.
    ReadValidated = 24,
    /// total end: the data-read handler returns.
    ReadReturn = 25,

    // -- device funnel + read fill (`read_fill_phase_ns` ends) ---------
    /// NvmeBlockDev: SQE submitted (dev_queue end) — read AND write
    /// funnels.
    DevSubmit = 32,
    /// NvmeBlockDev: CQE completed (dev_service end) — both funnels.
    DevComplete = 33,
    /// fetch_dma end.
    FetchDone = 34,
    /// decode end.
    Decoded = 35,
    /// admission end.
    Admitted = 36,
    /// deposit end.
    Deposited = 37,
    /// fill_total end.
    FillDone = 38,
    /// zc bridge (`zc_bridge_phase_ns`): the handler's fetch message
    /// sent to the queue worker (msg_hop's start / total's start).
    BridgeSent = 39,
    /// zc bridge: the queue worker took the message and built the SQE
    /// (msg_hop end; `dev_submit` is the enter that carries it,
    /// `dev_complete` its CQE pop, `block_fetched` the handler's resume).
    BridgeTaken = 40,

    // -- write pipeline (`write_pipeline_phase_ns` ends) ---------------
    /// admit_wait end (the pre-ACK admission).
    WriteAdmitted = 48,
    /// detach_lag end (the detached upload's first poll).
    WriteDetached = 49,
    /// lock_wait end.
    WriteLocked = 50,
    /// crypto end.
    WriteEncoded = 51,
    /// allocate end.
    WriteAllocated = 52,
    /// dma end.
    WriteDmaDone = 53,
    /// publish end.
    WritePublished = 54,
    /// displaced_free end.
    WriteFreed = 55,
    /// inval_tail end.
    WriteInvalDone = 56,
    /// total end (the block's whole pipeline residence).
    WriteDone = 57,

    // -- publish conveyor (`publish_phase_ns`) --------------------------
    /// The op's enqueue instant (queue_wait's start).
    PublishEnqueue = 64,
    /// queue_wait end (drained into a pass).
    PublishDrained = 65,
    /// lock_wait end.
    PublishLocked = 66,
    /// base_fetch end.
    PublishBaseReady = 67,
    /// apply end.
    PublishApplied = 68,
    /// save_encode end.
    PublishEncoded = 69,
    /// blob_write end.
    PublishBlobWritten = 70,
    /// meta_commit end.
    PublishCommitted = 71,
    /// commit_guard end.
    CommitGuarded = 72,
    /// commit_inode_read end.
    CommitInodeRead = 73,
    /// commit_slot_probe end.
    CommitSlotProbed = 74,
    /// commit_tx_wait end.
    CommitTxDone = 75,
    /// total end (the op's terminal fan-out).
    PublishDone = 76,

    // -- meta journal conveyor (`meta_txpass_phase_ns`) ----------------
    /// The tx's enqueue instant (tx_queue_wait's start).
    MetaEnqueue = 80,
    /// tx_queue_wait end / the pass's start.
    PassBegin = 81,
    /// pass_admission end.
    PassAdmitted = 82,
    /// pass_leaf_locks end.
    PassLocked = 83,
    /// journal_ring_write end.
    JournalWritten = 84,
    /// journal_prefix_wait end.
    JournalPrefixDone = 85,
    /// journal_barrier end.
    Barrier = 86,
    /// pass_journal_write / pass_total end.
    PassEnd = 87,
    /// The committer's outcome sent (fan-out).
    Fanout = 88,

    // -- metadata op (`meta_op_phase_ns`) -------------------------------
    /// entry_to_backend end (first backend touch).
    MetaBackendStart = 96,
    /// backend end.
    MetaBackendDone = 97,
    /// backend_to_reply / total end (handler return).
    MetaOpReturn = 98,

    // -- il ring (`ipc_ingress_ns` / `ipc_direct_phase_ns`) -------------
    /// The client's publish stamp (ring ingress start).
    IpcIngress = 112,
    /// Daemon dequeue (ingress end; the op's daemon t0).
    IpcDequeue = 113,
    /// admit end (direct-drive slab insert).
    IpcAdmitted = 114,
    /// sq_wait end (the carrying `io_uring_enter`).
    IpcSqEnter = 115,
    /// inflight / device_cq end (CQE popped).
    IpcCqe = 116,
    /// finish / total end (completion posted).
    IpcComplete = 117,

    // -- locks (`lock_phase_ns`) -----------------------------------------
    /// `INODE_META_LOCKS` (3.5) acquired (stripe_lock_wait end).
    StripeLockAcquired = 128,
    /// `INODE_META_LOCKS` (3.5) released (stripe_lock_hold end).
    StripeLockReleased = 129,
    /// 4a exclusive `I{ino}` guard acquired.
    DlmGuardAcquired = 130,
    /// 4a exclusive `I{ino}` guard released (dlm_guard_hold end).
    DlmGuardReleased = 131,
}

impl Stage {
    /// Every stage, in id order (the export's name table).
    pub const ALL: &'static [Stage] = &[
        Stage::TransportRecv,
        Stage::Dispatch,
        Stage::HandlerEntry,
        Stage::HandlerReturn,
        Stage::ReplyCommit,
        Stage::FastDispatch,
        Stage::ReadRouted,
        Stage::MetaResolved,
        Stage::KeysResolved,
        Stage::TierProbed,
        Stage::CohortServed,
        Stage::BlockFetched,
        Stage::BindingChecked,
        Stage::ServeCopied,
        Stage::ReadValidated,
        Stage::ReadReturn,
        Stage::DevSubmit,
        Stage::DevComplete,
        Stage::FetchDone,
        Stage::Decoded,
        Stage::Admitted,
        Stage::Deposited,
        Stage::FillDone,
        Stage::BridgeSent,
        Stage::BridgeTaken,
        Stage::WriteAdmitted,
        Stage::WriteDetached,
        Stage::WriteLocked,
        Stage::WriteEncoded,
        Stage::WriteAllocated,
        Stage::WriteDmaDone,
        Stage::WritePublished,
        Stage::WriteFreed,
        Stage::WriteInvalDone,
        Stage::WriteDone,
        Stage::PublishEnqueue,
        Stage::PublishDrained,
        Stage::PublishLocked,
        Stage::PublishBaseReady,
        Stage::PublishApplied,
        Stage::PublishEncoded,
        Stage::PublishBlobWritten,
        Stage::PublishCommitted,
        Stage::CommitGuarded,
        Stage::CommitInodeRead,
        Stage::CommitSlotProbed,
        Stage::CommitTxDone,
        Stage::PublishDone,
        Stage::MetaEnqueue,
        Stage::PassBegin,
        Stage::PassAdmitted,
        Stage::PassLocked,
        Stage::JournalWritten,
        Stage::JournalPrefixDone,
        Stage::Barrier,
        Stage::PassEnd,
        Stage::Fanout,
        Stage::MetaBackendStart,
        Stage::MetaBackendDone,
        Stage::MetaOpReturn,
        Stage::IpcIngress,
        Stage::IpcDequeue,
        Stage::IpcAdmitted,
        Stage::IpcSqEnter,
        Stage::IpcCqe,
        Stage::IpcComplete,
        Stage::StripeLockAcquired,
        Stage::StripeLockReleased,
        Stage::DlmGuardAcquired,
        Stage::DlmGuardReleased,
    ];

    /// The stage's snake_case name (the `.trace` table + the stitch
    /// tool's vocabulary).
    pub const fn name(self) -> &'static str {
        match self {
            Stage::TransportRecv => "transport_recv",
            Stage::Dispatch => "dispatch",
            Stage::HandlerEntry => "handler_entry",
            Stage::HandlerReturn => "handler_return",
            Stage::ReplyCommit => "reply_commit",
            Stage::FastDispatch => "fast_dispatch",
            Stage::ReadRouted => "read_routed",
            Stage::MetaResolved => "meta_resolved",
            Stage::KeysResolved => "keys_resolved",
            Stage::TierProbed => "tier_probed",
            Stage::CohortServed => "cohort_served",
            Stage::BlockFetched => "block_fetched",
            Stage::BindingChecked => "binding_checked",
            Stage::ServeCopied => "serve_copied",
            Stage::ReadValidated => "read_validated",
            Stage::ReadReturn => "read_return",
            Stage::DevSubmit => "dev_submit",
            Stage::DevComplete => "dev_complete",
            Stage::FetchDone => "fetch_done",
            Stage::Decoded => "decoded",
            Stage::Admitted => "admitted",
            Stage::Deposited => "deposited",
            Stage::FillDone => "fill_done",
            Stage::BridgeSent => "bridge_sent",
            Stage::BridgeTaken => "bridge_taken",
            Stage::WriteAdmitted => "write_admitted",
            Stage::WriteDetached => "write_detached",
            Stage::WriteLocked => "write_locked",
            Stage::WriteEncoded => "write_encoded",
            Stage::WriteAllocated => "write_allocated",
            Stage::WriteDmaDone => "write_dma_done",
            Stage::WritePublished => "write_published",
            Stage::WriteFreed => "write_freed",
            Stage::WriteInvalDone => "write_inval_done",
            Stage::WriteDone => "write_done",
            Stage::PublishEnqueue => "publish_enqueue",
            Stage::PublishDrained => "publish_drained",
            Stage::PublishLocked => "publish_locked",
            Stage::PublishBaseReady => "publish_base_ready",
            Stage::PublishApplied => "publish_applied",
            Stage::PublishEncoded => "publish_encoded",
            Stage::PublishBlobWritten => "publish_blob_written",
            Stage::PublishCommitted => "publish_committed",
            Stage::CommitGuarded => "commit_guarded",
            Stage::CommitInodeRead => "commit_inode_read",
            Stage::CommitSlotProbed => "commit_slot_probed",
            Stage::CommitTxDone => "commit_tx_done",
            Stage::PublishDone => "publish_done",
            Stage::MetaEnqueue => "meta_enqueue",
            Stage::PassBegin => "pass_begin",
            Stage::PassAdmitted => "pass_admitted",
            Stage::PassLocked => "pass_locked",
            Stage::JournalWritten => "journal_written",
            Stage::JournalPrefixDone => "journal_prefix_done",
            Stage::Barrier => "barrier",
            Stage::PassEnd => "pass_end",
            Stage::Fanout => "fanout",
            Stage::MetaBackendStart => "meta_backend_start",
            Stage::MetaBackendDone => "meta_backend_done",
            Stage::MetaOpReturn => "meta_op_return",
            Stage::IpcIngress => "ipc_ingress",
            Stage::IpcDequeue => "ipc_dequeue",
            Stage::IpcAdmitted => "ipc_admitted",
            Stage::IpcSqEnter => "ipc_sq_enter",
            Stage::IpcCqe => "ipc_cqe",
            Stage::IpcComplete => "ipc_complete",
            Stage::StripeLockAcquired => "stripe_lock_acquired",
            Stage::StripeLockReleased => "stripe_lock_released",
            Stage::DlmGuardAcquired => "dlm_guard_acquired",
            Stage::DlmGuardReleased => "dlm_guard_released",
        }
    }

    /// The stage for a wire id (`None` for anything not in [`Self::ALL`]).
    pub fn from_u16(id: u16) -> Option<Stage> {
        Stage::ALL.iter().copied().find(|s| *s as u16 == id)
    }
}

/// One trace sample (the drain's unit; the ring slot is its three
/// atomic words).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub op_id: u64,
    pub stage: u16,
    /// CLOCK_MONOTONIC nanoseconds.
    pub mono_ns: u64,
}

/// One ring slot: `[op_id, stage, mono_ns]` as plain relaxed words —
/// the head/tail edges order them (see the module doc).
type Slot = [AtomicU64; 3];

#[cfg_attr(not(loom), repr(align(64)))]
struct Cursor(AtomicUsize);

/// A single-producer / single-consumer trace ring (see the module doc).
/// Capacity is a power of two; `head`/`tail` are monotonic counters
/// (wrapping arithmetic — a lap is `head - tail`).
pub struct TraceRing {
    mask: usize,
    slots: Box<[Slot]>,
    /// Producer cursor: the owning thread's next slot.
    head: Cursor,
    /// Consumer cursor: the drain's next slot.
    tail: Cursor,
}

impl TraceRing {
    /// A ring of `capacity` slots (rounded UP to a power of two, minimum
    /// 2), all allocated here — never on the push path.
    pub fn with_capacity(capacity: usize) -> Self {
        let cap = capacity.max(2).next_power_of_two();
        let slots: Box<[Slot]> = (0..cap)
            .map(|_| [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)])
            .collect();
        Self {
            mask: cap - 1,
            slots,
            head: Cursor(AtomicUsize::new(0)),
            tail: Cursor(AtomicUsize::new(0)),
        }
    }

    /// Slots.
    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// PRODUCER (the owning thread only): append `s`; `false` = the ring
    /// is full and the sample was DROPPED (the caller counts it). Never
    /// blocks, never allocates.
    #[inline]
    pub fn push(&self, s: Sample) -> bool {
        let head = self.head.0.load(Ordering::Relaxed);
        // Acquire: the consumer's slot reads (before its Release tail
        // store) happen-before this slot's overwrite.
        let tail = self.tail.0.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.capacity() {
            return false;
        }
        let slot = &self.slots[head & self.mask];
        slot[0].store(s.op_id, Ordering::Relaxed);
        slot[1].store(u64::from(s.stage), Ordering::Relaxed);
        slot[2].store(s.mono_ns, Ordering::Relaxed);
        // Release: the three words above are visible to a consumer that
        // Acquire-loads this head.
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// CONSUMER (the drain — one thread at a time): move every published
    /// sample into `out`, freeing the slots.
    pub fn drain_into(&self, out: &mut Vec<Sample>) {
        let head = self.head.0.load(Ordering::Acquire);
        let mut tail = self.tail.0.load(Ordering::Relaxed);
        while tail != head {
            let slot = &self.slots[tail & self.mask];
            out.push(Sample {
                op_id: slot[0].load(Ordering::Relaxed),
                stage: slot[1].load(Ordering::Relaxed) as u16,
                mono_ns: slot[2].load(Ordering::Relaxed),
            });
            tail = tail.wrapping_add(1);
        }
        // Release: the reads above complete before the producer (Acquire
        // on tail) may overwrite these slots.
        self.tail.0.store(tail, Ordering::Release);
    }

    /// Samples published and not yet drained (diagnostic).
    pub fn len(&self) -> usize {
        self.head
            .0
            .load(Ordering::Acquire)
            .wrapping_sub(self.tail.0.load(Ordering::Relaxed))
    }

    /// `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The threshold value meaning "every op" (divisor 1).
pub const SELECT_ALL: u32 = u32::MAX;

/// The selection threshold for a sampling divisor `N` (≈ 1 in N ops).
pub const fn select_threshold(divisor: u32) -> u32 {
    if divisor <= 1 {
        SELECT_ALL
    } else {
        u32::MAX / divisor
    }
}

/// The sampling law: whether `op_id` is traced under `threshold`.
/// Fibonacci hashing (the 64-bit golden-ratio multiplier) of the id,
/// upper 32 bits against the threshold — deterministic per id (every
/// hook on one op agrees), stride-independent, one multiply.
#[inline]
pub const fn selected(op_id: u64, threshold: u32) -> bool {
    if threshold == SELECT_ALL {
        return true;
    }
    let h = op_id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
    (h as u32) < threshold
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn ring_round_trips_in_order_and_reports_full() {
        let r = TraceRing::with_capacity(4);
        assert_eq!(r.capacity(), 4);
        for i in 0..4u64 {
            assert!(r.push(Sample {
                op_id: i,
                stage: 1,
                mono_ns: 100 + i
            }));
        }
        assert!(
            !r.push(Sample {
                op_id: 9,
                stage: 1,
                mono_ns: 0
            }),
            "a full ring drops"
        );
        assert_eq!(r.len(), 4);
        let mut out = Vec::new();
        r.drain_into(&mut out);
        assert_eq!(
            out.iter().map(|s| s.op_id).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert!(r.is_empty());
        // Wraps: another lap lands in the freed slots.
        for i in 10..14u64 {
            assert!(r.push(Sample {
                op_id: i,
                stage: 2,
                mono_ns: i
            }));
        }
        out.clear();
        r.drain_into(&mut out);
        assert_eq!(
            out.iter().map(|s| s.op_id).collect::<Vec<_>>(),
            [10, 11, 12, 13]
        );
    }

    #[test]
    fn capacity_rounds_up_to_a_power_of_two() {
        assert_eq!(TraceRing::with_capacity(0).capacity(), 2);
        assert_eq!(TraceRing::with_capacity(5).capacity(), 8);
        assert_eq!(TraceRing::with_capacity(64).capacity(), 64);
    }

    #[test]
    fn selection_is_deterministic_and_about_one_in_n() {
        let t = select_threshold(16);
        let n = (1..=16_000u64).filter(|id| selected(*id, t)).count();
        assert!((700..=1_300).contains(&n), "{n} of 16000 at N=16");
        let even = (1..=16_000u64)
            .map(|k| k * 2)
            .filter(|id| selected(*id, t))
            .count();
        assert!((700..=1_300).contains(&even), "{even} of 16000 even ids");
        assert!(selected(123, SELECT_ALL));
        assert_eq!(select_threshold(1), SELECT_ALL);
        assert_eq!(select_threshold(0), SELECT_ALL);
    }

    #[test]
    fn stage_ids_are_unique_and_named() {
        let mut ids = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        for s in Stage::ALL {
            assert!(ids.insert(*s as u16));
            assert!(names.insert(s.name()));
            assert_eq!(Stage::from_u16(*s as u16), Some(*s));
        }
        assert_eq!(Stage::from_u16(0), None);
    }
}
