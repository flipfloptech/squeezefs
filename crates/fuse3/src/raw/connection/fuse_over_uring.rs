//! Kernel **FUSE-over-io_uring** (Linux 6.14+ / 7.x) — `linux/fuse.h` + libfuse `fuse_uring.c`.
//!
//! **Required** request transport for the hot path. No userspace opt-out, no
//! classical fallback after arm. Mount fails if setup fails. The kernel module
//! parameter `fuse.enable_uring` must be Y (we try to enable it at start).
//!
//! Classical `/dev/fuse` reads exist in exactly two places, both **via
//! io_uring `Readv`** (`BlockFuseConnection`):
//!
//! 1. The single `FUSE_INIT` exchange — the kernel rejects `REGISTER` until
//!    `fch->initialized`.
//! 2. The post-arm **classical sideband session**: the kernel keeps
//!    FORGET/BATCH_FORGET (`fuse_io_uring_ops.send_forget`) and INTERRUPT
//!    (`.send_interrupt`) on the classical queue even with the ring armed,
//!    `fuse_resend` splices resends onto the classical `fiq->pending`, and
//!    regular requests can land classically in the unlocked
//!    `WRITE_ONCE(fiq->ops, …)` switchover window (fs/fuse/dev_uring.c,
//!    v6.14–v7.1). Without a permanent reader those strand forever —
//!    `fusectl waiting ≥ 1`, syncfs blocks, umount EBUSY (the storm-teardown
//!    stuck-request wedge). All *other* request traffic stays over-uring.
//!
//! Tuning only:
//! ```text
//! SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH=N     # optional, per-queue depth (clamp
//!                                            # 1..32); wins verbatim over the
//!                                            # payload-buffer budget. Default =
//!                                            # the L1 policy: desired 32,
//!                                            # degraded to the buffer cap,
//!                                            # floor 4 (see TransportGeometry)
//! SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES=N      # testing only, default = kernel
//!                                            # possible CPUs (clamp 1..512);
//!                                            # fewer than possible CPUs never
//!                                            # becomes ready
//! SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS=N   # optional (§5.3 D3.b): SQPOLL the
//!                                            # queue rings — ONE shared kernel
//!                                            # poller (qid 0 leader, ATTACH_WQ
//!                                            # followers), idle timeout N ms.
//!                                            # Default off = plain rings.
//! SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU=C       # optional: pin that one poller
//!                                            # (leader pin governs the group)
//! SQUEEZEFS_TRANSPORT_DEBUG=1                # per-request transport tracing to
//!                                            # stderr (stuck-request forensics)
//! ```
//!
//! # Design (hardened)
//! - **Per-qid commit channel** — no shared demux / re-queue races.
//! - **Shared inbound work queue** + condvar — multi-queue session workers all pop requests.
//! - **eventfd** per queue — wake workers on commit / shutdown (no busy poll).
//! - Pool is **not** marked ready until every queue has submitted REGISTER — avoids
//!   a deadlock where the session stops reading classical `/dev/fuse` while the
//!   kernel has not yet switched to the uring path.

#![cfg(all(target_os = "linux", feature = "tokio-runtime"))]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use io_uring::squeue::Entry128;
use io_uring::{cqueue, opcode, squeue, types, IoUring};
use tracing::{debug, error, info, warn};

/// Payload-lease re-arm protocol core (refs/parked publish-then-recheck).
/// `#[path]`-included so `loom-models` can model-check the exact shipped
/// code (SqueezeFS zero-copy write-path design §5.4, house extracted-core
/// convention). `pub` (MEM-1): the SqueezeFS root suite composes the
/// dest-ownership tests against the REAL gate type
/// (`tests/nvme_dest_ownership_tests.rs`).
#[path = "lease_core.rs"]
pub mod lease_core;
use lease_core::{CommitGate, EntLeaseState};

/// Queue-worker eventfd wake-coalescing core (L3 transport-economy lever
/// B) — same `#[path]`-included-by-`loom-models` convention.
#[path = "wake_core.rs"]
mod wake_core;
use wake_core::WakeCoalescer;

use super::kmbuf::{self, KmbufQueue, TransportBufferMode};
use crate::raw::request::ReplySlot;

/// `FUSE_OVER_IO_URING` (1ULL<<41) → `flags2` bit 9.
pub const FUSE_OVER_IO_URING_FLAGS2: u32 = 1u32 << 9;

pub const FUSE_URING_IN_OUT_HEADER_SZ: usize = 128;
pub const FUSE_URING_OP_IN_OUT_SZ: usize = 128;

const FUSE_IO_URING_CMD_REGISTER: u32 = 1;
const FUSE_IO_URING_CMD_COMMIT_AND_FETCH: u32 = 2;
const FUSE_IN_HEADER_SIZE: usize = 40;
/// `linux/fuse.h` opcode 16 — the only opcode whose payload rides a lease.
const FUSE_WRITE_OPCODE: u32 = crate::raw::abi::fuse_opcode::FUSE_WRITE as u32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct FuseUringEntInOut {
    flags: u64,
    commit_id: u64,
    payload_sz: u32,
    padding: u32,
    reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FuseUringReqHeader {
    in_out: [u8; FUSE_URING_IN_OUT_HEADER_SZ],
    op_in: [u8; FUSE_URING_OP_IN_OUT_SZ],
    ring_ent_in_out: FuseUringEntInOut,
}

impl Default for FuseUringReqHeader {
    fn default() -> Self {
        Self {
            in_out: [0; FUSE_URING_IN_OUT_HEADER_SZ],
            op_in: [0; FUSE_URING_OP_IN_OUT_SZ],
            ring_ent_in_out: FuseUringEntInOut::default(),
        }
    }
}

/// `struct fuse_uring_cmd_req` (24 bytes). The carried kmbuf/zc series
/// re-purposes 4 of the historical 6 padding bytes as the REGISTER-time
/// `init` union (`{ u16 flags; u16 queue_depth; }` — uapi "7.46" comment,
/// minor stays 45): zeros on COMMIT_AND_FETCH and on pre-series kernels,
/// so the struct is wire-compatible in both directions.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FuseUringCmdReq {
    flags: u64,
    commit_id: u64,
    qid: u16,
    /// REGISTER `init.flags` (`FUSE_URING_BUF_RING` / `FUSE_URING_ZERO_COPY`).
    init_flags: u16,
    /// REGISTER `init.queue_depth` (zc arm only).
    init_queue_depth: u16,
    padding: [u8; 2],
}

/// Request delivered to the session as if read from `/dev/fuse`.
#[derive(Debug)]
pub struct InboundUringReq {
    /// `fuse_in_header` || per-op header (`op_in`).
    pub header_and_op: Vec<u8>,
    pub payload: Bytes,
    /// FUSE request unique (also embedded in `header_and_op`).
    #[allow(dead_code)]
    pub unique: u64,
    /// The ring slot this request was delivered on — the address its
    /// reply commits against (FUSE-2 ⊕ PERF-16: the request carries its
    /// reply address, so the `unique → slot` map is deleted, not
    /// wrapped).
    pub slot: ReplySlot,
    /// Reap stamp (transport epoch ns, `read_phase::transport_now_ns`) —
    /// anchors `read_transport_phase_ns`'s `queue_wait`/`transport_total`.
    pub arrived_ns: u64,
}

pub(crate) struct CommitMsg {
    ent_idx: u16,
    commit_id: u64,
    header: Vec<u8>,
    reply_body: Bytes,
}

/// One slot's liveness cell, published by its queue worker and read by
/// the watch thread (FUSE-2's ungated stale-slot watchdog — the
/// `transport_debug`-gated stale-pending scan it replaces was off in
/// production, which is why nine of the eleven lost-reply paths had no
/// detector at all).
#[derive(Default)]
struct SlotWatch {
    /// Unique of the request the slot owes a reply for; 0 = owes nothing.
    unique: AtomicU64,
    /// Transport-epoch ns of the delivery (`transport_now_ns`).
    since_ns: AtomicU64,
}

impl SlotWatch {
    /// Publish "this slot owes a reply for `unique`" (two relaxed stores
    /// — the whole cost of the observation side-channel that replaced a
    /// sharded-mutex map insert).
    #[inline]
    fn set(&self, unique: u64, now_ns: u64) {
        self.since_ns.store(now_ns, Ordering::Relaxed);
        self.unique.store(unique, Ordering::Release);
    }

    /// Publish "this slot owes nothing" (its commit was submitted).
    #[inline]
    fn clear(&self) {
        self.unique.store(0, Ordering::Release);
    }
}

/// Watchdog cadence and the age at which an owed reply is called out.
/// Deliberately generous: the point is to name a WEDGE loudly, not to
/// second-guess a slow-but-live handler (the deadline watchdog for
/// per-op latency lives in the daemon — D1.b).
const SLOT_WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
const SLOT_OVERDUE_NS: u64 = 5_000_000_000;

// ---------------------------------------------------------------------------
// FUSE-2 ⊕ PERF-16 — the per-`(qid, ent_idx)` slot state machine
// ---------------------------------------------------------------------------

/// **The exactly-one-reply invariant** (pre-RC spec FUSE-2):
///
/// > for every request delivered on a ring slot, exactly one
/// > COMMIT_AND_FETCH carrying either a reply or a synthesized error is
/// > submitted before the slot leaves [`SlotState::Delivered`], and the
/// > slot leaves that state only by that submission.
///
/// A lost reply parks the calling application in uninterruptible sleep
/// and makes `umount` return EBUSY, so every non-reply exit routes
/// through [`SlotTable::fail_ent`] instead of dropping the request.
///
/// **Ownership.** One [`SlotTable`] per queue, owned *exclusively* by
/// that queue's worker thread — every transition runs on the worker, so
/// the machine needs no synchronization and no loom model (the
/// single-owner claim is debug-asserted in [`SlotTable::assert_owner`]).
/// Other threads address a slot by value (`(qid, ent_idx, commit_id)`
/// carried in the request) and post to the queue's commit channel; they
/// never touch the state. The historical sharded `unique → slot` map is
/// deleted, not wrapped: there is no map to miss (row 4) or collide in
/// (row 10), and the reply path pays no mutex (PERF-16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotState {
    /// REGISTER submitted; the kernel owns the ent and no request is out.
    Registered,
    /// A request was handed to the session. Exactly one commit must follow.
    Delivered { unique: u64, commit_id: u64 },
    /// A commit for the delivered request is parked behind a live payload
    /// lease (§5.4 re-arm gate). Still owed exactly one commit.
    Parked { unique: u64, commit_id: u64 },
    /// COMMIT_AND_FETCH submitted; the kernel owns the ent again (its CQE
    /// brings the next delivery).
    Replied { commit_id: u64 },
    /// Retired after `REGISTER_RETRY_MAX` consecutive REGISTER failures
    /// (FUSE-3a) — the ent is out of the queue's rotation.
    Retired,
}

/// Outcome of a kernel delivery on a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliverOutcome {
    /// Normal delivery — the slot now owes exactly one commit.
    Accepted,
    /// FUSE-2 row 10: the kernel delivered onto a slot that still owes a
    /// reply (the `pending.insert` overwrite class, and the `fuse_resend`
    /// double-delivery shape). The displaced request can no longer be
    /// answered — its commit id is gone — so it is counted on the
    /// must-stay-0 `transport_requests_abandoned` tripwire and logged
    /// loudly; the new delivery is accepted.
    DisplacedRequest { unique: u64 },
}

/// Verdict on a `CommitMsg` arriving for a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitAdmit {
    /// The commit addresses the request this slot currently owes.
    Accept,
    /// Double reply, late reply, or a reply for a request the slot no
    /// longer holds (FUSE-3e: never an overwrite — the first commit
    /// stands, the second is refused loud-never-fatally).
    RefuseStale,
}

/// What [`SlotTable::fail_ent`] wants the worker to submit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailOutcome {
    /// Submit a header-only `-errno` COMMIT_AND_FETCH for this request.
    Synthesize { unique: u64, commit_id: u64 },
    /// The slot owed nothing (already replied / never delivered).
    Nothing,
}

/// FUSE-3a: what to do after a REGISTER CQE error on a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegisterAction {
    /// Re-REGISTER after `after` (bounded exponential backoff).
    Retry { after: Duration },
    /// `REGISTER_RETRY_MAX` consecutive failures — retire the ent.
    Retire,
}

/// FUSE-3a: consecutive REGISTER failures tolerated before an ent is
/// retired out of the queue's rotation.
pub(crate) const REGISTER_RETRY_MAX: u32 = 8;
/// FUSE-3a: first backoff step; doubles per failure up to
/// [`REGISTER_BACKOFF_MAX`].
pub(crate) const REGISTER_BACKOFF_BASE: Duration = Duration::from_millis(1);
/// FUSE-3a: backoff ceiling (a wedged ent must not spin a core, and must
/// not take minutes to recover either).
pub(crate) const REGISTER_BACKOFF_MAX: Duration = Duration::from_millis(128);

/// Per-queue slot states — see [`SlotState`] for the invariant and the
/// single-owner rule.
pub(crate) struct SlotTable {
    states: Vec<SlotState>,
    register_failures: Vec<u32>,
    /// Earliest instant a retried REGISTER may be re-pushed (FUSE-3a).
    retry_at: Vec<Option<Instant>>,
    /// Debug-only ownership proof (single-owner by construction).
    #[cfg(debug_assertions)]
    owner: std::thread::ThreadId,
}

impl SlotTable {
    pub(crate) fn new(depth: usize) -> Self {
        Self {
            states: vec![SlotState::Registered; depth],
            register_failures: vec![0; depth],
            retry_at: vec![None; depth],
            #[cfg(debug_assertions)]
            owner: std::thread::current().id(),
        }
    }

    /// Single-owner proof: every transition runs on the worker thread that
    /// built the table (that is what makes the machine lock-free and
    /// loom-model-free).
    #[inline]
    fn assert_owner(&self) {
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            self.owner,
            std::thread::current().id(),
            "SlotTable is single-owner (its queue worker); a foreign thread \
             mutated slot state"
        );
    }

    pub(crate) fn state(&self, ent: usize) -> SlotState {
        self.states[ent]
    }

    /// A kernel delivery landed on `ent`.
    pub(crate) fn on_deliver(&mut self, ent: usize, unique: u64, commit_id: u64) -> DeliverOutcome {
        self.assert_owner();
        let displaced = match self.states[ent] {
            SlotState::Delivered { unique: prev, .. } | SlotState::Parked { unique: prev, .. } => {
                // The kernel handed us a new request on a slot whose
                // previous request we never answered. Its commit id is
                // gone with it, so no commit can be submitted for it —
                // this is the tripwire's honest case.
                TRANSPORT_REQUESTS_ABANDONED.fetch_add(1, Ordering::Relaxed);
                Some(prev)
            }
            _ => None,
        };
        self.register_failures[ent] = 0;
        self.retry_at[ent] = None;
        self.states[ent] = SlotState::Delivered { unique, commit_id };
        match displaced {
            Some(unique) => DeliverOutcome::DisplacedRequest { unique },
            None => DeliverOutcome::Accepted,
        }
    }

    /// A delivery with `unique == 0` (the kernel filled only `commit_id`):
    /// there is no request to serve, but the ent must still be committed
    /// or it stays in USERSPACE forever (`waiting ≥ 1`, umount EBUSY).
    /// Recorded as an owed slot so the forced EIO commit below is the
    /// state machine's normal exit rather than a special case.
    pub(crate) fn on_deliver_degenerate(&mut self, ent: usize, commit_id: u64) {
        self.assert_owner();
        self.states[ent] = SlotState::Delivered {
            unique: 0,
            commit_id,
        };
    }

    /// A `CommitMsg` for `ent` arrived on the queue's commit channel.
    pub(crate) fn admit_commit(&self, ent: usize, commit_id: u64) -> CommitAdmit {
        self.assert_owner();
        match self.states[ent] {
            SlotState::Delivered {
                commit_id: owed, ..
            } if owed == commit_id => CommitAdmit::Accept,
            // Everything else is a reply the slot does not owe: a second
            // reply for one request (FUSE-3e — the release-build silent
            // overwrite), a reply that lost the race with a teardown
            // drain or a synthesized error, or a reply addressed at a
            // request the slot no longer holds.
            _ => {
                TRANSPORT_REPLIES_REFUSED_STALE.fetch_add(1, Ordering::Relaxed);
                CommitAdmit::RefuseStale
            }
        }
    }

    /// The admitted commit had to park behind a live payload lease.
    pub(crate) fn on_commit_parked(&mut self, ent: usize) {
        self.assert_owner();
        if let SlotState::Delivered { unique, commit_id } = self.states[ent] {
            self.states[ent] = SlotState::Parked { unique, commit_id };
        }
    }

    /// A COMMIT_AND_FETCH SQE for `ent` was pushed onto the ring — the
    /// slot's ONE exit from owing a reply.
    pub(crate) fn on_commit_submitted(&mut self, ent: usize, commit_id: u64) {
        self.assert_owner();
        self.states[ent] = SlotState::Replied { commit_id };
    }

    /// Row 6: a transient COMMIT failure is about to be re-committed
    /// (the ent still holds its applied reply, so this is a re-push of
    /// the SAME commit, never a re-REGISTER).
    pub(crate) fn on_commit_retry(&mut self, ent: usize) {
        self.assert_owner();
        if let SlotState::Replied { commit_id } = self.states[ent] {
            self.states[ent] = SlotState::Delivered {
                unique: 0,
                commit_id,
            };
        }
    }

    /// A REGISTER SQE for `ent` was pushed onto the ring.
    pub(crate) fn on_register_submitted(&mut self, ent: usize) {
        self.assert_owner();
        // Re-REGISTERing a slot that still owes a reply IS the loss row 5
        // describes; the caller must `fail_ent` first. Counting it here
        // makes an omission visible instead of silent.
        if matches!(
            self.states[ent],
            SlotState::Delivered { .. } | SlotState::Parked { .. }
        ) {
            TRANSPORT_REQUESTS_ABANDONED.fetch_add(1, Ordering::Relaxed);
        }
        self.retry_at[ent] = None;
        self.states[ent] = SlotState::Registered;
    }

    /// Every non-reply exit routes here (FUSE-2's single `fail_ent`
    /// helper): CQE-error reclaim, a failed inbound push, a refused
    /// delivery, teardown drain.
    /// The caller MUST submit the returned commit (or call
    /// [`Self::abandon`] if it cannot): the slot is moved out of
    /// `Delivered` here so a second `fail_ent` on the same slot can
    /// never produce a second commit.
    pub(crate) fn fail_ent(&mut self, ent: usize) -> FailOutcome {
        self.assert_owner();
        match self.states[ent] {
            SlotState::Delivered { unique, commit_id }
            | SlotState::Parked { unique, commit_id } => {
                TRANSPORT_REQUESTS_FAILED_SYNTHETIC.fetch_add(1, Ordering::Relaxed);
                self.states[ent] = SlotState::Replied { commit_id };
                FailOutcome::Synthesize { unique, commit_id }
            }
            _ => FailOutcome::Nothing,
        }
    }

    /// The request on this slot can never be answered (no commit id to
    /// address, or the ring is gone): count it on the must-stay-0
    /// tripwire.
    pub(crate) fn abandon(&mut self, ent: usize) {
        self.assert_owner();
        if matches!(
            self.states[ent],
            SlotState::Delivered { .. } | SlotState::Parked { .. }
        ) {
            TRANSPORT_REQUESTS_ABANDONED.fetch_add(1, Ordering::Relaxed);
        }
        self.states[ent] = SlotState::Registered;
    }

    /// FUSE-3a: a REGISTER CQE for `ent` failed with a non-fatal errno.
    pub(crate) fn note_register_failure(&mut self, ent: usize, now: Instant) -> RegisterAction {
        self.assert_owner();
        self.register_failures[ent] += 1;
        if self.register_failures[ent] >= REGISTER_RETRY_MAX {
            self.states[ent] = SlotState::Retired;
            self.retry_at[ent] = None;
            TRANSPORT_ENTS_RETIRED.fetch_add(1, Ordering::Relaxed);
            return RegisterAction::Retire;
        }
        let shift = self.register_failures[ent] - 1;
        let after = REGISTER_BACKOFF_BASE
            .checked_mul(1u32 << shift.min(16))
            .unwrap_or(REGISTER_BACKOFF_MAX)
            .min(REGISTER_BACKOFF_MAX);
        self.retry_at[ent] = Some(now + after);
        RegisterAction::Retry { after }
    }

    /// FUSE-3a: a REGISTER for `ent` completed successfully.
    pub(crate) fn note_register_success(&mut self, ent: usize) {
        self.assert_owner();
        self.register_failures[ent] = 0;
        self.retry_at[ent] = None;
    }

    /// FUSE-3a: ents whose backoff has expired and that are due a
    /// re-REGISTER push (consumes the deadline).
    pub(crate) fn register_retries_due(&mut self, now: Instant) -> Vec<usize> {
        self.assert_owner();
        let mut due = Vec::new();
        for ent in 0..self.retry_at.len() {
            if matches!(self.retry_at[ent], Some(t) if t <= now) {
                self.retry_at[ent] = None;
                due.push(ent);
            }
        }
        due
    }

    /// FUSE-3a: the soonest outstanding REGISTER retry, which bounds the
    /// worker's ring wait (an idle queue gets no CQE to wake it).
    pub(crate) fn next_retry_deadline(&self) -> Option<Instant> {
        self.retry_at.iter().flatten().min().copied()
    }

    /// FUSE-3a: the whole queue has retired — the session cannot serve.
    pub(crate) fn all_retired(&self) -> bool {
        !self.states.is_empty() && self.states.iter().all(|s| *s == SlotState::Retired)
    }

    /// Slots still owing a reply, for the teardown drain and the
    /// stale-slot watchdog.
    pub(crate) fn owing(&self) -> impl Iterator<Item = (usize, u64, u64)> + '_ {
        self.states
            .iter()
            .enumerate()
            .filter_map(|(idx, st)| match st {
                SlotState::Delivered { unique, commit_id }
                | SlotState::Parked { unique, commit_id } => Some((idx, *unique, *commit_id)),
                _ => None,
            })
    }
}

/// FUSE-2 row 6 — the ring-op class carried in `user_data`.
///
/// `user_data == ent_idx` for BOTH REGISTER and COMMIT_AND_FETCH today,
/// so an `EAGAIN` CQE cannot be attributed: the worker re-REGISTERs and
/// discards a reply `apply_reply` already wrote into the ent (the code
/// documents the hazard 60 lines below the bug). Tagging the op class
/// makes the two distinguishable, so an EAGAIN'd COMMIT is re-committed
/// (the ent still holds its applied reply) instead of re-REGISTERed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RingOp {
    Register,
    Commit,
}

/// `user_data` reserved for the wake-fd PollAdd (unchanged).
pub(crate) const UD_POLL: u64 = u64::MAX;

/// Op-class tag in the high half of `user_data` (the low half carries
/// the ent index; ring depth is clamped to 32 by knob, so 32 bits of
/// index is unbounded headroom).
const UD_TAG_SHIFT: u32 = 32;
const UD_TAG_REGISTER: u64 = 1;
const UD_TAG_COMMIT: u64 = 2;

/// Encode `(op, ent_idx)` into an SQE `user_data` word.
#[inline]
pub(crate) fn encode_user_data(op: RingOp, ent_idx: usize) -> u64 {
    let tag = match op {
        RingOp::Register => UD_TAG_REGISTER,
        RingOp::Commit => UD_TAG_COMMIT,
    };
    (tag << UD_TAG_SHIFT) | (ent_idx as u64 & 0xFFFF_FFFF)
}

/// Decode a CQE `user_data` word — `None` for the poll marker.
#[inline]
pub(crate) fn decode_user_data(user_data: u64) -> Option<(RingOp, usize)> {
    if user_data == UD_POLL {
        return None;
    }
    let op = match user_data >> UD_TAG_SHIFT {
        UD_TAG_REGISTER => RingOp::Register,
        UD_TAG_COMMIT => RingOp::Commit,
        // A word this worker never pushed (kernel echo of an unknown op
        // class): treat as a REGISTER completion — the historical
        // reading — rather than dropping the CQE.
        _ => RingOp::Register,
    };
    Some((op, (user_data & 0xFFFF_FFFF) as usize))
}

struct QueueHandle {
    /// Unbounded: a bounded sync_channel can block the session reply task if the
    /// queue worker is briefly not draining, freezing *all* fuse replies.
    commit_tx: std::sync::mpsc::Sender<CommitMsg>,
    /// Wake the queue thread (commit or shutdown).
    wake_fd: RawFd,
    /// Keep OwnedFd alive.
    _wake: OwnedFd,
    /// L3 lever B: elides redundant `wake_fd` writes — N reply submissions
    /// between two worker passes cost one eventfd write. Shared with the
    /// queue's [`PayloadArena`] so lease drops elide through the same flag.
    wake_coalescer: Arc<WakeCoalescer>,
    /// The queue's payload arena, set once by the worker at startup. Held
    /// here so payload pointers handed out via `get_payload_buffer` stay
    /// valid for the pool's whole life, even after the worker exited.
    arena: std::sync::Mutex<Option<Arc<PayloadArena>>>,
    /// kmbuf mode (2026-08-04): the queue's kernel-managed-buffer
    /// resources — `get_payload_buffer` serves the ent's ATTACHED buffer
    /// through this (attachments are per-delivery, not per-ent-static).
    kmbuf: std::sync::Mutex<Option<Arc<KmbufQueue>>>,
    /// Per-ent §5.4 lease states — pool-level (not worker-local) so
    /// [`FuseOverUring::lease_dest_window`] can acquire DMA-destination
    /// leases (MEM-1) against the very words the worker's commit gate
    /// checks. The worker clones this Vec once at startup.
    lease_states: Vec<Arc<EntLeaseState>>,
    /// The queue's dest-claim geometry (MEM-1 address-containment
    /// resolution), set once by the worker right after its arena exists.
    dest_window: std::sync::OnceLock<DestWindow>,
}

/// One queue's dest-claim geometry (MEM-1): the arena span, its buffer
/// stride, and the keep-alives a claim token needs.
struct DestWindow {
    base: usize,
    span: usize,
    stride: usize,
    arena: Arc<PayloadArena>,
    /// kmbuf mode: window buffer indices are BIDs — the ent currently
    /// attached to the bid owns the lease word. Attachment is stable
    /// across the claim window: the kernel re-points/recycles only at
    /// fetch (triggered by our COMMIT_AND_FETCH), and a claim happens
    /// strictly before the claiming request's reply can exist.
    kmbuf: Option<Arc<KmbufQueue>>,
}

impl DestWindow {
    /// True when `[addr, addr + len)` lies inside this arena span.
    fn contains(&self, addr: usize, len: usize) -> bool {
        self.stride != 0
            && addr >= self.base
            && addr
                .checked_add(len)
                .is_some_and(|end| end <= self.base + self.span)
    }
}

/// Pure MEM-1 window math: the buffer index `[addr, addr + len)` occupies
/// inside a `(base, stride)` arena, or `None` when the window straddles
/// two buffers (never a valid single-request dest — refused rather than
/// mis-leased). Caller has already proven containment.
fn dest_window_index(base: usize, stride: usize, addr: usize, len: usize) -> Option<usize> {
    debug_assert!(stride > 0 && len > 0 && addr >= base);
    let off = addr - base;
    let idx = off / stride;
    ((off + len - 1) / stride == idx).then_some(idx)
}

/// Owns every registered payload buffer of one queue plus a dup of the
/// queue eventfd (§5.4). Payload allocations live here — not in the
/// worker-local `Ent` — so a payload lease outliving the worker (shutdown
/// with a pathological handler) keeps pointing at valid memory, and the
/// wake fd a late lease drop writes can never be closed/reused underneath
/// it. A leaked lease degrades to a leaked buffer, never a dangling
/// pointer.
struct PayloadArena {
    /// One anonymous mmap span carrying every ent's payload buffer at a
    /// 4 KiB-aligned stride (NUMA-affinity campaign 2026-07-31 — the
    /// near-zero-copy note's OQ-3 shape): a single vma lets the arena be
    /// node-BOUND before first touch (`mbind` + populate when placement
    /// is active) and `MADV_HUGEPAGE`d as one range, and its pages'
    /// ACTUAL nodes are queried once per buffer for the locality
    /// instrument. Stored as `usize` (stable for the arena's life).
    ///
    /// kmbuf mode (2026-08-04): the span is the QUEUE's mmap'd
    /// kernel-managed buffer region instead (bid-indexed buffers) —
    /// owned by [`KmbufQueue`] and held alive here via `kmbuf`, never
    /// unmapped by this drop.
    base: usize,
    span: usize,
    /// Buffer stride inside the span (ent-indexed classical; bid-indexed
    /// kmbuf).
    stride: usize,
    /// Per-buffer bases inside the span (stride-spaced).
    bufs: Vec<usize>,
    /// Dense node index each buffer's first page ACTUALLY landed on
    /// (queried post-placement — the instrument's memory-node source;
    /// `None` = query failed / unmapped node).
    buf_nodes: Vec<Option<usize>>,
    /// dup(2) of the queue eventfd: lease drops wake the worker through the
    /// arena so the fd is alive exactly as long as any lease can write it.
    wake: OwnedFd,
    /// The queue's wake-elision flag (shared with [`QueueHandle`]): lease
    /// drops arm it before writing `wake` (L3 lever B).
    wake_coalescer: Arc<WakeCoalescer>,
    /// kmbuf mode: the region owner (mapping liveness for leases); also
    /// the owns-the-mapping discriminant for `Drop`.
    kmbuf: Option<Arc<KmbufQueue>>,
}

impl PayloadArena {
    /// `node` = the queue's intended NUMA node (`node_of_cpu(qid)` on
    /// queue-per-possible-CPU sessions; `None` = no placement intent —
    /// testing queue overrides, single-node maps, `SQUEEZEFS_NUMA=0`).
    /// Placement is best-effort: a refused bind leaves a fully
    /// functional arena, and the instrument reports where pages REALLY
    /// landed either way.
    fn new(
        depth: usize,
        payload_sz: usize,
        wake_fd: RawFd,
        wake_coalescer: Arc<WakeCoalescer>,
        node: Option<usize>,
    ) -> io::Result<Arc<Self>> {
        let dup = unsafe { libc::dup(wake_fd) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dup` just returned a fresh owned descriptor.
        let wake = unsafe { OwnedFd::from_raw_fd(dup) };
        let stride = payload_sz
            .checked_next_multiple_of(4096)
            .ok_or_else(|| io::Error::other("payload_sz overflow"))?;
        let span = stride
            .checked_mul(depth)
            .ok_or_else(|| io::Error::other("payload arena span overflow"))?;
        // SAFETY: fresh anonymous RW mapping, kernel-validated length.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                span,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "payload arena mmap failed",
            ));
        }
        let base = base as usize;
        // Anon THP is opportunistic fleet-wide (`enabled=always`); the
        // advise makes it explicit where the policy is `madvise`.
        // SAFETY: madvise over our own fresh mapping — advisory only.
        unsafe {
            libc::madvise(base as *mut libc::c_void, span, libc::MADV_HUGEPAGE);
        }
        let numa = crate::numa_core::topology();
        if let Some(n) = node {
            if crate::numa_core::placement_active() {
                // Bind BEFORE first touch, then populate so the pages
                // fault deterministically on the queue's node (the same
                // compose-order the session-arena THP lever uses).
                let bound = numa.bind_region_preferred(base as *mut u8, span, n);
                // SAFETY: advisory populate of our own mapping.
                unsafe {
                    libc::madvise(base as *mut libc::c_void, span, libc::MADV_POPULATE_WRITE);
                }
                debug!("fuse-over-uring payload arena: node {n} bind took={bound} ({span} B)");
            }
        }
        let bufs: Vec<usize> = (0..depth).map(|i| base + i * stride).collect();
        // The instrument's memory-node truth: where each buffer's first
        // page ACTUALLY landed (get_mempolicy faults it in if needed —
        // one-time, off the data path).
        let buf_nodes: Vec<Option<usize>> = bufs
            .iter()
            .map(|&p| numa.node_of_addr(p as *const u8))
            .collect();
        Ok(Arc::new(Self {
            base,
            span,
            stride,
            bufs,
            buf_nodes,
            wake,
            wake_coalescer,
            kmbuf: None,
        }))
    }

    /// kmbuf-mode arena view (2026-08-04): wraps the queue's mmap'd
    /// kernel buffer region — bid-indexed buffers, mapping owned by the
    /// [`KmbufQueue`] (held here so leases keep the region alive), wake
    /// protocol identical. NUMA nodes are queried per buffer the same
    /// way (the locality instrument stays live on the kmbuf arm).
    fn from_kmbuf(
        kq: Arc<KmbufQueue>,
        wake_fd: RawFd,
        wake_coalescer: Arc<WakeCoalescer>,
    ) -> io::Result<Arc<Self>> {
        let dup = unsafe { libc::dup(wake_fd) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dup` just returned a fresh owned descriptor.
        let wake = unsafe { OwnedFd::from_raw_fd(dup) };
        let (base, span, stride, count) = kq.region_geometry();
        let bufs: Vec<usize> = (0..count).map(|i| base + i * stride).collect();
        let numa = crate::numa_core::topology();
        let buf_nodes: Vec<Option<usize>> = bufs
            .iter()
            .map(|&p| numa.node_of_addr(p as *const u8))
            .collect();
        Ok(Arc::new(Self {
            base,
            span,
            stride,
            bufs,
            buf_nodes,
            wake,
            wake_coalescer,
            kmbuf: Some(kq),
        }))
    }

    fn buf(&self, idx: usize) -> Option<*mut u8> {
        self.bufs.get(idx).map(|&p| p as *mut u8)
    }

    /// Dense node index buffer `idx`'s pages landed on (`None` stays out
    /// of the locality instrument).
    fn node_of_buf(&self, idx: usize) -> Option<usize> {
        self.buf_nodes.get(idx).copied().flatten()
    }

    /// Node lookup by pointer (kmbuf deliveries know the buffer only by
    /// address — the attachment is bid-indexed, not ent-indexed).
    fn node_of_ptr(&self, ptr: *const u8) -> Option<usize> {
        let p = ptr as usize;
        if p < self.base || self.stride == 0 {
            return None;
        }
        self.node_of_buf((p - self.base) / self.stride)
    }
}

impl Drop for PayloadArena {
    fn drop(&mut self) {
        if self.kmbuf.is_none() {
            // SAFETY: unmapping the span mapped in `new`; dropped once.
            // (kmbuf-mode spans are owned and unmapped by KmbufQueue.)
            unsafe { libc::munmap(self.base as *mut libc::c_void, self.span) };
        }
    }
}

// SAFETY: the raw buffer pointers reference kernel-shared payload memory
// whose access is serialized by the §5.4 lease protocol (the worker and the
// kernel write only when the ent's lease refs == 0; leases read only while
// refs > 0). `OwnedFd` writes are thread-safe.
unsafe impl Send for PayloadArena {}
unsafe impl Sync for PayloadArena {}

/// Owner behind `Bytes::from_owner` for a FUSE_WRITE payload delivered
/// zero-copy (§5.4). Holds the arena (memory + wake fd) alive and drives
/// the refs/parked re-arm protocol on drop.
struct EntPayloadLease {
    arena: Arc<PayloadArena>,
    state: Arc<EntLeaseState>,
    ptr: *const u8,
    len: usize,
    born: Instant,
}

impl AsRef<[u8]> for EntPayloadLease {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: `ptr..ptr+len` lies inside one arena buffer (kept alive by
        // `self.arena`); the lease protocol guarantees no writer (worker or
        // kernel re-arm) touches it while this lease (refs > 0) exists.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for EntPayloadLease {
    fn drop(&mut self) {
        let age_ms = self.born.elapsed().as_millis() as u64;
        TRANSPORT_LEASE_MAX_AGE_MS.fetch_max(age_ms, Ordering::Relaxed);
        TRANSPORT_LEASES_OUTSTANDING.fetch_sub(1, Ordering::Relaxed);
        // §5.4 severance-boundary TRIPWIRE — loud, NEVER fatal (2026-07-28
        // ingest-economy BLOCKING finding; the deterministic pin is
        // tests/transport_lease_overlong_tests.rs in the root suite). This
        // used to be a `debug_assert!(age < 1s)`: a firing PANICKED the
        // write-handler task mid-flight, so (a) the FUSE reply was never
        // sent — the kernel's writeback folio parked forever and fsync(2)
        // sat in uninterruptible D-state (captured live: a hung gate with
        // `waiting=28` lost requests on the connection, umount recursing
        // into the same stuck sync) — and (b) the unwind skipped the
        // `release()` below, parking this ent's COMMIT_AND_FETCH re-arm
        // forever (permanent queue-depth loss). A watchdog must never take
        // down the data plane. And ≥ 1 s is NOT proof of escape: one
        // handler invocation legitimately exceeds it under write-pipeline
        // admission backpressure on slow/debug substrates — load merely
        // selects that schedule. Overlong leases now count
        // (`transport_lease_overlong`, stats inode) and log loudly; a
        // GENUINE §5.4 escape (payload parked toward a long-lived cache)
        // shows as unbounded ages + growing `transport_parked_commits`,
        // adjudicated by counter, never by killing the handler.
        if age_ms >= 1000 {
            TRANSPORT_LEASE_OVERLONG.fetch_add(1, Ordering::Relaxed);
            error!(
                "transport payload lease held {age_ms} ms (≥ 1 s) — overlong \
                 handler invocation or a §5.4 severance escape; watch \
                 transport_lease_overlong / transport_parked_commits"
            );
        }
        lease_release_and_wake(&self.state, &self.arena);
    }
}

// SAFETY: the payload memory is owned by the arena (held alive by the Arc);
// reads are immutable while the lease lives (protocol above); drops can run
// on any thread (tokio workers) and only touch atomics + an eventfd write.
unsafe impl Send for EntPayloadLease {}
unsafe impl Sync for EntPayloadLease {}

/// Release one §5.4 lease ref on `state`; when this was the last ref with
/// a commit parked, wake the queue worker through the arena's coalescer.
/// L3 lever B — the publish (`release`'s `fetch_sub`) happened first, so
/// the coalescer may elide the eventfd write when a wake is already armed
/// (wake_core protocol; loom-verified with this exact release→arm→write
/// order). Shared by [`EntPayloadLease`] (FUSE_WRITE payload leases) and
/// [`DestDmaLease`] (MEM-1 read-destination tokens) — one wake law.
fn lease_release_and_wake(state: &EntLeaseState, arena: &PayloadArena) {
    if state.release() {
        if arena.wake_coalescer.arm() {
            let one: u64 = 1;
            // SAFETY: writing 8 bytes to an eventfd the arena keeps alive
            // (`arena.wake` is a dup owned by the arena itself).
            unsafe { libc::write(arena.wake.as_raw_fd(), &one as *const u64 as *const _, 8) };
            TRANSPORT_WAKE_WRITES.fetch_add(1, Ordering::Relaxed);
        } else {
            TRANSPORT_WAKES_ELIDED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// MEM-1 (pre-rc engineering spec §2, P0): owner token for one zero-copy
/// READ-destination DMA — the §5.4 lease protocol applied to the
/// completion direction. Claimed by the NVMe read path at device-request
/// build ([`FuseOverUring::lease_dest_window`], reached through the
/// SqueezeFS `nvme_dev` dest-resolver registry) and held by the DEVICE
/// WORKER for exactly the SQE's lifetime: while any token is live the
/// ent's COMMIT_AND_FETCH parks (the same gate FUSE_WRITE payload leases
/// ride), so an abandoned read future (timeout / drop) can no longer let
/// the transport re-arm the payload buffer while device DMA can still
/// land in it. The arena Arc keeps the destination memory and wake fd
/// alive even past worker/pool teardown — a wedged device degrades to a
/// parked ring slot, never a dangling pointer.
///
/// LAW: the reply body served from the destination must NEVER hold this
/// token — `apply_reply` runs only after the gate proves refs == 0, so a
/// token owned by the reply would park its own commit forever. The token
/// belongs to the worker's in-flight request, nothing else.
pub struct DestDmaLease {
    arena: Arc<PayloadArena>,
    state: Arc<EntLeaseState>,
}

impl Drop for DestDmaLease {
    fn drop(&mut self) {
        lease_release_and_wake(&self.state, &self.arena);
    }
}

/// Shared work queue for all session workers (primary + multi-queue clones).
struct InboundQueue {
    tx: tokio::sync::mpsc::UnboundedSender<InboundUringReq>,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<InboundUringReq>>,
}

impl InboundQueue {
    fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            tx,
            rx: tokio::sync::Mutex::new(rx),
        }
    }

    /// FUSE-2 row 4: the caller MUST handle a failed push. A closed
    /// receiver (the session worker exited) used to be `let _ = …send()`
    /// while the queue worker went on believing the request was in
    /// flight — the request then existed nowhere and its caller waited
    /// forever.
    fn push(&self, req: InboundUringReq) -> Result<(), ()> {
        self.tx.send(req).map_err(|_| ())
    }

    /// Pure event-driven pop (L3 lever C): parks on the channel wake and the
    /// pool's shutdown notify — no poll cadence, no per-pull timer
    /// registration (the retired 200 ms `pop_timeout` cost a time-driver
    /// park + `epoll_wait` per request). Returns `None` exactly when the
    /// pool is shut down.
    async fn pop(
        &self,
        active: &AtomicBool,
        shutdown: &tokio::sync::Notify,
    ) -> Option<InboundUringReq> {
        let mut rx_guard = self.rx.lock().await;
        loop {
            // Register interest BEFORE the active check: `notify_waiters`
            // wakes only already-registered waiters, so enable-then-check
            // closes the store(false)/notify vs check/park race (the
            // documented tokio pattern). Either the check sees the store,
            // or the registration precedes the notify and the select wakes.
            let notified = shutdown.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !active.load(Ordering::Acquire) {
                return None;
            }
            tokio::select! {
                r = rx_guard.recv() => return r,
                _ = notified.as_mut() => continue, // re-check active
            }
        }
    }
}

/// Process-wide FUSE-over-io_uring controller (one per fuse session / mount).
pub struct FuseOverUring {
    /// Session may drain the inbound queue only when true (all queues REGISTERed).
    ready: AtomicBool,
    /// False after shutdown; workers exit and session falls through to errors.
    active: AtomicBool,
    /// Wakes every parked session pull on shutdown (L3 lever C — the pull
    /// path is pure event-driven; this is its only non-channel wake).
    shutdown_notify: tokio::sync::Notify,
    /// Number of queue workers that have submitted their initial REGISTERs.
    queues_registered: AtomicU64,
    pub(crate) nqueues: u16, // used for diagnostics
    inbound: Vec<Arc<InboundQueue>>,
    /// Per-slot liveness publication for the **ungated** stale-slot
    /// watchdog (FUSE-2): `nqueues × depth` cells, indexed
    /// `qid * depth + ent_idx`.
    ///
    /// This replaces the sharded `unique → slot` map (PERF-16). The map
    /// cost three mutex acquisitions plus three hashes per request and
    /// was still the *source of truth* for reply addressing, which is
    /// exactly why a request could be lost by missing it. Now the
    /// authority is the queue worker's own [`SlotTable`], the reply
    /// address rides the request, and this array is a pure observation
    /// side-channel: two relaxed stores at delivery, one at commit, read
    /// only by the watch thread.
    slot_watch: Vec<SlotWatch>,
    /// Ring depth (per queue) — the `slot_watch` stride.
    depth: usize,
    /// §5.3 D3.b: session SQPOLL posture for the queue rings (`None` =
    /// knob unset = plain rings). See [`SqpollGroup`] for the one-poller
    /// leader/attach topology.
    sqpoll: Option<SqpollGroup>,
    queues: Vec<QueueHandle>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    fuse_fd: RawFd,
    payload_sz: usize,
    /// Session buffer mode (2026-08-04 kmbuf campaign): resolved ONCE at
    /// `try_start` from the runtime capability probe + the
    /// `SQUEEZEFS_FUSE_KMBUF` lever. `UserEnts` = today's path,
    /// byte-identical.
    buffer_mode: TransportBufferMode,
    /// True when qid == kernel cpu id (queue count == kernel possible
    /// CPUs — the production default). The NUMA placement/instrument
    /// derives each queue's node from its qid ONLY under this
    /// correspondence; testing queue overrides break it and disable
    /// per-queue placement rather than mis-derive.
    qid_is_cpu: bool,
    // metrics
    pub stats_requests: AtomicU64,
    pub stats_replies: AtomicU64,
    pub stats_cqe_err: AtomicU64,
    pub stats_register: AtomicU64,
}

/// `SQUEEZEFS_TRANSPORT_DEBUG=1` — per-request transport tracing to stderr
/// (delivery / reply / commit / CQE errors) for stuck-request forensics.
pub fn transport_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("SQUEEZEFS_TRANSPORT_DEBUG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

macro_rules! xport_dbg {
    ($($arg:tt)*) => {
        if transport_debug() {
            eprintln!($($arg)*);
        }
    };
}

static ACTIVE_SESSIONS: AtomicU64 = AtomicU64::new(0);
static STATS_REQUESTS: AtomicU64 = AtomicU64::new(0);
static STATS_REPLIES: AtomicU64 = AtomicU64::new(0);
static STATS_CQE_ERR: AtomicU64 = AtomicU64::new(0);
static STATS_REGISTER: AtomicU64 = AtomicU64::new(0);
// D3.a (S2) transport submit economy (SqueezeFS metadata-throughput design
// §5.3): COMMIT_AND_FETCH SQEs per ring flush. ≈ 1 under load means the
// queue-worker batching regressed to submit-per-message.
static TRANSPORT_COMMIT_BATCH: CommitBatchHistogram = CommitBatchHistogram::new();
static TRANSPORT_COMMIT_FLUSHES: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_COMMITS_SUBMITTED: AtomicU64 = AtomicU64::new(0);

/// Bucket labels for [`CommitBatchHistogram`] (stats-JSON keys, exported by
/// [`over_uring_commit_batch_stats`]). Exact for batch sizes 1–8, then
/// power-of-two up to the per-queue depth cap (`Q_DEPTH` clamps at 32).
pub const COMMIT_BATCH_LABELS: [&str; 11] = [
    "1", "2", "3", "4", "5", "6", "7", "8", "<=16", "<=32", ">32",
];

/// `transport_commit_batch` histogram (design §9): COMMIT_AND_FETCH SQEs
/// carried by one `io_uring_enter` flush of a queue worker. Exact buckets
/// 1–8 so a "≈ 1 under load ⇒ batching regressed" verdict never rides
/// bucket rounding (the `meta_commit_group_size` convention).
pub struct CommitBatchHistogram {
    buckets: [AtomicU64; 11],
}

impl Default for CommitBatchHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitBatchHistogram {
    // `pub` methods below (new/record) exist for the microbench program
    // (2026-08-04): `benches/fuse3_hot_bench.rs` prices the per-flush
    // batch accounting the queue-worker drain pays on every submit.
    pub const fn new() -> Self {
        Self {
            buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }

    fn bucket_index(n: usize) -> usize {
        match n {
            0 => 0, // empty batches are never recorded; clamp defensively
            1..=8 => n - 1,
            9..=16 => 8,
            17..=32 => 9,
            _ => 10,
        }
    }

    /// Record one flush that carried `n ≥ 1` COMMIT_AND_FETCH SQEs.
    pub fn record(&self, n: usize) {
        self.buckets[Self::bucket_index(n)].fetch_add(1, Ordering::Relaxed);
    }

    /// Bucket snapshot in [`COMMIT_BATCH_LABELS`] order.
    pub fn snapshot(&self) -> [u64; 11] {
        std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed))
    }
}

/// D3.a batch accounting: `(flushes, commits, buckets)` — ring flushes that
/// carried ≥ 1 COMMIT_AND_FETCH SQE, total COMMIT_AND_FETCH SQEs submitted,
/// and the per-flush batch-size histogram in [`COMMIT_BATCH_LABELS`] order.
/// `commits / flushes` is the mean batch size; ≈ 1 under storm load means
/// the queue-worker submit batching regressed (design §9).
pub fn over_uring_commit_batch_stats() -> (u64, u64, [u64; 11]) {
    (
        TRANSPORT_COMMIT_FLUSHES.load(Ordering::Relaxed),
        TRANSPORT_COMMITS_SUBMITTED.load(Ordering::Relaxed),
        TRANSPORT_COMMIT_BATCH.snapshot(),
    )
}

/// Per-queue-worker pending-SQE flush accounting (D3.a S2). Every SQE push
/// in the worker loop routes through [`push_cmd_batched`] /
/// [`push_poll_batched`]; [`flush_submit`] (or the loop-bottom
/// `submit_and_wait`) is the single syscall that carries them.
#[derive(Default)]
struct SubmitBatch {
    /// SQEs pushed since the last flush (all opcodes).
    pending: u32,
    /// COMMIT_AND_FETCH SQEs among `pending`.
    commits: u32,
    /// `commit_flush` attribution (2026-08-04): READ / WRITE commits
    /// present in the pending batch (per delivered opcode — see
    /// `Ent::last_opcode`).
    commit_reads: bool,
    commit_writes: bool,
}

impl SubmitBatch {
    /// Attribute a pushed commit to its op class for the `commit_flush`
    /// phase (READ/WRITE only — the families' scope).
    fn note_commit_opcode(&mut self, opcode: u32) {
        const FUSE_READ_OPCODE: u32 = crate::raw::abi::fuse_opcode::FUSE_READ as u32;
        if opcode == FUSE_READ_OPCODE {
            self.commit_reads = true;
        } else if opcode == FUSE_WRITE_OPCODE {
            self.commit_writes = true;
        }
    }

    /// Note that a flush syscall is about to carry the pending SQEs:
    /// record the commit batch size, reset the counters, and hand back
    /// the batch's `(reads, writes)` commit-class presence so the caller
    /// can record `commit_flush` spans after the syscall returns.
    fn note_flush(&mut self) -> (bool, bool) {
        if self.commits > 0 {
            TRANSPORT_COMMIT_BATCH.record(self.commits as usize);
            TRANSPORT_COMMIT_FLUSHES.fetch_add(1, Ordering::Relaxed);
            TRANSPORT_COMMITS_SUBMITTED.fetch_add(self.commits as u64, Ordering::Relaxed);
        }
        let classes = (self.commit_reads, self.commit_writes);
        self.pending = 0;
        self.commits = 0;
        self.commit_reads = false;
        self.commit_writes = false;
        classes
    }
}

/// Record one `commit_flush` span per op class the flushed batch carried
/// (see [`TransportPhase::CommitFlush`] for the sampling contract).
fn record_commit_flush(had_reads: bool, had_writes: bool, dur: Duration) {
    use crate::raw::read_phase::{
        read_transport_phase_record, write_transport_phase_record, TransportPhase,
    };
    if had_reads {
        read_transport_phase_record(TransportPhase::CommitFlush, dur);
    }
    if had_writes {
        write_transport_phase_record(TransportPhase::CommitFlush, dur);
    }
}

/// Push one FUSE uring cmd SQE with batch accounting — **no submit**. The
/// syscall is shared: the loop-bottom `submit_and_wait(1)` (or an explicit
/// [`flush_submit`]) carries every SQE pushed since the last flush (§5.3
/// D3.a). SQ-full is absorbed by submit-and-continue: flush the queued
/// SQEs (which records the partial commit batch) and retry the push once —
/// only a push that fails right after a successful flush is a real error.
#[allow(clippy::too_many_arguments)] // one wire word per SQE field; a spec struct would obscure the ABI
fn push_cmd_batched(
    ring: &mut Ring,
    batch: &mut SubmitBatch,
    cmd_op: u32,
    qid: u16,
    commit_id: u64,
    iov: Option<(*const libc::iovec, u32)>,
    user_data: u64,
    init_flags: u16,
    buf_index: u16,
) -> io::Result<()> {
    if push_cmd(
        ring, cmd_op, qid, commit_id, iov, user_data, init_flags, buf_index,
    )
    .is_err()
    {
        // `push` only fails on a full SQ (§5.3 D3.a SQ-full rule).
        flush_submit(ring, batch)?;
        push_cmd(
            ring, cmd_op, qid, commit_id, iov, user_data, init_flags, buf_index,
        )?;
    }
    batch.pending += 1;
    if cmd_op == FUSE_IO_URING_CMD_COMMIT_AND_FETCH {
        batch.commits += 1;
    }
    Ok(())
}

/// Re-arm the wake-fd PollAdd (`user_data = u64::MAX`) with the same batch
/// accounting and SQ-full handling as [`push_cmd_batched`]; submitted by
/// the next flush. Lost-wake-safe by level-triggered eventfd semantics: a
/// signal raised before the (deferred) submit completes the poll the
/// moment it is armed.
fn push_poll_batched(ring: &mut Ring, batch: &mut SubmitBatch) -> io::Result<()> {
    let entry = Entry128::from(
        opcode::PollAdd::new(types::Fixed(1), libc::POLLIN as _)
            .build()
            .user_data(UD_POLL),
    );
    // SAFETY: a PollAdd SQE references no user memory.
    if unsafe { ring.submission().push(&entry) }.is_err() {
        flush_submit(ring, batch)?;
        // SAFETY: as above.
        unsafe { ring.submission().push(&entry) }
            .map_err(|_| io::Error::other("sq full (poll)"))?;
    }
    batch.pending += 1;
    Ok(())
}

/// Flush every pushed-but-unsubmitted SQE with ONE `ring.submit()`,
/// recording the commit batch it carries (and, when commits ride it, the
/// `commit_flush` span — `submit()` is wait-free, so the duration is the
/// submission work itself, which is where the kernel's commit-side copy
/// machinery runs). Returns the submitted count.
fn flush_submit(ring: &mut Ring, batch: &mut SubmitBatch) -> io::Result<usize> {
    let (had_reads, had_writes) = batch.note_flush();
    let t0 = (had_reads || had_writes).then(Instant::now);
    let n = ring.submit()?;
    if let Some(t0) = t0 {
        record_commit_flush(had_reads, had_writes, t0.elapsed());
    }
    Ok(n)
}
// §5.4 transport payload-lease observability (SqueezeFS stats inode).
static TRANSPORT_PAYLOAD_LEASES: AtomicU64 = AtomicU64::new(0);
// MEM-1 dest-DMA lease claims (read-destination owner tokens) — the
// engagement instrument: ≈ every dest-bearing device read on an armed
// session claims exactly one per SQE.
static TRANSPORT_DEST_DMA_LEASES: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_PARKED_COMMITS: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_LEASES_OUTSTANDING: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_LEASE_MAX_AGE_MS: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_LEASE_OVERLONG: AtomicU64 = AtomicU64::new(0);
// L3 lever B wake economy: eventfd writes performed vs elided by the
// per-queue WakeCoalescer (submit_reply + lease-drop sites). Regression
// signal: writes/(writes+elided) ≈ 1 under saturated load means the
// coalescer stopped eliding (the pre-L3 1.67 eventfd writes/op posture).
static TRANSPORT_WAKE_WRITES: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_WAKES_ELIDED: AtomicU64 = AtomicU64::new(0);

// FUSE-2 reply-integrity counters — ALWAYS ON (the two detectors that
// existed before this program were both `transport_debug`-gated, i.e.
// off in production). `transport_requests_abandoned` is the must-stay-0
// tripwire: any growth means a request left its slot without a
// COMMIT_AND_FETCH, which is an application in uninterruptible sleep and
// an `umount` that returns EBUSY.
static TRANSPORT_REQUESTS_FAILED_SYNTHETIC: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_REQUESTS_ABANDONED: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_REPLIES_REFUSED_STALE: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_REPLIES_DROPPED_NO_SLOT: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_ENTS_RETIRED: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_SLOTS_OVERDUE: AtomicU64 = AtomicU64::new(0);

/// FUSE-2 reply-integrity counters (stats inode):
/// `(requests_failed_synthetic, requests_abandoned, replies_refused_stale,
/// replies_dropped_no_slot, ents_retired, slots_overdue)`.
///
/// * `requests_failed_synthetic` — replies the transport synthesized
///   because no handler reply could be delivered (CQE-error reclaim,
///   failed inbound push, teardown drain, panicked handler). Nonzero is
///   not a bug by itself; it is the honest count of the errors the
///   kernel was told about instead of being left waiting.
/// * `requests_abandoned` — **must stay 0**: a delivered request that
///   left its slot with no commit submitted.
/// * `replies_refused_stale` — double / late / mis-addressed replies
///   refused by the slot state machine (FUSE-3e; never an overwrite).
/// * `replies_dropped_no_slot` — replies for uniques with no ring slot
///   on an armed session (FUSE-3b: kept OUT of the reply gauge).
/// * `ents_retired` — ring ents retired after `REGISTER_RETRY_MAX`
///   consecutive REGISTER failures (FUSE-3a).
/// * `slots_overdue` — slots seen owing a reply for longer than the
///   watchdog window (ungated; the promoted `stale-pending` scan).
pub fn transport_reply_integrity_stats() -> (u64, u64, u64, u64, u64, u64) {
    (
        TRANSPORT_REQUESTS_FAILED_SYNTHETIC.load(Ordering::Relaxed),
        TRANSPORT_REQUESTS_ABANDONED.load(Ordering::Relaxed),
        TRANSPORT_REPLIES_REFUSED_STALE.load(Ordering::Relaxed),
        TRANSPORT_REPLIES_DROPPED_NO_SLOT.load(Ordering::Relaxed),
        TRANSPORT_ENTS_RETIRED.load(Ordering::Relaxed),
        TRANSPORT_SLOTS_OVERDUE.load(Ordering::Relaxed),
    )
}

/// FUSE-2 rows 2/3: a reply the session's reply task could no longer
/// carry, delivered by committing straight against its ring slot.
/// Counted as a synthetic-path delivery — the reply itself is the
/// handler's, but the path is the transport's rescue arm.
pub fn note_reply_direct_commit() {
    TRANSPORT_REQUESTS_FAILED_SYNTHETIC.fetch_add(1, Ordering::Relaxed);
}

/// FUSE-2 row 1: an EIO synthesized by the session's per-request reply
/// guard (a handler that panicked or was dropped before replying).
pub fn note_reply_synthesized_by_guard() {
    TRANSPORT_REQUESTS_FAILED_SYNTHETIC.fetch_add(1, Ordering::Relaxed);
}

/// A request that can no longer be answered by ANY path — the
/// must-stay-0 tripwire.
pub fn note_request_abandoned() {
    TRANSPORT_REQUESTS_ABANDONED.fetch_add(1, Ordering::Relaxed);
}

/// FUSE-3b: a reply whose slot no longer holds its request (teardown, or
/// a refusal by the slot state machine). Counted APART from the reply
/// gauge — a dropped reply must never read as delivered.
pub(crate) fn note_reply_dropped_no_slot() {
    TRANSPORT_REPLIES_DROPPED_NO_SLOT.fetch_add(1, Ordering::Relaxed);
}

/// L3 lever B wake-economy counters: `(wake_writes, wakes_elided)` —
/// queue-eventfd writes performed vs elided by the per-queue coalescer.
pub fn transport_wake_stats() -> (u64, u64) {
    (
        TRANSPORT_WAKE_WRITES.load(Ordering::Relaxed),
        TRANSPORT_WAKES_ELIDED.load(Ordering::Relaxed),
    )
}
// NUMA-affinity campaign (2026-07-31): the transport half of the
// UPI-crossing estimate instrument — payload bytes whose instrumented
// CPU pass was / was not a minimal-distance choice
// (`numa_core::is_local_choice`). Sites: FUSE_WRITE lease delivery
// (exec ≈ the qid CPU the kernel copied from, mem = the ent buffer's
// actual node — the K1 crossing estimate) and the reply body copy into
// the ent payload (`apply_reply`, exec = the queue worker). Bytes with
// an unknown node on either side never enter the instrument. Surfaced
// as `fuse3_numa_{local,remote}_bytes` on the stats inode.
static FUSE3_NUMA_LOCAL_BYTES: AtomicU64 = AtomicU64::new(0);
static FUSE3_NUMA_REMOTE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Distance-based locality classification for one transport CPU pass
/// over `bytes` payload bytes (unknown nodes stay out — never guessed).
fn numa_classify_pass(exec_node: Option<usize>, mem_node: Option<usize>, bytes: usize) {
    let (Some(e), Some(m)) = (exec_node, mem_node) else {
        return;
    };
    let t = crate::numa_core::topology();
    if e >= t.len() || m >= t.len() {
        return;
    }
    if t.is_local_choice(e, m) {
        FUSE3_NUMA_LOCAL_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    } else {
        FUSE3_NUMA_REMOTE_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// Transport-side locality gauge (stats inode `fuse3_numa_local_bytes`).
pub fn numa_local_bytes() -> u64 {
    FUSE3_NUMA_LOCAL_BYTES.load(Ordering::Relaxed)
}

/// Transport-side locality gauge (stats inode `fuse3_numa_remote_bytes`).
pub fn numa_remote_bytes() -> u64 {
    FUSE3_NUMA_REMOTE_BYTES.load(Ordering::Relaxed)
}

// Post-arm classical sideband deliveries (kernel-mandated: FORGET/INTERRUPT/
// resends + `fiq->ops` switchover stragglers ride the classical device even
// with the ring armed). Zero here after an unlink storm means sideband
// traffic is being stranded — the stuck-request unmount wedge class.
static TRANSPORT_CLASSICAL_SIDEBAND: AtomicU64 = AtomicU64::new(0);

/// Record one post-arm classical `/dev/fuse` delivery (sideband session).
pub(crate) fn note_classical_sideband() {
    TRANSPORT_CLASSICAL_SIDEBAND.fetch_add(1, Ordering::Relaxed);
}

/// Post-arm classical sideband deliveries serviced (FORGET/BATCH_FORGET,
/// INTERRUPT, resends, and switchover-window stragglers).
pub fn over_uring_classical_sideband() -> u64 {
    TRANSPORT_CLASSICAL_SIDEBAND.load(Ordering::Relaxed)
}

pub fn over_uring_sessions_active() -> u64 {
    ACTIVE_SESSIONS.load(Ordering::Relaxed)
}

/// Cumulative FUSE-over-io_uring counters: (requests, replies, cqe_err, registers).
pub fn over_uring_stats() -> (u64, u64, u64, u64) {
    (
        STATS_REQUESTS.load(Ordering::Relaxed),
        STATS_REPLIES.load(Ordering::Relaxed),
        STATS_CQE_ERR.load(Ordering::Relaxed),
        STATS_REGISTER.load(Ordering::Relaxed),
    )
}

/// Transport payload-lease counters (§5.4): `(payload_leases,
/// parked_commits, leases_outstanding, lease_max_age_ms, lease_overlong,
/// dest_dma_leases)`.
/// `payload_leases` proves adoption (FUSE_WRITE rides leases, not copies);
/// `parked_commits` ≫ 0 means handlers hold payloads past their reply or
/// Q_DEPTH is too small; `leases_outstanding` returns to 0 at quiesce;
/// `lease_max_age_ms` is the severance-boundary high-water mark (bounded by
/// one handler invocation); `lease_overlong` counts ≥ 1 s lifetimes — the
/// loud-never-fatal §5.4 tripwire (see `EntPayloadLease::drop`);
/// `dest_dma_leases` counts MEM-1 read-destination owner-token claims
/// (≈ one per dest-bearing device-read SQE on an armed session).
pub fn transport_lease_stats() -> (u64, u64, u64, u64, u64, u64) {
    (
        TRANSPORT_PAYLOAD_LEASES.load(Ordering::Relaxed),
        TRANSPORT_PARKED_COMMITS.load(Ordering::Relaxed),
        TRANSPORT_LEASES_OUTSTANDING.load(Ordering::Relaxed),
        TRANSPORT_LEASE_MAX_AGE_MS.load(Ordering::Relaxed),
        TRANSPORT_LEASE_OVERLONG.load(Ordering::Relaxed),
        TRANSPORT_DEST_DMA_LEASES.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// L1 transport-concurrency policy (IOPS-parity program, 2026-07-15,
// `.benchmarks/2026-07-15-iops-parity-decomposition.md`)
//
// Random-4k iodepth workloads offer hundreds of in-flight requests; two
// kernel-side gates multiply on delivery: the per-queue ring depth and the
// INIT-negotiated `max_background`. Measured on the user's exact elbencho
// line: depth 16 alone +2.2×, max_background 256 alone +0×, BOTH 44k → 316k
// IOPS (7.2×, device-true). The policy below ships that class by DEFAULT
// while a payload-buffer budget keeps small-RAM boxes at (or gracefully
// near) the pre-L1 footprint. Every knob stays an override with unchanged
// semantics.
// ---------------------------------------------------------------------------

/// Desired per-queue depth when the payload-buffer budget allows it — the
/// measured-best configuration (QD32 + mb256 = 316k on the decomposition
/// box) and the existing clamp ceiling.
pub const Q_DEPTH_DESIRED: usize = 32;
/// Never degrade below the pre-L1 shipped default: at floor the payload
/// arena is exactly yesterday's footprint (queues × 4 × payload_sz), so no
/// box regresses below the behavior it already ran.
pub const Q_DEPTH_FLOOR: usize = 4;
/// The budget ladder's payload floor: the pre-4 MiB-campaign shipped ent
/// size (max_write 1 MiB = 256 × 4 KiB pages — the shape every box ran
/// before the sysctl-negotiated geometry). The variable-ent ladder never
/// degrades max_write below `min(target, PAYLOAD_BASE)`, so at the floor
/// the arena is exactly yesterday's posture (queues × 4 × 1 MiB) — the
/// same never-regress law [`Q_DEPTH_FLOOR`] encodes for depth.
pub const PAYLOAD_BASE: usize = 1024 * 1024;
/// `max_background` floor: the intent of the historical (dead-letter)
/// `max_background=64` mount-option string — never ship less delivered
/// background concurrency than that on any geometry (the never-regress-
/// below-shipped floor law).
pub const MAX_BACKGROUND_FLOOR: u16 = 64;

/// Fallback payload-buffer cap for embedders that pass no cap through
/// [`crate::MountOptions::transport_buffer_cap_bytes`]: an eighth of
/// physical RAM — the same scale-free fraction SqueezeFS derives from
/// its resolved memory budget. No absolute byte ceiling (2026-08-04
/// derivation sweep; the former fixed 2 GiB
/// `TRANSPORT_BUFFER_CAP_CEILING` degraded depth below the measured-best
/// 32 on > 64-possible-CPU big-RAM boxes for no physical reason): the
/// pinned-arena bound is STRUCTURAL — [`TransportGeometry::plan`] never
/// registers more than `nqueues × Q_DEPTH_DESIRED × payload_sz` (the
/// demand cap; depth is clamped to the desired 32 by construction).
fn default_buffer_cap() -> u64 {
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page_sz = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
    if pages <= 0 || page_sz <= 0 {
        // Cannot size RAM: fall back to the floor geometry (depth 4).
        return 0;
    }
    (pages as u64).saturating_mul(page_sz as u64) / 8
}

/// The kernel's `fs.fuse.max_pages_limit` sysctl — the ceiling
/// `process_init_reply` clamps our advertised `max_pages` to
/// (`fc->max_pages = min(fc->max_pages_limit, max(arg->max_pages, 1))`,
/// fs/fuse/inode.c). Default 256, writable 1..65535, absent on kernels
/// that predate the sysctl — the 256 fallback is the compiled-in default
/// those kernels still carry.
fn max_pages_limit() -> usize {
    std::fs::read_to_string("/proc/sys/fs/fuse/max_pages_limit")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(256)
        .clamp(1, u16::MAX as usize)
}

/// Runtime page size (the kernel's `PAGE_SIZE` in the max_pages math) —
/// derived, never assumed 4 KiB (portable-by-default: 16K/64K-page arm64
/// boxes compute the same geometry the kernel does).
fn page_size() -> usize {
    let sz = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
    if sz > 0 {
        sz as usize
    } else {
        4096
    }
}

/// Kernel possible-CPU count (`_SC_NPROCESSORS_CONF` — what
/// fuse_uring_create() sizes queues by). Shared by the geometry resolve
/// and the qid↔cpu correspondence check (NUMA placement relies on
/// qid == kernel cpu id, which holds exactly when the queue count is
/// the kernel's own).
fn kernel_possible_cpus() -> usize {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
    if n > 0 {
        n as usize
    } else {
        std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4)
    }
}

/// The resolved per-session transport geometry + INIT background limits.
/// Resolved ONCE per session (in `Session::init_filesystem`, before the
/// INIT reply is serialized) and passed unchanged into
/// [`FuseOverUring::try_start`], so the limits the kernel was told always
/// match the rings that got registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportGeometry {
    /// Queue count — kernel possible CPUs (`num_possible_cpus()` on the
    /// kernel side); `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` is a
    /// testing-only override (fewer queues than possible CPUs never
    /// becomes ready).
    pub nqueues: usize,
    /// Per-queue ring depth (see [`Self::resolve`] for the policy).
    pub depth: usize,
    /// Per-entry payload buffer size:
    /// max(FUSE_MIN_READ_BUFFER, max_write, max_pages × page).
    pub payload_sz: usize,
    /// NEGOTIATED INIT-reply `max_write` (the filesystem's desired value
    /// gated by the kernel's `fs.fuse.max_pages_limit` and the payload
    /// budget ladder).
    pub max_write: usize,
    /// INIT-reply `max_pages` — must describe [`Self::max_write`] exactly
    /// so the kernel's `ring->max_payload_sz` can never exceed the
    /// registered ents.
    pub max_pages: u16,
    /// INIT-reply `max_background`.
    pub max_background: u16,
    /// INIT-reply `congestion_threshold`.
    pub congestion_threshold: u16,
}

impl TransportGeometry {
    /// Resolve the session geometry: environment + sysconf + sysctl
    /// inputs, then the pure [`Self::plan`]. `desired_max_write` is the
    /// filesystem's INIT desire; the plan negotiates it against the
    /// kernel's `fs.fuse.max_pages_limit` and the payload budget.
    pub fn resolve(
        desired_max_write: usize,
        buffer_cap_bytes: Option<u64>,
        max_background_override: Option<u16>,
        congestion_threshold_override: Option<u16>,
    ) -> Self {
        // Kernel fuse_uring_create() uses num_possible_cpus() for
        // ring->nr_queues and is_ring_ready() requires EVERY queue (except
        // the current) to have ≥ 1 entry. Registering fewer queues means
        // the kernel never switches off the classical path → permanent
        // hang. Override only for testing; production must match the
        // kernel.
        let kernel_nqueues = kernel_possible_cpus();
        let env_queues = std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES")
            .ok()
            .and_then(|s| s.parse().ok());
        let env_depth = std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok());
        let geom = Self::plan(
            kernel_nqueues,
            env_queues,
            env_depth,
            desired_max_write,
            max_pages_limit(),
            page_size(),
            buffer_cap_bytes.unwrap_or_else(default_buffer_cap),
            max_background_override,
            congestion_threshold_override,
        );
        if geom.nqueues < kernel_nqueues {
            warn!(
                "SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES={} < kernel possible CPUs \
                 ({kernel_nqueues}); FUSE-over-io_uring will never become ready",
                geom.nqueues
            );
        }
        geom
    }

    /// The pure policy core (unit-tested). The geometry law (2026-08-04
    /// campaign; design-zero-copy-write-path §5.4c):
    ///
    /// - `nqueues`: env override clamped 1..512, else kernel possible CPUs.
    /// - `max_write` (negotiated): the filesystem's desire gated by the
    ///   kernel's advertisable ceiling —
    ///   `clamp(desired, max(page, 4096), max_pages_limit × page)` —
    ///   then possibly degraded by the budget ladder below. NEVER a
    ///   hardcoded page count: the pre-fix planner pinned 256 pages while
    ///   the INIT reply advertised `max_pages = u16::MAX`, so a raised
    ///   `fs.fuse.max_pages_limit` made the kernel's REGISTER bound
    ///   exceed the ents and every REGISTER refused (mount failure —
    ///   over-uring is mandatory).
    /// - `max_pages` (advertised): `ceil(max_write / page)` — it
    ///   describes the negotiated max_write EXACTLY, so the kernel's
    ///   `fc->max_pages = min(limit, advertised) = advertised` and
    ///   `ring->max_payload_sz = max(FUSE_MIN_READ_BUFFER, max_write,
    ///   max_pages × page) == payload_sz` by construction: REGISTER
    ///   acceptance is structural, not coincidental.
    /// - `payload_sz`: the kernel mirror above (== the registered ent
    ///   payload iovec length).
    /// - `depth` + the variable-ent budget ladder (the L1 policy
    ///   re-derived for variable ent sizes): env override wins verbatim
    ///   (clamped 1..[`Q_DEPTH_DESIRED`], bypasses the budget AND the
    ///   payload leg — explicit operator intent); otherwise
    ///   1. depth = clamp(cap / (nqueues × payload_sz), [`Q_DEPTH_FLOOR`],
    ///      [`Q_DEPTH_DESIRED`]) — the depth leg degrades FIRST;
    ///   2. only when the floor-4 arena still exceeds the cap does the
    ///      payload leg engage: max_write degrades (page-aligned) to the
    ///      largest value whose floor-4 arena fits, never below
    ///      [`PAYLOAD_BASE`] (yesterday's shipped 1 MiB ent) — at the
    ///      base, floor 4 pins regardless, exactly the pre-L1 posture
    ///      (no box regresses below the behavior it already ran).
    /// - `max_background`: override (> 0) wins, else
    ///   clamp(nqueues × depth, [`MAX_BACKGROUND_FLOOR`], u16::MAX) —
    ///   scaled with delivered ring capacity; the only ceiling is the
    ///   INIT-reply wire format (`max_background` is a u16 — a
    ///   protocol bound, not a policy constant). The former fixed 256
    ///   ceiling (the 316k-measurement's largest bracketed value) was
    ///   retired by the 2026-08-04 derivation sweep: it bound at
    ///   ≥ 64-CPU geometries with no physical basis, and the measured
    ///   row scales with `max_background` once depth is open. The old
    ///   posture stays reachable verbatim via `-o max_background=256`
    ///   (the field A0 lever).
    /// - `congestion_threshold`: override (> 0) wins, else ¾ of
    ///   `max_background` (the kernel's own default ratio).
    #[allow(clippy::too_many_arguments)] // pure policy core: every input is a policy input
    fn plan(
        kernel_nqueues: usize,
        env_queues: Option<usize>,
        env_depth: Option<usize>,
        desired_max_write: usize,
        max_pages_limit: usize,
        page_sz: usize,
        buffer_cap_bytes: u64,
        max_background_override: Option<u16>,
        congestion_threshold_override: Option<u16>,
    ) -> Self {
        const FUSE_MIN_READ_BUFFER: usize = 8192;
        let page = page_sz.max(512);
        let limit = max_pages_limit.clamp(1, u16::MAX as usize);

        // The kernel mirror, per candidate max_write: what fc->max_pages
        // and ring->max_payload_sz become when we advertise
        // ceil(mw / page) pages (fs/fuse/inode.c INIT processing +
        // fs/fuse/dev_uring.c fuse_uring_create).
        let pages_for = |mw: usize| mw.div_ceil(page).clamp(1, limit);
        let payload_for = |mw: usize| FUSE_MIN_READ_BUFFER.max(mw).max(pages_for(mw) * page);

        // Negotiate the desire against the kernel's advertisable ceiling
        // (a raised sysctl opens it; a lowered one gates it; kernels
        // without the sysctl ride the 256 fallback = today's shape).
        let max_write_target = desired_max_write.clamp(page.max(4096), limit.saturating_mul(page));

        let nqueues = env_queues.unwrap_or(kernel_nqueues).clamp(1, 512);

        let depth_at = |mw: usize| -> usize {
            let per_queue = nqueues as u64 * payload_for(mw) as u64;
            usize::try_from(buffer_cap_bytes / per_queue)
                .unwrap_or(Q_DEPTH_DESIRED)
                .clamp(Q_DEPTH_FLOOR, Q_DEPTH_DESIRED)
        };

        let (max_write, depth) = match env_depth {
            // Explicit operator intent bypasses the budget entirely —
            // both legs (unchanged env semantics).
            Some(d) => (max_write_target, d.clamp(1, Q_DEPTH_DESIRED)),
            None => {
                let depth = depth_at(max_write_target);
                let floor_arena =
                    nqueues as u64 * Q_DEPTH_FLOOR as u64 * payload_for(max_write_target) as u64;
                let base = max_write_target.min(PAYLOAD_BASE);
                if depth > Q_DEPTH_FLOOR
                    || floor_arena <= buffer_cap_bytes
                    || max_write_target <= base
                {
                    (max_write_target, depth)
                } else {
                    // Payload leg: the largest page-aligned max_write in
                    // [base, target] whose floor-depth arena fits the cap.
                    let fit = buffer_cap_bytes / (nqueues as u64 * Q_DEPTH_FLOOR as u64);
                    let fit = usize::try_from(fit).unwrap_or(max_write_target);
                    let mw = (fit / page * page).clamp(base, max_write_target);
                    (mw, depth_at(mw))
                }
            }
        };

        // limit ≤ u16::MAX by the clamp above, so this never truncates.
        let max_pages = pages_for(max_write) as u16;
        let payload_sz = payload_for(max_write);

        let max_background = match max_background_override {
            Some(mb) if mb > 0 => mb,
            _ => u16::try_from(nqueues.saturating_mul(depth))
                .unwrap_or(u16::MAX)
                .max(MAX_BACKGROUND_FLOOR),
        };
        let congestion_threshold = match congestion_threshold_override {
            Some(ct) if ct > 0 => ct,
            _ => max_background / 4 * 3,
        };

        Self {
            nqueues,
            depth,
            payload_sz,
            max_write,
            max_pages,
            max_background,
            congestion_threshold,
        }
    }

    /// Total registered payload-arena bytes this geometry pins.
    pub fn total_payload_bytes(&self) -> u64 {
        self.nqueues as u64 * self.depth as u64 * self.payload_sz as u64
    }
}

// Session geometry gauges (stats inode: `transport_{queues,q_depth,
// payload_buffer_bytes,max_background}`) — stored by `try_start` when the
// rings register. The arena is session-lifetime registered memory: the
// gauge is a level, not a counter, and returns to describe whatever
// session is live.
static GEOM_QUEUES: AtomicU64 = AtomicU64::new(0);
static GEOM_DEPTH: AtomicU64 = AtomicU64::new(0);
static GEOM_PAYLOAD_SZ: AtomicU64 = AtomicU64::new(0);
static GEOM_MAX_BACKGROUND: AtomicU64 = AtomicU64::new(0);
static GEOM_MAX_WRITE: AtomicU64 = AtomicU64::new(0);
static GEOM_MAX_PAGES: AtomicU64 = AtomicU64::new(0);

/// Resolved transport geometry of the live session:
/// `(queues, depth, payload_sz, total_payload_buffer_bytes,
/// max_background)`. Zeros until a session arms.
pub fn over_uring_geometry() -> (u64, u64, u64, u64, u64) {
    let q = GEOM_QUEUES.load(Ordering::Relaxed);
    let d = GEOM_DEPTH.load(Ordering::Relaxed);
    let p = GEOM_PAYLOAD_SZ.load(Ordering::Relaxed);
    let mb = GEOM_MAX_BACKGROUND.load(Ordering::Relaxed);
    (q, d, p, q * d * p, mb)
}

/// The live session's negotiated INIT write geometry:
/// `(max_write, max_pages)` — the values the kernel was actually told
/// (stats inode `transport_max_write` / `transport_max_pages`; the 4 MiB
/// max_write field row's engagement gauge). Zeros until a session arms.
pub fn over_uring_negotiated_write() -> (u64, u64) {
    (
        GEOM_MAX_WRITE.load(Ordering::Relaxed),
        GEOM_MAX_PAGES.load(Ordering::Relaxed),
    )
}

/// Best-effort: turn on kernel `fuse.enable_uring` so REGISTER is accepted.
/// Returns whether the parameter reads as enabled after the attempt.
pub fn ensure_kernel_fuse_uring_enabled() -> io::Result<bool> {
    const PATH: &str = "/sys/module/fuse/parameters/enable_uring";
    let read = || {
        std::fs::read_to_string(PATH).map(|s| {
            matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "y" | "1" | "yes" | "true" | "on"
            )
        })
    };
    if read().unwrap_or(false) {
        return Ok(true);
    }
    // Need privileges; mount is typically root for allow_other / fuse.
    if let Err(e) = std::fs::write(PATH, b"Y") {
        warn!("could not set {PATH}=Y: {e}");
    }
    let on = read().unwrap_or(false);
    if on {
        info!("enabled kernel fuse.enable_uring=Y");
    }
    Ok(on)
}

impl FuseOverUring {
    /// Start the queue rings for a session with an ALREADY-RESOLVED
    /// geometry (see [`TransportGeometry::resolve`] — resolved once in
    /// `Session::init_filesystem` so the INIT reply's background limits
    /// and the registered rings can never disagree).
    pub fn try_start(fuse_fd: RawFd, geom: TransportGeometry) -> io::Result<Arc<Self>> {
        if !ensure_kernel_fuse_uring_enabled()? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "kernel fuse.enable_uring is off and could not be enabled \
                 (need CAP_SYS_ADMIN / root: echo Y > /sys/module/fuse/parameters/enable_uring)",
            ));
        }
        let TransportGeometry {
            nqueues,
            depth,
            payload_sz,
            max_write,
            max_pages,
            max_background,
            ..
        } = geom;
        // kmbuf capability lattice (2026-08-04): probe + lever, once per
        // session. Absent surfaces (stock kernels) resolve to UserEnts —
        // today's path byte-identical; Present surfaces arm the bufring
        // (post-probe registration refusals FAIL the mount loudly, never
        // silently downgrade — SQUEEZEFS_FUSE_KMBUF=0 is the operator
        // escape).
        let buffer_mode = kmbuf::resolve_buffer_mode();
        GEOM_QUEUES.store(nqueues as u64, Ordering::Relaxed);
        GEOM_DEPTH.store(depth as u64, Ordering::Relaxed);
        GEOM_PAYLOAD_SZ.store(payload_sz as u64, Ordering::Relaxed);
        GEOM_MAX_BACKGROUND.store(max_background as u64, Ordering::Relaxed);
        GEOM_MAX_WRITE.store(max_write as u64, Ordering::Relaxed);
        GEOM_MAX_PAGES.store(max_pages as u64, Ordering::Relaxed);

        let mut inbound = Vec::with_capacity(nqueues);
        for _ in 0..nqueues {
            inbound.push(Arc::new(InboundQueue::new()));
        }
        let mut queue_handles = Vec::with_capacity(nqueues);
        let mut commit_rxs = Vec::with_capacity(nqueues);
        let mut wake_fds = Vec::with_capacity(nqueues);

        for _ in 0..nqueues {
            let (commit_tx, commit_rx) = std::sync::mpsc::channel();
            let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if efd < 0 {
                return Err(io::Error::last_os_error());
            }
            let wake = unsafe { OwnedFd::from_raw_fd(efd) };
            let wake_fd = wake.as_raw_fd();
            wake_fds.push(wake_fd);
            queue_handles.push(QueueHandle {
                commit_tx,
                wake_fd,
                _wake: wake,
                wake_coalescer: Arc::new(WakeCoalescer::new()),
                arena: std::sync::Mutex::new(None),
                kmbuf: std::sync::Mutex::new(None),
                // One §5.4 lease word per ring ent — pool-level so MEM-1
                // dest claims and the worker's commit gate share them.
                lease_states: (0..depth).map(|_| Arc::new(EntLeaseState::new())).collect(),
                dest_window: std::sync::OnceLock::new(),
            });
            commit_rxs.push(commit_rx);
        }

        // §5.3 D3.b: SQPOLL posture, read once per session from the same
        // knobs the classical INIT/notify rings honor. Knob unset ⇒ None ⇒
        // the workers build today's plain rings.
        let sqpoll = SqpollConfig::from_env().map(|cfg| {
            info!(
                "FUSE-over-io_uring SQPOLL enabled for queue rings: idle={}ms cpu={:?} — \
                 one shared kernel poller (qid 0 leader, ATTACH_WQ followers)",
                cfg.idle_ms, cfg.cpu
            );
            SqpollGroup {
                cfg,
                leader: OnceLock::new(),
            }
        });

        let pool = Arc::new(Self {
            // Critical: stay not-ready until every queue has submitted REGISTER.
            // Otherwise the session stops classical /dev/fuse reads while the kernel
            // still delivers on the classical path → permanent hang.
            ready: AtomicBool::new(false),
            active: AtomicBool::new(true),
            shutdown_notify: tokio::sync::Notify::new(),
            queues_registered: AtomicU64::new(0),
            nqueues: nqueues as u16,
            inbound,
            slot_watch: (0..nqueues * depth).map(|_| SlotWatch::default()).collect(),
            depth,
            sqpoll,
            queues: queue_handles,
            workers: Mutex::new(Vec::new()),
            fuse_fd,
            payload_sz,
            buffer_mode,
            qid_is_cpu: nqueues == kernel_possible_cpus(),
            stats_requests: AtomicU64::new(0),
            stats_replies: AtomicU64::new(0),
            stats_cqe_err: AtomicU64::new(0),
            stats_register: AtomicU64::new(0),
        });

        let (err_tx, err_rx) = std::sync::mpsc::sync_channel::<String>(nqueues.max(1));
        let mut handles = Vec::new();
        for qid in 0..nqueues as u16 {
            let pool_c = pool.clone();
            let commit_rx = commit_rxs.remove(0);
            let wake_fd = wake_fds[qid as usize];
            let err_tx = err_tx.clone();
            let h = std::thread::Builder::new()
                .name(format!("fuse-over-uring-{qid}"))
                .spawn(move || {
                    if let Err(e) =
                        queue_worker(pool_c.clone(), qid, depth, payload_sz, commit_rx, wake_fd)
                    {
                        let msg = format!("qid={qid}: {e}");
                        error!("fuse-over-uring worker {msg}");
                        let _ = err_tx.send(msg);
                        pool_c.shutdown();
                    }
                })
                .map_err(io::Error::other)?;
            handles.push(h);
        }
        drop(err_tx);
        *pool.workers.lock().unwrap() = handles;

        // Block until every queue has submitted REGISTER so the kernel has
        // switched fiq→uring *before* the session marks ready and the request
        // hot path moves over-uring.
        //
        // Requests that arrived on classical during this wait (e.g. parent
        // metadata() while daemon is still in INIT), plus the kernel's
        // permanent classical traffic (FORGET/INTERRUPT/resends), are serviced
        // by the post-arm classical sideband session — see
        // `FuseConnection::set_classical_sideband`.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(msg) = err_rx.try_recv() {
                pool.shutdown();
                return Err(io::Error::other(format!(
                    "FUSE-over-io_uring worker failed during setup: {msg}"
                )));
            }
            if pool.all_queues_registered() {
                break;
            }
            if !pool.active.load(Ordering::Acquire) {
                return Err(io::Error::other(
                    "FUSE-over-io_uring shut down during REGISTER",
                ));
            }
            if std::time::Instant::now() > deadline {
                let n = pool.queues_registered.load(Ordering::Acquire);
                pool.shutdown();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("FUSE-over-io_uring REGISTER timed out ({n}/{nqueues} queues)"),
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        // Every queue REGISTERed under the resolved mode — the
        // negotiation gauge is now truthful (a bufring REGISTER refusal
        // would have failed the barrier above).
        kmbuf::set_kmbuf_negotiated(buffer_mode == TransportBufferMode::BufRing);
        ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);

        // Watch /dev/fuse for POLLERR/POLLHUP/etc so we shut down even if a
        // worker is blocked in submit_and_wait and has not yet seen a CQE with
        // -ENOTCONN (e.g. all ring entries already torn down by the kernel).
        {
            let watch = pool.clone();
            let h = std::thread::Builder::new()
                .name("fuse-over-uring-watch".into())
                .spawn(move || connection_watch(watch))
                .map_err(io::Error::other)?;
            pool.workers.lock().unwrap().push(h);
        }

        // Session-log evidence line: sqpoll=off (knob unset), sqpoll=idle=<ms>
        // (poller live; cpu appended when pinned), or sqpoll=declined (kernel
        // refused; plain rings).
        let sqpoll_state = match &pool.sqpoll {
            None => "off".to_string(),
            Some(group) => match group.leader.get() {
                Some(Some(_)) => match group.cfg.cpu {
                    Some(cpu) => format!("idle={}ms,cpu={cpu}", group.cfg.idle_ms),
                    None => format!("idle={}ms", group.cfg.idle_ms),
                },
                _ => "declined".to_string(),
            },
        };
        let mode_state = match buffer_mode {
            TransportBufferMode::UserEnts => "user-ents",
            TransportBufferMode::BufRing => "kmbuf-bufring",
        };
        eprintln!(
            "FUSE-over-io_uring registered: queues={nqueues} depth={depth} payload_sz={payload_sz} \
             max_write={max_write} max_pages={max_pages} buffers={mode_state} fd={fuse_fd} sqpoll={sqpoll_state}"
        );
        info!(
            "FUSE-over-io_uring registered: queues={nqueues} depth={depth} \
             payload_sz={payload_sz} max_write={max_write} max_pages={max_pages} \
             buffers={mode_state} fd={fuse_fd} sqpoll={sqpoll_state}"
        );
        Ok(pool)
    }

    /// SIM venue (the `KmbufQueue::sim_anon` precedent): an inert pool —
    /// real atomics, slot-watch cells, inbound queues, and per-queue wake
    /// eventfds, but NO kernel fuse fd, NO rings, NO worker threads. It
    /// exists so the connection-slot install/teardown protocol (PERF-2)
    /// and the reply-send prelude bench can exercise the SHIPPED liveness
    /// machinery (`is_ready`/`is_active`/`shutdown`/slot addressing) without a
    /// mounted session. Session accounting mirrors `try_start`
    /// (ACTIVE_SESSIONS +1 here, −1 exactly once at shutdown/drop) so the
    /// `over_uring_sessions_active` gauge stays balanced in test/bench
    /// processes. The commit receivers are dropped — a `submit_reply`
    /// against a sim pool reports BrokenPipe (use
    /// `sim_inert_with_commit_rx` in-crate to hold them live).
    pub fn sim_inert(nqueues: u16) -> Arc<Self> {
        Self::sim_inert_inner(nqueues).0
    }

    /// Ring depth the SIM venue publishes (no rings exist; the slot
    /// addressing and watch geometry still need a stride).
    pub const SIM_DEPTH: usize = 32;

    /// In-crate sim variant keeping the per-queue commit receivers alive
    /// so `submit_reply` round-trips (the teardown pins).
    #[cfg(test)]
    pub(crate) fn sim_inert_with_commit_rx(
        nqueues: u16,
    ) -> (Arc<Self>, Vec<std::sync::mpsc::Receiver<CommitMsg>>) {
        Self::sim_inert_inner(nqueues)
    }

    fn sim_inert_inner(nqueues: u16) -> (Arc<Self>, Vec<std::sync::mpsc::Receiver<CommitMsg>>) {
        let mut inbound = Vec::with_capacity(nqueues as usize);
        let mut queues = Vec::with_capacity(nqueues as usize);
        let mut commit_rxs = Vec::with_capacity(nqueues as usize);
        for _ in 0..nqueues {
            inbound.push(Arc::new(InboundQueue::new()));
            let (commit_tx, commit_rx) = std::sync::mpsc::channel();
            let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            assert!(
                efd >= 0,
                "sim_inert eventfd: {}",
                io::Error::last_os_error()
            );
            // SAFETY: efd is a freshly-created, owned eventfd (checked >= 0).
            let wake = unsafe { OwnedFd::from_raw_fd(efd) };
            let wake_fd = wake.as_raw_fd();
            queues.push(QueueHandle {
                commit_tx,
                wake_fd,
                _wake: wake,
                wake_coalescer: Arc::new(WakeCoalescer::new()),
                arena: std::sync::Mutex::new(None),
                kmbuf: std::sync::Mutex::new(None),
                // MEM-1: the sim venue arms no ring, but the §5.4 lease
                // words must still exist so dest-claim paths compile and
                // read as "no owner" here (sim_inert never DMAs). The
                // inert venue registers no ents, so an empty set is the
                // honest shape.
                lease_states: Vec::new(),
                dest_window: std::sync::OnceLock::new(),
            });
            commit_rxs.push(commit_rx);
        }
        let pool = Arc::new(Self {
            ready: AtomicBool::new(false),
            active: AtomicBool::new(true),
            shutdown_notify: tokio::sync::Notify::new(),
            queues_registered: AtomicU64::new(0),
            nqueues,
            inbound,
            slot_watch: (0..nqueues as usize * Self::SIM_DEPTH)
                .map(|_| SlotWatch::default())
                .collect(),
            depth: Self::SIM_DEPTH,
            sqpoll: None,
            queues,
            workers: Mutex::new(Vec::new()),
            fuse_fd: -1,
            payload_sz: 1 << 20,
            buffer_mode: TransportBufferMode::UserEnts,
            qid_is_cpu: false,
            stats_requests: AtomicU64::new(0),
            stats_replies: AtomicU64::new(0),
            stats_cqe_err: AtomicU64::new(0),
            stats_register: AtomicU64::new(0),
        });
        // Mirror try_start's session accounting so shutdown's decrement
        // balances (the gauge never underflows in sim processes).
        ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);
        (pool, commit_rxs)
    }

    /// True once every per-CPU queue has submitted its initial REGISTER batch.
    /// Kernel `is_ring_ready` requires this before it switches `fiq->ops` to uring.
    pub fn all_queues_registered(&self) -> bool {
        self.queues_registered.load(Ordering::Acquire) >= self.nqueues as u64
            && self.active.load(Ordering::Acquire)
    }

    /// Open the session uring read path — only after all queues REGISTERed **and**
    /// the session has finished any classical handoff reads.
    pub fn mark_ready(&self) {
        if !self.all_queues_registered() {
            warn!(
                "mark_ready called before all queues REGISTERed ({}/{})",
                self.queues_registered.load(Ordering::Acquire),
                self.nqueues
            );
        }
        self.ready.store(true, Ordering::Release);
        eprintln!(
            "FUSE-over-io_uring session path armed (ready=true, queues={})",
            self.nqueues
        );
    }

    /// Session should drain the uring inbound path only when ready.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire) && self.active.load(Ordering::Acquire)
    }

    /// Event-driven pop for the session read path: parks until a request
    /// arrives or the pool shuts down (`None`). A qid beyond the queue set
    /// is structurally unreachable (workers are spawned for 0..nqueues) and
    /// returns `None` — the caller fails loud, never poll-parks.
    pub async fn recv_inbound(&self, qid: u16) -> Option<InboundUringReq> {
        if (qid as usize) < self.inbound.len() {
            self.inbound[qid as usize]
                .pop(&self.active, &self.shutdown_notify)
                .await
        } else {
            None
        }
    }

    /// Commit a reply against the slot its request was delivered on.
    ///
    /// PERF-16: no map probe, no mutex — the address rode the request.
    /// The queue worker's `SlotTable` is the authority on whether the
    /// slot still owes this reply (double / late / mis-addressed replies
    /// are refused there, loud-never-fatally, never overwriting a live
    /// commit).
    pub fn submit_reply(
        &self,
        slot: ReplySlot,
        header: Vec<u8>,
        reply_body: Bytes,
    ) -> io::Result<()> {
        let ReplySlot::Ring {
            qid,
            ent_idx,
            commit_id,
        } = slot
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "uring: reply has no ring slot (classical delivery)",
            ));
        };
        if !self.active.load(Ordering::Acquire) {
            // Teardown: the worker's drain owns every owed slot from
            // here (it synthesizes what the kernel is still waiting
            // for). Accepting a commit now would race that drain.
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "uring: session torn down",
            ));
        }
        xport_dbg!(
            "[XPORT] reply qid={qid} ent={ent_idx} cid={commit_id} body={}",
            reply_body.len()
        );
        let q = self
            .queues
            .get(qid as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad qid"))?;
        q.commit_tx
            .send(CommitMsg {
                ent_idx,
                commit_id,
                header,
                reply_body,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "uring commit closed"))?;
        // Wake the queue thread. L3 lever B: the channel send above is the
        // publication; the coalescer elides the eventfd write when a wake
        // is already armed — N replies between two worker passes cost one
        // write (wake_core protocol, loom-verified send→arm→write order).
        if q.wake_coalescer.arm() {
            let one: u64 = 1;
            let _ = unsafe { libc::write(q.wake_fd, &one as *const u64 as *const _, 8) };
            TRANSPORT_WAKE_WRITES.fetch_add(1, Ordering::Relaxed);
        } else {
            TRANSPORT_WAKES_ELIDED.fetch_add(1, Ordering::Relaxed);
        }
        self.stats_replies.fetch_add(1, Ordering::Relaxed);
        STATS_REPLIES.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn get_payload_buffer(&self, slot: ReplySlot) -> Option<(u64, usize)> {
        let ReplySlot::Ring { qid, ent_idx, .. } = slot else {
            return None;
        };
        let q = self.queues.get(qid as usize)?;
        // kmbuf mode: the reply target is the ent's ATTACHED kernel
        // buffer (per-delivery, bid-indexed) — never a static per-ent
        // slot. The attachment cannot move between delivery and our
        // commit (the kernel re-points/recycles only at fetch, which our
        // COMMIT_AND_FETCH triggers).
        if let Some(kq) = q.kmbuf.lock().unwrap().clone() {
            let (ptr, len) = kq.attached_ptr(ent_idx as usize)?;
            return Some((ptr as u64, len));
        }
        let arena = q.arena.lock().unwrap().clone()?;
        let ptr = arena.buf(ent_idx as usize)?;
        Some((ptr as u64, self.payload_sz))
    }

    /// MEM-1: claim a DMA-destination owner token over the payload window
    /// `[addr, addr + len)`. `None` when the window lies in no queue's
    /// arena (not a transport dest — e.g. an IPC-arena override), when it
    /// straddles two ent buffers (never a valid single-request dest), or
    /// when the owning ent cannot be resolved (kmbuf bid unattached).
    ///
    /// Soundness rides program order: every claim happens strictly before
    /// the claiming request's reply can exist (mint → claim → submit →
    /// await on the handler task), so a commit that passes the refs == 0
    /// gate proves no claimed SQE can still write this buffer, and a
    /// claim can never target an already-re-armed ent from a live path
    /// (the detached-assembly face is MEM-2's join-before-release law).
    pub fn lease_dest_window(&self, addr: u64, len: usize) -> Option<DestDmaLease> {
        if len == 0 {
            return None;
        }
        let addr = usize::try_from(addr).ok()?;
        for q in &self.queues {
            let Some(win) = q.dest_window.get() else {
                continue;
            };
            if !win.contains(addr, len) {
                continue;
            }
            // Straddling two buffers is never a valid single-request dest.
            let idx = dest_window_index(win.base, win.stride, addr, len)?;
            let ent = match &win.kmbuf {
                Some(kq) => kq.ent_of_bid(idx as u64)?,
                None => idx,
            };
            let state = Arc::clone(q.lease_states.get(ent)?);
            state.acquire_dest();
            TRANSPORT_DEST_DMA_LEASES.fetch_add(1, Ordering::Relaxed);
            return Some(DestDmaLease {
                arena: Arc::clone(&win.arena),
                state,
            });
        }
        None
    }

    /// The watch cell of one slot (`None` for a geometry-less sim pool).
    #[inline]
    fn slot_watch_cell(&self, qid: u16, ent_idx: usize) -> Option<&SlotWatch> {
        self.slot_watch.get(qid as usize * self.depth + ent_idx)
    }

    /// Publish a slot's owed-reply state for the watchdog (`unique == 0`
    /// clears it).
    #[inline]
    fn publish_slot_owed(&self, qid: u16, ent_idx: usize, unique: u64) {
        if let Some(w) = self.slot_watch_cell(qid, ent_idx) {
            if unique == 0 {
                w.clear();
            } else {
                w.set(unique, crate::raw::read_phase::transport_now_ns());
            }
        }
    }

    /// FUSE-2's ungated stale-slot watchdog pass: name every slot that
    /// has owed a reply for longer than [`SLOT_OVERDUE_NS`] and count it.
    fn scan_overdue_slots(&self) {
        let now = crate::raw::read_phase::transport_now_ns();
        for (idx, w) in self.slot_watch.iter().enumerate() {
            let unique = w.unique.load(Ordering::Acquire);
            if unique == 0 {
                continue;
            }
            let since = w.since_ns.load(Ordering::Relaxed);
            if since == 0 || now.saturating_sub(since) < SLOT_OVERDUE_NS {
                continue;
            }
            let depth = self.depth.max(1);
            let (qid, ent) = (idx / depth, idx % depth);
            TRANSPORT_SLOTS_OVERDUE.fetch_add(1, Ordering::Relaxed);
            warn!(
                "fuse-over-uring qid={qid} ent={ent}: unique={unique} delivered {} ms ago and \
                 still unreplied — the caller is in uninterruptible sleep (transport_slots_overdue)",
                now.saturating_sub(since) / 1_000_000
            );
        }
    }

    /// True once workers are live (may still be registering). Prefer [`is_ready`] for the
    /// session read path.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Kernel abort / unmount completed a uring cmd with a fatal disconnect errno.
    ///
    /// From `fs/fuse/dev_uring.c`: ring entry teardown and cancel complete with
    /// `-ENOTCONN`; the abort path sets request errors to `-ECONNABORTED` and
    /// may surface `-ENODEV` on the classical device. Treat all of these as
    /// "session is dead — stop workers and wake the session loop".
    #[inline]
    pub fn is_disconnect_errno(err: i32) -> bool {
        matches!(
            err,
            libc::ENOTCONN
                | libc::ECONNABORTED
                | libc::ENODEV
                | libc::EPIPE
                | libc::EBADF
                | libc::ESHUTDOWN
        )
    }

    pub fn shutdown(&self) {
        self.ready.store(false, Ordering::Release);
        if self.active.swap(false, Ordering::Release) {
            // Session was counted at spawn time (before ready).
            ACTIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
            info!("FUSE-over-io_uring shutting down (fd={})", self.fuse_fd);
            // FUSE-2 row 8: shutdown does NOT drop owed requests. There
            // is no map to clear — each queue worker's teardown drain
            // walks its own [`SlotTable`] and submits exactly one commit
            // (reply or synthesized error) per owing slot, so a
            // non-fatal shutdown cause can no longer strand a caller in
            // uninterruptible sleep. What the drain genuinely cannot
            // commit lands on `transport_requests_abandoned`.
            for (idx, w) in self.slot_watch.iter().enumerate() {
                let unique = w.unique.load(Ordering::Relaxed);
                if unique != 0 {
                    let (qid, ent) = (idx / self.depth.max(1), idx % self.depth.max(1));
                    warn!(
                        "fuse-over-uring shutdown with an owed reply: qid={qid} ent={ent} \
                         unique={unique} — the queue drain owns it"
                    );
                }
            }
        }
        // Wake every parked session pull (after the active=false store above
        // — pop's enable-then-check ordering makes this race-free).
        self.shutdown_notify.notify_waiters();
        // Teardown wakes stay UNCONDITIONAL (no coalescer): the worker must
        // wake to see active == false whatever the elision flag says, and a
        // one-shot extra write costs nothing.
        let one: u64 = 1;
        for q in &self.queues {
            let _ = unsafe { libc::write(q.wake_fd, &one as *const u64 as *const _, 8) };
        }
    }
}

impl Drop for FuseOverUring {
    fn drop(&mut self) {
        self.shutdown();
    }
}

type Ring = IoUring<squeue::Entry128, cqueue::Entry>;

/// §5.3 D3.b (S3) — SQPOLL posture for the over-uring queue rings, parsed
/// once per session from the **same env knobs** the classical INIT/notify
/// rings honor (`SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` / `_CPU`,
/// `connection/tokio.rs` semantics verbatim): idle unset / `0` /
/// unparsable ⇒ `None` ⇒ plain queue rings — today's behavior,
/// byte-identical; a bad `_CPU` value degrades to "unpinned", never to
/// "off". Knob-only: no code path depends on SQPOLL being on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SqpollConfig {
    /// `IORING_SETUP_SQPOLL` idle timeout (ms) — the poller sleeps after
    /// this long without SQEs and the next submit wakes it (crate-handled
    /// `IORING_ENTER_SQ_WAKEUP`).
    idle_ms: u32,
    /// Optional `IORING_SETUP_SQ_AFF` pin for **the one shared poller**
    /// (see [`SqpollGroup`] — the leader ring's pin governs; the kernel
    /// ignores attached rings' cpu params by design).
    cpu: Option<u32>,
}

impl SqpollConfig {
    /// `tokio.rs` knob semantics, extracted for the queue rings: idle must
    /// parse > 0 to enable; cpu is honored when parseable (CPU 0 is a real
    /// CPU — the CLI's "0 disables pinning" convention is applied by
    /// `squeezefs mount` *before* the env reaches fuse3).
    fn parse(idle_ms: Option<&str>, cpu: Option<&str>) -> Option<Self> {
        let idle_ms = idle_ms?
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|idle| *idle > 0)?;
        let cpu = cpu.and_then(|raw| raw.trim().parse::<u32>().ok());
        Some(Self { idle_ms, cpu })
    }

    /// Read the session posture from the classical knobs, once, at
    /// [`FuseOverUring::try_start`].
    fn from_env() -> Option<Self> {
        let idle = std::env::var("SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS").ok();
        let cpu = std::env::var("SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU").ok();
        Self::parse(idle.as_deref(), cpu.as_deref())
    }
}

/// One SQPOLL coordination slot per session (multi-queue policy, §5.3
/// D3.b): **exactly one kernel poller total**, whatever
/// `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` says. The qid-0 ring is the
/// *leader* — it creates the poller (`IDLE_MS` idle, optional `_CPU` pin)
/// and publishes its ring fd here; every other queue ring *attaches* to
/// that poller via `IORING_SETUP_ATTACH_WQ` (shared-sqpoll, kernel ≥ 5.12).
/// Naive per-queue SQPOLL would burn up to `nqueues` (≤ 32 by knob clamp,
/// up to 512 raw) cores; the attach topology bounds the burn at one poller
/// regardless of queue count, and makes `_CPU` unambiguous: it pins *the*
/// poller.
struct SqpollGroup {
    cfg: SqpollConfig,
    /// Leader outcome: `Some(fd)` = leader SQPOLL ring built, attach to it;
    /// `None` = the kernel declined SQPOLL (EPERM on locked-down boxes,
    /// EINVAL on pre-SQPOLL kernels) and the leader fell back to a plain
    /// ring — every follower then builds plain too. Fallback is
    /// warn-and-degrade, mirroring the classical rings' knob semantics: an
    /// opt-in accelerator must never fail the mount.
    leader: OnceLock<Option<RawFd>>,
}

/// How long a follower waits for the leader's SQPOLL outcome before
/// degrading to a plain ring. The leader publishes within its worker's
/// first microseconds; 10 s stays well inside `try_start`'s 30 s REGISTER
/// deadline even on a badly oversubscribed box.
const SQPOLL_LEADER_WAIT: Duration = Duration::from_secs(10);

/// Queue-ring setup posture (PERF-5, pre-rc spec §9).
///
/// `Modern` = today's SQE128 + cqsize setup PLUS
/// `IORING_SETUP_SINGLE_ISSUER` + `IORING_SETUP_DEFER_TASKRUN` (the
/// flags pair — the kernel refuses DEFER_TASKRUN without SINGLE_ISSUER).
/// The queue workers are one-thread-per-ring by construction
/// (`queue_worker` builds its ring on its own spawned thread and is the
/// only submitter/reaper — `submit_and_wait(1)` at the loop bottom), so
/// SINGLE_ISSUER is free, and DEFER_TASKRUN moves completion task-work
/// onto that same thread's `io_uring_enter` instead of inter-processor
/// interrupts.
///
/// `Plain` = today's setup byte-identical — the landing spot for
/// probe-miss kernels (SILENT — portable-by-default: probe, never
/// version-check) and for a Modern build refusal (warn-and-degrade; an
/// accelerator never fails the mount).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueueRingPosture {
    Modern,
    Plain,
}

/// The knob-unset queue-ring builder — today's SQE128 ring, and the
/// landing spot for every SQPOLL refusal (warn-and-degrade). NOTE the
/// SQPOLL builders in [`build_queue_ring`] never take the Modern flags:
/// DEFER_TASKRUN is kernel-incompatible with SQPOLL (the poller owns
/// task-work), and the SQPOLL posture is a knob-only measured-not-
/// recommended lever (M10).
fn build_plain_queue_ring(sq_entries: u32) -> io::Result<Ring> {
    build_plain_queue_ring_with(
        sq_entries,
        modern_queue_ring_flags_probed(),
        |sq, posture| {
            let mut builder = IoUring::<squeue::Entry128, cqueue::Entry>::builder();
            builder.setup_cqsize(sq * 2);
            if posture == QueueRingPosture::Modern {
                // The pair (kernel refuses DEFER_TASKRUN without
                // SINGLE_ISSUER); safe by construction — the worker
                // thread builds, submits, and reaps this ring alone.
                builder.setup_single_issuer().setup_defer_taskrun();
            }
            builder.build(sq).map_err(|e| {
                io::Error::other(format!(
                    "SQE128 IoUring build(sq={sq}, {posture:?}): {e} — need IORING_SETUP_SQE128"
                ))
            })
        },
    )
}

/// PERF-5 runtime capability probe (portable-by-default: probe, NEVER a
/// kernel-version check): can this kernel build the queue-ring shape
/// with `IORING_SETUP_SINGLE_ISSUER` + `IORING_SETUP_DEFER_TASKRUN`?
/// A scratch SQE128 ring is built with the exact flag set and dropped;
/// pre-6.1 kernels refuse at `io_uring_setup` (EINVAL) ⇒ `false` ⇒ the
/// Plain posture, today's setup byte-identical. Memoized per process
/// (the capability is process-invariant; the kmbuf-probe convention) —
/// derived defaults resolve at spawn, never on a cadence.
fn modern_queue_ring_flags_probed() -> bool {
    static PROBE: OnceLock<bool> = OnceLock::new();
    *PROBE.get_or_init(|| {
        let ok = IoUring::<squeue::Entry128, cqueue::Entry>::builder()
            .setup_cqsize(8)
            .setup_single_issuer()
            .setup_defer_taskrun()
            .build(4)
            .is_ok();
        info!(
            "fuse-over-uring queue-ring setup probe: SINGLE_ISSUER+DEFER_TASKRUN {}",
            if ok {
                "available"
            } else {
                "unavailable — plain setup (pre-6.1 kernel)"
            }
        );
        ok
    })
}

/// The injectable setup seam (PERF-5 fallback contract, pinned by
/// `queue_ring_posture_tests`):
/// - `modern == true` (the runtime probe found the flags) ⇒ attempt the
///   [`QueueRingPosture::Modern`] build FIRST; a refusal warns and lands
///   on the Plain build — never fails the mount over an accelerator.
/// - `modern == false` (probe miss — pre-6.1 kernels) ⇒ build Plain
///   directly, SILENTLY: today's setup, byte-identical, zero extra
///   syscalls per ring.
fn build_plain_queue_ring_with<F>(sq_entries: u32, modern: bool, mut build: F) -> io::Result<Ring>
where
    F: FnMut(u32, QueueRingPosture) -> io::Result<Ring>,
{
    if modern {
        match build(sq_entries, QueueRingPosture::Modern) {
            Ok(ring) => return Ok(ring),
            Err(e) => {
                // Probed-present but refused at build (seccomp/lockdown
                // surprises past the probe): warn-and-degrade — an
                // accelerator never fails the mount.
                warn!(
                    "fuse-over-uring: SINGLE_ISSUER+DEFER_TASKRUN refused at \
                     ring build despite probe ({e}); plain ring"
                );
            }
        }
    }
    build(sq_entries, QueueRingPosture::Plain)
}

/// Build one queue ring per the session's SQPOLL posture (§5.3 D3.b).
///
/// `sqpoll == None` (knob unset) is the default path: exactly today's
/// SQE128 builder, byte-identical, no coordination. With the knob set the
/// [`SqpollGroup`] topology applies — qid 0 creates the single poller,
/// every other qid attaches — and any SQPOLL refusal (EPERM on
/// locked-down boxes, attach on a dead leader, leader-wait deadline)
/// degrades loudly to the plain ring instead of failing the mount,
/// mirroring the classical rings' knob semantics. The worker's submit
/// paths need no SQPOLL awareness: the io-uring crate folds
/// `IORING_ENTER_SQ_WAKEUP` into `submit`/`submit_and_wait` when the
/// poller has gone idle.
fn build_queue_ring(
    sq_entries: u32,
    qid: u16,
    sqpoll: Option<&SqpollGroup>,
    active: &AtomicBool,
) -> io::Result<Ring> {
    let Some(group) = sqpoll else {
        return build_plain_queue_ring(sq_entries);
    };
    if qid == 0 {
        // Leader: create THE kernel poller (idle timeout + optional pin).
        let mut builder = IoUring::<squeue::Entry128, cqueue::Entry>::builder();
        builder
            .setup_cqsize(sq_entries * 2)
            .setup_sqpoll(group.cfg.idle_ms);
        if let Some(cpu) = group.cfg.cpu {
            builder.setup_sqpoll_cpu(cpu);
        }
        match builder.build(sq_entries) {
            Ok(ring) => {
                let _ = group.leader.set(Some(ring.as_raw_fd()));
                info!(
                    "fuse-over-uring qid=0: SQPOLL poller created (idle={}ms cpu={:?}); \
                     other queues attach via ATTACH_WQ",
                    group.cfg.idle_ms, group.cfg.cpu
                );
                Ok(ring)
            }
            Err(e) => {
                warn!(
                    "fuse-over-uring qid=0: kernel declined SQPOLL ({e}); \
                     plain rings for every queue this session"
                );
                let _ = group.leader.set(None);
                build_plain_queue_ring(sq_entries)
            }
        }
    } else {
        // Follower: wait (bounded) for the leader outcome, then attach to
        // its poller. Never create a second poller.
        let deadline = Instant::now() + SQPOLL_LEADER_WAIT;
        let leader_fd = loop {
            if let Some(outcome) = group.leader.get() {
                break *outcome;
            }
            if !active.load(Ordering::Acquire) {
                break None;
            }
            if Instant::now() > deadline {
                warn!(
                    "fuse-over-uring qid={qid}: leader SQPOLL outcome not published \
                     within {SQPOLL_LEADER_WAIT:?}; plain ring"
                );
                break None;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let Some(fd) = leader_fd else {
            return build_plain_queue_ring(sq_entries);
        };
        let mut builder = IoUring::<squeue::Entry128, cqueue::Entry>::builder();
        builder
            .setup_cqsize(sq_entries * 2)
            .setup_sqpoll(group.cfg.idle_ms)
            .setup_attach_wq(fd);
        match builder.build(sq_entries) {
            Ok(ring) => Ok(ring),
            Err(e) => {
                warn!(
                    "fuse-over-uring qid={qid}: SQPOLL ATTACH_WQ(fd={fd}) declined ({e}); \
                     plain ring"
                );
                build_plain_queue_ring(sq_entries)
            }
        }
    }
}

struct Ent {
    /// Header slot. Classical mode: points into `_owned_header` (the
    /// per-ent Box the REGISTER iov[0] names). kmbuf mode: points into
    /// the queue's fixed headers region (`KmbufQueue::header_ptr` —
    /// index 0 of the ring's fixed-buffer table), which the kernel
    /// reads/writes through the registered buffer instead of GUP.
    header_ptr: *mut FuseUringReqHeader,
    /// Classical-mode header storage (kmbuf mode: `None` — the region is
    /// owned by the queue's [`KmbufQueue`], alive past the ents).
    _owned_header: Option<Box<FuseUringReqHeader>>,
    /// Payload buffer. Classical mode: the ent's static arena slot.
    /// kmbuf mode: the CURRENTLY-ATTACHED kernel buffer (re-pointed at
    /// flagged deliveries; null until the first attachment — payload
    /// views are empty then).
    payload_ptr: *mut u8,
    payload_len: usize,
    /// The buffer's ACTUAL NUMA node (dense index; the locality
    /// instrument's memory-node source — `None` stays uncounted).
    /// kmbuf mode: re-derived per attachment (`node_of_ptr`).
    node: Option<usize>,
    iov: [libc::iovec; 2],
    /// Opcode of the last delivered request (commit_flush attribution;
    /// 0 = nothing delivered yet).
    last_opcode: u32,
}

impl Ent {
    /// Header view. SAFETY of the deref: `header_ptr` targets either the
    /// ent's owned Box or the queue's headers region, both alive for the
    /// worker's life; only the worker thread and the kernel (between
    /// re-arm and CQE — the same window discipline the payload has)
    /// touch it, and both views here are taken outside that window.
    fn hdr(&self) -> &FuseUringReqHeader {
        // SAFETY: see above.
        unsafe { &*self.header_ptr }
    }

    fn hdr_mut(&mut self) -> &mut FuseUringReqHeader {
        // SAFETY: see `hdr`.
        unsafe { &mut *self.header_ptr }
    }

    /// True when a payload buffer is bound (kmbuf ents are unbound until
    /// their first flagged delivery).
    fn has_payload_buf(&self) -> bool {
        !self.payload_ptr.is_null()
    }

    /// Immutable payload view (delivery-time copy for non-leased opcodes).
    /// Empty when no buffer is bound.
    fn payload(&self) -> &[u8] {
        if self.payload_ptr.is_null() {
            return &[];
        }
        // SAFETY: `payload_ptr..+payload_len` is one arena/kmbuf buffer,
        // alive for the worker's life (arena Arc); the kernel only writes
        // it between re-arm and the delivery CQE, and this view is taken
        // after the CQE.
        unsafe { std::slice::from_raw_parts(self.payload_ptr, self.payload_len) }
    }

    /// Mutable payload view for reply application. Caller must hold the
    /// §5.4 gate proof: the ent's lease refs == 0 (CommitGate::Ready /
    /// try_unpark). Writing while a lease lives is the mutation-under-alias
    /// UB class the protocol exists to eliminate. Empty when no buffer
    /// is bound (callers must check [`Self::has_payload_buf`] before
    /// counting on capacity).
    fn payload_mut(&mut self) -> &mut [u8] {
        if self.payload_ptr.is_null() {
            return &mut [];
        }
        // SAFETY: as above, plus the caller-supplied refs == 0 proof that no
        // live `&[u8]` (lease) aliases the region.
        unsafe { std::slice::from_raw_parts_mut(self.payload_ptr, self.payload_len) }
    }
}

/// Poll `/dev/fuse` until the connection is aborted/closed or the pool shuts down.
fn connection_watch(pool: Arc<FuseOverUring>) {
    // A std thread inherits its SPAWNER's affinity — here a core-pinned
    // tokio runtime worker, leaving the watch 1-CPU hostage. Node scope
    // (transport-ingress campaign) resets it to the process mask; the
    // `core` lever keeps the historical inherited posture.
    if crate::raw::affinity::pin_scope() == crate::raw::affinity::PinScope::Node {
        let _ = crate::raw::affinity::set_current_affinity(&crate::raw::affinity::process_cpus());
    }
    let mut last_scan = Instant::now();
    while pool.active.load(Ordering::Relaxed) {
        // FUSE-2: the stale-slot watchdog is ALWAYS ON. Its ancestor —
        // the `stale-pending` scan — was `transport_debug`-gated, i.e.
        // off in production, which is why a lost reply only ever showed
        // up as a user-visible D-state process and an EBUSY umount. A
        // slot owing a reply for longer than the window is logged loudly
        // and counted on `transport_slots_overdue`.
        if last_scan.elapsed() >= SLOT_WATCHDOG_INTERVAL {
            last_scan = Instant::now();
            pool.scan_overdue_slots();
        }
        let mut pfd = libc::pollfd {
            fd: pool.fuse_fd,
            events: libc::POLLERR | libc::POLLHUP | libc::POLLNVAL,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, 250) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            warn!("fuse-over-uring watch poll failed: {e}; shutting down");
            pool.shutdown();
            return;
        }
        if r == 0 {
            continue;
        }
        let rev = pfd.revents;
        if rev & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            info!(
                "fuse-over-uring watch: /dev/fuse revents={rev:#x} (abort/unmount); shutting down"
            );
            pool.shutdown();
            return;
        }
    }
}

fn queue_worker(
    pool: Arc<FuseOverUring>,
    qid: u16,
    depth: usize,
    payload_sz: usize,
    commit_rx: std::sync::mpsc::Receiver<CommitMsg>,
    wake_fd: RawFd,
) -> io::Result<()> {
    // The queue's NUMA node (NUMA-affinity campaign 2026-07-31): the
    // kernel routes requests to the queue of the requester's CPU, so on
    // queue-per-possible-CPU sessions qid IS a kernel cpu id and the
    // queue's home node is a map lookup. Testing queue overrides break
    // the correspondence — no per-queue node, no placement.
    let queue_node = if pool.qid_is_cpu {
        crate::numa_core::topology().node_of_cpu(qid as usize)
    } else {
        None
    };
    // Affinity posture (transport-ingress campaign 2026-08-01): node
    // scope by DEFAULT — the worker keeps its queue's home node (arena
    // binding + reply-serve locality unchanged) but may run on any
    // process-mask CPU of it, so the reap wake stops paying the pinned
    // core's runqueue wait (the same hostage mechanism the fuse3-tpc
    // lanes measured; that wait lands in the KERNEL-SIDE residue term).
    // `SQUEEZEFS_FUSE_PIN_SCOPE=core` restores the pre-campaign posture.
    match crate::raw::affinity::pin_scope() {
        crate::raw::affinity::PinScope::Core => {
            // Best-effort pin to core qid; when the exact core is outside
            // the process mask (taskset-restricted mounts) fall back to
            // the queue's NODE cpu set (intersected with the process mask)
            // so the worker's reply serves and the arena stay co-located.
            if !core_affinity::set_for_current(core_affinity::CoreId { id: qid as usize }) {
                if let Some(n) = queue_node {
                    if crate::numa_core::placement_active() {
                        let _ = crate::numa_core::topology().pin_current_to_node(n);
                    }
                }
            }
        }
        crate::raw::affinity::PinScope::Node => {
            // Home = cpu qid when qid IS a kernel cpu id; the derivation
            // degrades to the whole process mask for testing queue
            // overrides / unknown nodes / masks excluding the node.
            let topo = crate::numa_core::topology();
            let avail = crate::raw::affinity::process_cpus();
            let cpus = crate::raw::affinity::scoped_affinity_cpus(
                crate::raw::affinity::PinScope::Node,
                qid as usize,
                &avail,
                |c| topo.node_of_cpu(c),
                |n| {
                    topo.nodes()
                        .get(n)
                        .map(|d| d.cpus.clone())
                        .unwrap_or_default()
                },
            );
            let _ = crate::raw::affinity::set_current_affinity(&cpus);
        }
    }

    let sq_entries = (depth as u32 + 8).next_power_of_two().max(16);
    // §5.3 D3.b: plain SQE128 ring by default; SQPOLL leader/attach
    // topology when the session knobs opted in (see `build_queue_ring`).
    let mut ring: Ring = build_queue_ring(sq_entries, qid, pool.sqpoll.as_ref(), &pool.active)?;

    ring.submitter()
        .register_files(&[pool.fuse_fd, wake_fd])
        .map_err(|e| {
            io::Error::other(format!(
                "register_files(fuse_fd={}, wake_fd={}): {e}",
                pool.fuse_fd, wake_fd
            ))
        })?;

    // kmbuf mode (2026-08-04): register the queue's fixed headers buffer
    // + kernel-managed payload bufring BEFORE any ent REGISTER (the
    // kernel resolves both at ent-registration time). A refusal here
    // fails the worker → the mount (the capability gate is the PROBE;
    // post-probe refusals never silently downgrade).
    let kmbuf_q: Option<Arc<KmbufQueue>> = match pool.buffer_mode {
        TransportBufferMode::BufRing => {
            Some(Arc::new(KmbufQueue::setup(&ring, depth, payload_sz)?))
        }
        TransportBufferMode::UserEnts => None,
    };

    // Payload memory lives in an Arc'd arena (not the worker-local Ent) so
    // FUSE_WRITE leases and `get_payload_buffer` pointers stay valid past
    // worker exit (§5.4). The arena shares the queue's wake coalescer so
    // lease-drop wakes elide through the same flag as reply submissions.
    // kmbuf mode: the arena is a bid-indexed view over the queue's mmap'd
    // kernel buffer region (mapping owned by KmbufQueue, held alive by
    // the arena for lease lifetimes).
    let wake_coalescer = Arc::clone(&pool.queues[qid as usize].wake_coalescer);
    let arena = match &kmbuf_q {
        Some(kq) => PayloadArena::from_kmbuf(Arc::clone(kq), wake_fd, Arc::clone(&wake_coalescer))?,
        None => PayloadArena::new(
            depth,
            payload_sz,
            wake_fd,
            Arc::clone(&wake_coalescer),
            queue_node,
        )?,
    };
    // One lease state per ring ent (pool-level since MEM-1 — dest claims
    // acquire against the same words) + the worker-local parked commit
    // slots.
    let lease_states: Vec<Arc<EntLeaseState>> = pool.queues[qid as usize].lease_states.clone();
    debug_assert_eq!(lease_states.len(), depth, "lease states sized to depth");
    let mut parked_msgs: Vec<Option<CommitMsg>> = (0..depth).map(|_| None).collect();

    let mut ents: Vec<Ent> = (0..depth)
        .map(|idx| match &kmbuf_q {
            None => {
                let mut header = Box::new(FuseUringReqHeader::default());
                header.ring_ent_in_out.payload_sz = payload_sz as u32;
                let header_ptr = &mut *header as *mut FuseUringReqHeader;
                Ent {
                    header_ptr,
                    _owned_header: Some(header),
                    payload_ptr: arena.buf(idx).expect("arena sized to depth"),
                    payload_len: payload_sz,
                    node: arena.node_of_buf(idx),
                    iov: [
                        libc::iovec {
                            iov_base: std::ptr::null_mut(),
                            iov_len: 0,
                        },
                        libc::iovec {
                            iov_base: std::ptr::null_mut(),
                            iov_len: 0,
                        },
                    ],
                    last_opcode: 0,
                }
            }
            Some(kq) => {
                let mut ent = Ent {
                    // Header slot inside the fixed headers region (fresh
                    // anon mapping — zeroed).
                    header_ptr: kq.header_ptr(idx).cast(),
                    _owned_header: None,
                    // No payload buffer until the first flagged delivery.
                    payload_ptr: std::ptr::null_mut(),
                    payload_len: payload_sz,
                    node: None,
                    iov: [
                        libc::iovec {
                            iov_base: std::ptr::null_mut(),
                            iov_len: 0,
                        },
                        libc::iovec {
                            iov_base: std::ptr::null_mut(),
                            iov_len: 0,
                        },
                    ],
                    last_opcode: 0,
                };
                ent.hdr_mut().ring_ent_in_out.payload_sz = payload_sz as u32;
                ent
            }
        })
        .collect();

    if kmbuf_q.is_none() {
        for ent in &mut ents {
            ent.iov[0] = libc::iovec {
                iov_base: ent.header_ptr.cast(),
                iov_len: std::mem::size_of::<FuseUringReqHeader>(),
            };
            ent.iov[1] = libc::iovec {
                iov_base: ent.payload_ptr.cast(),
                iov_len: ent.payload_len,
            };
        }
    }

    *pool.queues[qid as usize].arena.lock().unwrap() = Some(arena.clone());
    *pool.queues[qid as usize].kmbuf.lock().unwrap() = kmbuf_q.clone();
    // MEM-1: publish the queue's dest-claim geometry (set exactly once —
    // workers run once per qid; `lease_dest_window` resolves claims by
    // address containment against this window).
    let _ = pool.queues[qid as usize].dest_window.set(DestWindow {
        base: arena.base,
        span: arena.span,
        stride: arena.stride,
        arena: Arc::clone(&arena),
        kmbuf: kmbuf_q.clone(),
    });

    // REGISTER shape per mode: classical = 2 iovecs (header + payload);
    // kmbuf = no iovecs, `init.flags = FUSE_URING_BUF_RING`,
    // `sqe->buf_index = ent_idx` (the ent's fixed_buf_id).
    let reg_init_flags: u16 = match pool.buffer_mode {
        TransportBufferMode::BufRing => kmbuf::init_flags(true, false),
        TransportBufferMode::UserEnts => 0,
    };
    let reg_iov = |ent: &Ent| -> Option<(*const libc::iovec, u32)> {
        match pool.buffer_mode {
            TransportBufferMode::BufRing => None,
            TransportBufferMode::UserEnts => Some((ent.iov.as_ptr(), 2)),
        }
    };
    let reg_buf_index = |idx: usize| -> u16 {
        match pool.buffer_mode {
            TransportBufferMode::BufRing => idx as u16,
            TransportBufferMode::UserEnts => 0,
        }
    };

    // FUSE-2: the queue's slot state machine — one state per ring ent,
    // owned exclusively by THIS thread (see [`SlotState`] for the
    // exactly-one-reply invariant it enforces).
    let mut slots = SlotTable::new(depth);

    for (idx, ent) in ents.iter().enumerate() {
        push_cmd(
            &mut ring,
            FUSE_IO_URING_CMD_REGISTER,
            qid,
            0,
            reg_iov(ent),
            encode_user_data(RingOp::Register, idx),
            reg_init_flags,
            reg_buf_index(idx),
        )
        .map_err(|e| io::Error::other(format!("push REGISTER ent={idx}: {e}")))?;
        slots.on_register_submitted(idx);
        pool.stats_register.fetch_add(1, Ordering::Relaxed);
        STATS_REGISTER.fetch_add(1, Ordering::Relaxed);
    }
    {
        let poll_e = opcode::PollAdd::new(types::Fixed(1), libc::POLLIN as _)
            .build()
            .user_data(UD_POLL);
        unsafe {
            ring.submission()
                .push(&Entry128::from(poll_e))
                .map_err(|_| io::Error::other("sq full (poll)"))?;
        }
    }
    ring.submit()
        .map_err(|e| io::Error::other(format!("submit REGISTER batch: {e}")))?;
    pool.queues_registered.fetch_add(1, Ordering::AcqRel);

    // §5.3 D3.a (S2) submit economy: the passes below PUSH their SQEs
    // (commits, poll re-arms, re-REGISTERs) through the batched helpers and
    // share ONE syscall — the loop-bottom `submit_and_wait(1)` flushes the
    // batch on its way into the wait (one `io_uring_enter` = submission +
    // wait). Only the syscall is shared: the §5.4 lease re-arm gate still
    // runs per ent *before* its SQE is pushed.
    let mut batch = SubmitBatch::default();
    // Row 7: sticky across passes — see the re-arm site below.
    let mut need_repoll_sticky = false;

    while pool.active.load(Ordering::Relaxed) {
        // Drain the eventfd FIRST. The wake-fd PollAdd re-arm is deferred to
        // the loop-bottom submit_and_wait (S2), so during the passes below
        // the poll may be unarmed — consuming a wake AFTER scanning its
        // producer would strand that producer until an unrelated event (a
        // stuck FUSE reply). Order closes it: every wake producer publishes
        // its state BEFORE writing the eventfd (submit_reply: channel send →
        // arm → write; lease drop: refs release → arm → write; shutdown:
        // active store → write), so a wake consumed here means the state is
        // already visible to the drains below — and any wake arriving AFTER
        // this drain leaves the counter nonzero, which completes the
        // (level-triggered) PollAdd the moment submit_and_wait arms it.
        let mut buf = [0u8; 8];
        loop {
            let n = unsafe { libc::read(wake_fd, buf.as_mut_ptr().cast(), 8) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if n == 0 {
                break;
            }
        }
        // L3 lever B — disarm the wake coalescer AT THIS POINT: after the
        // eventfd drain, before any producer-state scan below. Disarming
        // before the drain leaves the flag armed after the pass while the
        // covering write was just consumed — the next producer elides
        // against it and strands on a zero counter. Disarming after the
        // scans loses the happens-before edge that makes a covered
        // publication visible to THIS pass's scans. The drain→disarm→scan
        // order is loom-verified (`wake_coalescer_*` models; weakening
        // evidence in the model docs).
        wake_coalescer.disarm();

        // Drain commits for this queue only (no demux). §5.4 re-arm gate: a
        // COMMIT_AND_FETCH both writes the reply into the ent payload and
        // re-arms the registered buffers for the kernel — never legal while
        // a payload lease is live. Gate every commit; park the message when
        // leased and rely on the lease drop's eventfd wake.
        while let Ok(msg) = commit_rx.try_recv() {
            let idx = msg.ent_idx as usize;
            if idx >= ents.len() {
                warn!("fuse-over-uring qid={qid}: commit for bad ent {idx}");
                TRANSPORT_REPLIES_REFUSED_STALE.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // FUSE-2 / FUSE-3e: the slot decides. A commit that does not
            // address the request this slot currently owes (double reply,
            // late reply, a reply for a displaced request) is refused
            // loud-never-fatally — never allowed to overwrite a live one,
            // and never turned into a second COMMIT_AND_FETCH.
            if slots.admit_commit(idx, msg.commit_id) == CommitAdmit::RefuseStale {
                warn!(
                    "fuse-over-uring qid={qid} ent={idx}: refusing a stale reply \
                     (cid={} state={:?}) — the slot does not owe it",
                    msg.commit_id,
                    slots.state(idx)
                );
                continue;
            }
            match lease_states[idx].try_commit() {
                CommitGate::Ready => {
                    xport_dbg!("[XPORT] commit qid={qid} ent={idx} cid={}", msg.commit_id);
                    apply_reply(&mut ents[idx], &msg.header, &msg.reply_body);
                    batch.note_commit_opcode(ents[idx].last_opcode);
                    submit_commit(
                        &mut ring,
                        &mut batch,
                        &mut slots,
                        pool.slot_watch_cell(qid, idx),
                        qid,
                        idx,
                        msg.commit_id,
                    )?;
                }
                CommitGate::Parked => {
                    xport_dbg!(
                        "[XPORT] commit-parked qid={qid} ent={idx} cid={}",
                        msg.commit_id
                    );
                    TRANSPORT_PARKED_COMMITS.fetch_add(1, Ordering::Relaxed);
                    slots.on_commit_parked(idx);
                    parked_msgs[idx] = Some(msg);
                }
            }
        }

        // Parked scan (runs on every wake path — lease-drop eventfd, new
        // CQEs, commit sends — and always before the worker can sleep in
        // submit_and_wait): un-park and commit every ent whose lease is
        // gone. try_unpark re-proves refs == 0, so the payload write below
        // cannot alias a live lease.
        for idx in 0..ents.len() {
            if parked_msgs[idx].is_some() && lease_states[idx].try_unpark() {
                let msg = parked_msgs[idx].take().expect("checked is_some");
                xport_dbg!(
                    "[XPORT] commit-unparked qid={qid} ent={idx} cid={}",
                    msg.commit_id
                );
                apply_reply(&mut ents[idx], &msg.header, &msg.reply_body);
                batch.note_commit_opcode(ents[idx].last_opcode);
                submit_commit(
                    &mut ring,
                    &mut batch,
                    &mut slots,
                    pool.slot_watch_cell(qid, idx),
                    qid,
                    idx,
                    msg.commit_id,
                )?;
            }
        }

        // Exit promptly when another worker/watch already shut us down (wake_fd).
        if !pool.active.load(Ordering::Relaxed) {
            break;
        }

        // ONE syscall for everything pushed above: submit_and_wait both
        // flushes the batch (recorded here) and parks for the next event.
        // commit_flush sampling: time the syscall only when it is
        // provably non-blocking (CQ already non-empty — the saturated
        // passes); an idle pass's duration is park time, not submission
        // work (see TransportPhase::CommitFlush).
        let (cf_reads, cf_writes) = batch.note_flush();
        let cf_t0 = ((cf_reads || cf_writes) && {
            let mut cq = ring.completion();
            cq.sync();
            !cq.is_empty()
        })
        .then(Instant::now);
        // FUSE-3a: an ent waiting out its REGISTER backoff needs the
        // loop to come back even on a queue with no traffic — otherwise
        // its retry waits for an unrelated CQE that may never arrive.
        // Bound the wait by the nearest retry deadline (and only then;
        // the steady-state path keeps today's plain `submit_and_wait`).
        let wait_result = match slots.next_retry_deadline() {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                let ts = types::Timespec::new()
                    .sec(left.as_secs())
                    .nsec(left.subsec_nanos());
                let args = types::SubmitArgs::new().timespec(&ts);
                match ring.submitter().submit_with_args(1, &args) {
                    // A timed-out wait is the retry tick, not an error.
                    Err(e) if e.raw_os_error() == Some(libc::ETIME) => Ok(0),
                    other => other,
                }
            }
            None => ring.submit_and_wait(1),
        };
        match wait_result {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) if FuseOverUring::is_disconnect_errno(e.raw_os_error().unwrap_or(0)) => {
                info!("fuse-over-uring qid={qid}: submit_and_wait disconnect ({e}); shutting down");
                pool.shutdown();
                break;
            }
            Err(e) => return Err(e),
        }
        if let Some(t0) = cf_t0 {
            record_commit_flush(cf_reads, cf_writes, t0.elapsed());
        }

        if !pool.active.load(Ordering::Relaxed) {
            break;
        }

        let completed: Vec<(u64, i32, u32)> = {
            let mut cq = ring.completion();
            cq.sync();
            cq.map(|c| (c.user_data(), c.result(), c.flags())).collect()
        };

        let mut resubmit = Vec::new();
        let mut disconnect = false;
        for (user_data, res, cqe_flags) in completed {
            // FUSE-2 row 6: the op class rides `user_data`, so an errored
            // COMMIT is never mistaken for an errored REGISTER (which is
            // how an EAGAIN'd commit used to discard the reply
            // `apply_reply` had already written into the ent).
            let Some((op, ent_idx)) = decode_user_data(user_data) else {
                // wake_fd poll completed — re-arm (or exit if inactive)
                need_repoll_sticky = true;
                continue;
            };
            if ent_idx >= ents.len() {
                warn!("fuse-over-uring qid={qid}: CQE for out-of-range ent {ent_idx}");
                continue;
            }
            if res < 0 {
                let err = -res;
                pool.stats_cqe_err.fetch_add(1, Ordering::Relaxed);
                STATS_CQE_ERR.fetch_add(1, Ordering::Relaxed);
                xport_dbg!("[XPORT] cqe-err qid={qid} ent={ent_idx} op={op:?} err={err}");
                // Kernel abort/unmount (dev_uring.c): -ENOTCONN on entry teardown /
                // cancel; -ECONNABORTED when abort_with_err is set.
                if FuseOverUring::is_disconnect_errno(err) {
                    info!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: disconnect CQE err={err}; shutting down"
                    );
                    disconnect = true;
                    break;
                }
                if err == libc::ENOTSUP || err == libc::EINVAL || err == libc::ENOSYS {
                    error!("fuse-over-uring: kernel rejected protocol err={err}");
                    pool.shutdown();
                    return Err(io::Error::from_raw_os_error(err));
                }
                match op {
                    // Row 6: a transient COMMIT failure re-commits. The
                    // ent still holds the applied reply, so re-pushing
                    // the same COMMIT_AND_FETCH is the whole recovery —
                    // re-REGISTERing here would discard that reply and
                    // leave the caller waiting forever.
                    RingOp::Commit if err == libc::EAGAIN || err == libc::EINTR => {
                        let commit_id = match slots.state(ent_idx) {
                            SlotState::Replied { commit_id } => Some(commit_id),
                            _ => None,
                        };
                        match commit_id {
                            Some(cid) => {
                                warn!(
                                    "fuse-over-uring qid={qid} ent={ent_idx}: COMMIT err={err}; \
                                     re-committing cid={cid} (reply already applied)"
                                );
                                slots.on_commit_retry(ent_idx);
                                submit_commit(
                                    &mut ring,
                                    &mut batch,
                                    &mut slots,
                                    pool.slot_watch_cell(qid, ent_idx),
                                    qid,
                                    ent_idx,
                                    cid,
                                )?;
                            }
                            None => {
                                warn!(
                                    "fuse-over-uring qid={qid} ent={ent_idx}: COMMIT err={err} \
                                     with no reply in flight ({:?}); re-REGISTER",
                                    slots.state(ent_idx)
                                );
                                resubmit.push(ent_idx);
                            }
                        }
                    }
                    // A COMMIT that failed for a non-transient reason:
                    // the kernel discarded the reply. Nothing can be
                    // committed for the (already answered) request, so
                    // reclaim the ent by re-REGISTERing it.
                    RingOp::Commit => {
                        warn!(
                            "fuse-over-uring qid={qid} ent={ent_idx}: COMMIT cqe err={err}; \
                             reclaim entry"
                        );
                        resubmit.push(ent_idx);
                    }
                    // Row 5: a REGISTER error on a slot that still owes a
                    // reply must SYNTHESIZE that reply before the ent is
                    // reclaimed — the historical `pending.retain(...)`
                    // dropped it silently. FUSE-3a: back off, and retire
                    // the ent after a bounded number of failures instead
                    // of spinning an unthrottled re-REGISTER loop.
                    RingOp::Register => {
                        if fail_ent(
                            &mut ring,
                            &mut batch,
                            &mut slots,
                            &mut ents[ent_idx],
                            &lease_states[ent_idx],
                            pool.slot_watch_cell(qid, ent_idx),
                            qid,
                            ent_idx,
                            libc::EIO,
                        )? {
                            // A synthesized commit is now in flight for
                            // this ent; the kernel re-arms it.
                            continue;
                        }
                        match slots.note_register_failure(ent_idx, Instant::now()) {
                            RegisterAction::Retry { after } => {
                                warn!(
                                    "fuse-over-uring qid={qid} ent={ent_idx}: REGISTER err={err}; \
                                     retry in {after:?}"
                                );
                                if after.is_zero() {
                                    resubmit.push(ent_idx);
                                }
                                // A nonzero backoff is served by the
                                // backoff pass below (it also bounds the
                                // ring wait so an idle queue still
                                // retries).
                            }
                            RegisterAction::Retire => {
                                error!(
                                    "fuse-over-uring qid={qid} ent={ent_idx}: REGISTER failed \
                                     {REGISTER_RETRY_MAX}× (last err={err}); retiring the ent"
                                );
                                if slots.all_retired() {
                                    error!(
                                        "fuse-over-uring qid={qid}: every ring ent retired — \
                                         the queue can no longer serve; failing the session"
                                    );
                                    pool.shutdown();
                                    return Err(io::Error::from_raw_os_error(err));
                                }
                            }
                        }
                    }
                }
                continue;
            }
            if op == RingOp::Register {
                slots.note_register_success(ent_idx);
            }
            // kmbuf attachment law (2026-08-04): a flagged CQE re-points
            // the ent's payload buffer to the freshly-selected kernel
            // buffer; an unflagged one keeps the current attachment (the
            // kernel's reuse case). NUMA locality is re-derived per
            // attachment (bid-indexed buffers).
            if let Some(kq) = &kmbuf_q {
                match kq.note_delivery(ent_idx, cqe_flags) {
                    Some((p, len)) => {
                        if ents[ent_idx].payload_ptr != p {
                            ents[ent_idx].payload_ptr = p;
                            ents[ent_idx].payload_len = len;
                            ents[ent_idx].node = arena.node_of_ptr(p);
                        }
                    }
                    None if cqe_flags & kmbuf::IORING_CQE_F_BUFFER != 0 => {
                        // Flagged with an out-of-range bid: protocol
                        // breach — fail loud, never index out of the
                        // region.
                        error!(
                            "fuse-over-uring qid={qid} ent={ent_idx}: kmbuf delivery                              carried an out-of-range buffer id (cqe flags {cqe_flags:#x});                              shutting down"
                        );
                        pool.shutdown();
                        return Err(io::Error::other("kmbuf buffer id out of range"));
                    }
                    None => {} // nothing ever attached — payload-less traffic
                }
            }
            // Kernel sets commit_id = unique when delivering a request.
            let unique = u64::from_le_bytes(ents[ent_idx].hdr().in_out[8..16].try_into().unwrap());
            let mut commit_id = ents[ent_idx].hdr().ring_ent_in_out.commit_id;
            if commit_id == 0 {
                // Fall back to unique — some paths only fill in_out.
                commit_id = unique;
            }
            if unique == 0 {
                // Prefer COMMIT with commit_id if the kernel filled it — re-REGISTER
                // alone leaves USERSPACE entries and permanent waiting/EBUSY umount.
                let cid = ents[ent_idx].hdr().ring_ent_in_out.commit_id;
                if cid != 0 {
                    warn!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: unique=0 commit_id={cid}; force EIO COMMIT"
                    );
                    xport_dbg!("[XPORT] unique0-force-commit qid={qid} ent={ent_idx} cid={cid}");
                    // Delivery on this ent implies its previous commit passed
                    // the refs == 0 gate; header-only reply, payload untouched.
                    debug_assert!(!lease_states[ent_idx].leased());
                    slots.on_deliver_degenerate(ent_idx, cid);
                    apply_reply(
                        &mut ents[ent_idx],
                        &error_out_header(0, libc::EIO),
                        &Bytes::new(),
                    );
                    submit_commit(
                        &mut ring,
                        &mut batch,
                        &mut slots,
                        pool.slot_watch_cell(qid, ent_idx),
                        qid,
                        ent_idx,
                        cid,
                    )?;
                } else {
                    warn!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: unique=0 commit_id=0; re-REGISTER"
                    );
                    xport_dbg!("[XPORT] unique0-re-register qid={qid} ent={ent_idx}");
                    resubmit.push(ent_idx);
                }
                continue;
            }
            let opcode = u32::from_le_bytes(ents[ent_idx].hdr().in_out[4..8].try_into().unwrap());
            let payload_sz = ents[ent_idx].hdr().ring_ent_in_out.payload_sz as usize;
            ents[ent_idx].last_opcode = opcode;
            if payload_sz > 0 && !ents[ent_idx].has_payload_buf() {
                // kmbuf: request payload announced but no buffer was ever
                // attached — the attachment law is broken; serving a
                // fabricated payload would corrupt data. Fail loud.
                error!(
                    "fuse-over-uring qid={qid} ent={ent_idx}: delivery announced                      {payload_sz} payload bytes with no attached buffer                      (kmbuf attachment law violated); shutting down"
                );
                pool.shutdown();
                return Err(io::Error::other("kmbuf delivery without attached buffer"));
            }
            let mut header_and_op =
                Vec::with_capacity(FUSE_IN_HEADER_SIZE + FUSE_URING_OP_IN_OUT_SZ);
            header_and_op.extend_from_slice(&ents[ent_idx].hdr().in_out[..FUSE_IN_HEADER_SIZE]);
            header_and_op.extend_from_slice(&ents[ent_idx].hdr().op_in);
            let capped_sz = payload_sz.min(ents[ent_idx].payload_len);
            // §5.4: FUSE_WRITE payloads ride a zero-copy lease over the
            // registered buffer (kills the 1 MiB copy + alloc per write
            // request, audit #1); the commit gate above defers the ent's
            // re-arm until the lease drops. FORGET/BATCH_FORGET are
            // auto-committed below *before* the session consumes the payload
            // — leasing them would hand the session a buffer the kernel is
            // already refilling — and non-write opcodes carry small payloads
            // (names, xattrs): both keep the copy.
            let payload = if opcode == FUSE_WRITE_OPCODE && capped_sz > 0 {
                let state = Arc::clone(&lease_states[ent_idx]);
                let prev = state.acquire();
                debug_assert_eq!(prev, 0, "delivery on a still-leased ent");
                TRANSPORT_PAYLOAD_LEASES.fetch_add(1, Ordering::Relaxed);
                TRANSPORT_LEASES_OUTSTANDING.fetch_add(1, Ordering::Relaxed);
                // K1 crossing estimate: the kernel copied this payload
                // from the app on ≈ CPU qid (queue selection is by
                // requester CPU) into the buffer's actual node
                // (ent-static classically; per-attachment on kmbuf —
                // `ents[..].node` tracks both).
                numa_classify_pass(queue_node, ents[ent_idx].node, capped_sz);
                Bytes::from_owner(EntPayloadLease {
                    arena: Arc::clone(&arena),
                    state,
                    ptr: ents[ent_idx].payload_ptr as *const u8,
                    len: capped_sz,
                    born: Instant::now(),
                })
            } else {
                Bytes::copy_from_slice(&ents[ent_idx].payload()[..capped_sz])
            };

            // FUSE-2: the slot now owes exactly one commit. A delivery
            // onto a slot that STILL owes one (row 10's overwrite class,
            // and row 11's `fuse_resend` double-delivery shape) is
            // reported loudly and counted on the must-stay-0 tripwire —
            // the displaced request's commit id is gone with it, so it
            // cannot be answered, and pretending otherwise (what
            // `pending.insert` did) hides a wedged caller.
            if let DeliverOutcome::DisplacedRequest { unique: lost } =
                slots.on_deliver(ent_idx, unique, commit_id)
            {
                error!(
                    "fuse-over-uring qid={qid} ent={ent_idx}: kernel delivered unique={unique} \
                     onto a slot still owing a reply for unique={lost} — that request can no \
                     longer be answered (transport_requests_abandoned)"
                );
            }
            pool.publish_slot_owed(qid, ent_idx, unique);

            pool.stats_requests.fetch_add(1, Ordering::Relaxed);
            STATS_REQUESTS.fetch_add(1, Ordering::Relaxed);
            debug!(
                qid,
                ent_idx, unique, commit_id, payload_sz, opcode, "fuse-over-uring inbound request"
            );
            xport_dbg!(
                "[XPORT] deliver qid={qid} ent={ent_idx} unique={unique} op={opcode} cid={commit_id} psz={payload_sz}"
            );

            // FUSE_FORGET (2) / FUSE_BATCH_FORGET (42) are "no reply" on classical.
            // Over-uring still holds the ring entry in USERSPACE until COMMIT.
            // Commit *immediately* here (do not wait for the session task).
            // Session still runs forget accounting from `inbound`; it must not reply.
            const FUSE_FORGET: u32 = 2;
            const FUSE_BATCH_FORGET: u32 = 42;
            if matches!(opcode, FUSE_FORGET | FUSE_BATCH_FORGET) {
                xport_dbg!("[XPORT] autocommit-forget qid={qid} ent={ent_idx} unique={unique}");
                // Deliver for nlookup accounting only — the session never
                // replies to a forget; this ent is committed right here,
                // which IS its one commit.
                let pushed = pool.inbound[qid as usize].push(InboundUringReq {
                    header_and_op,
                    payload,
                    unique,
                    slot: ReplySlot::Classical,
                    arrived_ns: crate::raw::read_phase::transport_now_ns(),
                });
                if pushed.is_err() {
                    // Forget accounting is lost (the session is gone), but
                    // the ent must still be committed — that is what the
                    // auto-commit below does.
                    warn!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: forget delivery dropped \
                         (session inbound closed); committing the ent anyway"
                    );
                }
                // FORGET payloads are copies (never leased) and this ent's
                // previous commit passed the refs == 0 gate: the immediate
                // auto-commit below cannot alias a live lease. Its SQE rides
                // the shared loop-bottom flush like every other commit.
                debug_assert!(!lease_states[ent_idx].leased());
                apply_reply(
                    &mut ents[ent_idx],
                    &error_out_header(unique, 0),
                    &Bytes::new(),
                );
                submit_commit(
                    &mut ring,
                    &mut batch,
                    &mut slots,
                    pool.slot_watch_cell(qid, ent_idx),
                    qid,
                    ent_idx,
                    commit_id,
                )?;
                pool.stats_replies.fetch_add(1, Ordering::Relaxed);
                STATS_REPLIES.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // The request carries its own reply address (FUSE-2 ⊕
            // PERF-16). The historical map insert had to happen BEFORE
            // the request was exposed, or a fast handler's reply missed
            // it and the kernel kept `waiting ≥ 1` forever (plain
            // `umount` EBUSY, seen after a full pjdfstest run). That race
            // is now structurally impossible: there is nothing to insert.
            //
            // FUSE-2 row 4: a push onto a closed session queue used to be
            // `let _ = tx.send(req)` while the worker went on believing
            // the request was in flight. Now a failed push is a non-reply
            // exit like any other and routes through `fail_ent`.
            if pool.inbound[qid as usize]
                .push(InboundUringReq {
                    header_and_op,
                    payload,
                    unique,
                    slot: ReplySlot::Ring {
                        qid,
                        ent_idx: ent_idx as u16,
                        commit_id,
                    },
                    arrived_ns: crate::raw::read_phase::transport_now_ns(),
                })
                .is_err()
            {
                error!(
                    "fuse-over-uring qid={qid} ent={ent_idx}: session inbound queue is closed; \
                     synthesizing EIO for unique={unique} (row 4)"
                );
                fail_ent(
                    &mut ring,
                    &mut batch,
                    &mut slots,
                    &mut ents[ent_idx],
                    &lease_states[ent_idx],
                    pool.slot_watch_cell(qid, ent_idx),
                    qid,
                    ent_idx,
                    libc::EIO,
                )?;
            }
        }
        if disconnect {
            pool.shutdown();
            break;
        }
        if !pool.active.load(Ordering::Relaxed) {
            break;
        }
        // FUSE-2 row 7: losing the wake-fd poll re-arm stops the queue
        // waking on `commit_tx` ENTIRELY — every reply on this queue is
        // then stranded until an unrelated CQE happens by. The flag is
        // STICKY: a failed push is retried on the next pass instead of
        // being swallowed by `let _ =`.
        if need_repoll_sticky {
            match push_poll_batched(&mut ring, &mut batch) {
                Ok(()) => need_repoll_sticky = false,
                Err(e) => warn!(
                    "fuse-over-uring qid={qid}: wake-poll re-arm push failed ({e}); \
                     retrying next pass"
                ),
            }
        }
        // Never re-REGISTER after a disconnect; only while still active.
        if !resubmit.is_empty() && pool.active.load(Ordering::Relaxed) {
            for ent_idx in resubmit {
                let iov = reg_iov(&ents[ent_idx]);
                // Re-REGISTER is a non-reply exit: an ent that still owes
                // a reply must be failed first (row 5) — `fail_ent` is a
                // no-op for slots that owe nothing.
                fail_ent(
                    &mut ring,
                    &mut batch,
                    &mut slots,
                    &mut ents[ent_idx],
                    &lease_states[ent_idx],
                    pool.slot_watch_cell(qid, ent_idx),
                    qid,
                    ent_idx,
                    libc::EIO,
                )?;
                if let Err(e) = push_cmd_batched(
                    &mut ring,
                    &mut batch,
                    FUSE_IO_URING_CMD_REGISTER,
                    qid,
                    0,
                    iov,
                    encode_user_data(RingOp::Register, ent_idx),
                    reg_init_flags,
                    reg_buf_index(ent_idx),
                ) {
                    warn!("fuse-over-uring qid={qid} ent={ent_idx}: re-REGISTER push failed ({e})");
                    continue;
                }
                slots.on_register_submitted(ent_idx);
            }
        }
        // FUSE-3a: serve any ent whose REGISTER backoff has expired. An
        // idle queue gets no CQEs, so the loop wait above is bounded
        // while a backoff is outstanding (see `wait_budget`).
        let now = Instant::now();
        for ent_idx in slots.register_retries_due(now) {
            let iov = reg_iov(&ents[ent_idx]);
            if push_cmd_batched(
                &mut ring,
                &mut batch,
                FUSE_IO_URING_CMD_REGISTER,
                qid,
                0,
                iov,
                encode_user_data(RingOp::Register, ent_idx),
                reg_init_flags,
                reg_buf_index(ent_idx),
            )
            .is_ok()
            {
                slots.on_register_submitted(ent_idx);
            }
        }
    }
    // Final drain of parked messages plus any pending commits (including the
    // FUSE_DESTROY reply) before exiting. §5.4: the no-write-while-leased
    // rule stays unconditional — it is not waived at shutdown. A still-leased
    // ent gets a short bounded wait for the lease to drop; if it survives,
    // the worker sends a header-only error reply (16-byte fuse_out_header,
    // payload_sz = 0 — the exact shape of the unique=0 recovery path), never
    // writing the leased payload region. The arena Arc keeps the leased
    // memory valid, so a pathological handler holding a payload past
    // shutdown degrades to a leaked buffer and a dropped reply body — never
    // a dangling pointer, and never a write into memory a live &[u8]
    // aliases.
    let mut final_msgs: Vec<CommitMsg> = parked_msgs.iter_mut().filter_map(|s| s.take()).collect();
    while let Ok(msg) = commit_rx.try_recv() {
        final_msgs.push(msg);
    }
    xport_dbg!(
        "[XPORT] worker-exit qid={qid} final_msgs={} active={}",
        final_msgs.len(),
        pool.active.load(Ordering::Relaxed)
    );
    let mut final_commits = 0;
    for msg in final_msgs {
        let idx = msg.ent_idx as usize;
        if idx >= ents.len() {
            continue;
        }
        let deadline = Instant::now() + Duration::from_millis(100);
        let mut free = lease_states[idx].try_unpark();
        while !free && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
            free = lease_states[idx].try_unpark();
        }
        if free {
            apply_reply(&mut ents[idx], &msg.header, &msg.reply_body);
        } else {
            warn!(
                "fuse-over-uring qid={qid} ent={idx}: payload lease still live at \
                 shutdown; committing header-only error reply"
            );
            let unique = if msg.header.len() >= 16 {
                u64::from_le_bytes(msg.header[8..16].try_into().unwrap())
            } else {
                0
            };
            // Header-only: apply_reply never touches the payload region when
            // the reply has no body beyond the 16-byte fuse_out_header.
            apply_reply(
                &mut ents[idx],
                &error_out_header(unique, libc::EIO),
                &Bytes::new(),
            );
        }
        if submit_commit(
            &mut ring,
            &mut batch,
            &mut slots,
            pool.slot_watch_cell(qid, idx),
            qid,
            idx,
            msg.commit_id,
        )
        .is_ok()
        {
            final_commits += 1;
        }
    }
    // FUSE-2 row 8: every slot that STILL owes a reply gets one here.
    // `shutdown()` used to `pending.clear()` — reachable from entirely
    // non-fatal causes (a `connection_watch` EBADF, a kernel protocol
    // reject, any worker `Err`, plain `Drop`) — and every request in the
    // map vanished, parking its caller in uninterruptible sleep with an
    // `umount` that returns EBUSY. The drain is the teardown half of the
    // exactly-one-reply invariant.
    let owed: Vec<(usize, u64, u64)> = slots.owing().collect();
    for (idx, unique, commit_id) in owed {
        warn!(
            "fuse-over-uring qid={qid} ent={idx}: unanswered request unique={unique} at \
             teardown; synthesizing EIO (row 8)"
        );
        let _ = commit_id;
        match fail_ent(
            &mut ring,
            &mut batch,
            &mut slots,
            &mut ents[idx],
            &lease_states[idx],
            pool.slot_watch_cell(qid, idx),
            qid,
            idx,
            libc::EIO,
        ) {
            Ok(true) => final_commits += 1,
            Ok(false) => {}
            Err(e) => {
                // The ring itself is gone: the request cannot be
                // answered at all — the must-stay-0 tripwire's honest
                // case.
                error!(
                    "fuse-over-uring qid={qid} ent={idx}: teardown commit push failed ({e}); \
                     unique={unique} abandoned"
                );
                slots.abandon(idx);
                pool.publish_slot_owed(qid, idx, 0);
            }
        }
    }
    // A loop exit between the drain passes and the loop-bottom
    // submit_and_wait leaves applied replies pushed but unsubmitted; flush
    // them together with the final commits — teardown must not drop a reply
    // that was already applied to its ent. (commit_flush deliberately
    // unsampled at teardown.)
    let _ = batch.note_flush();
    if final_commits > 0 {
        let _ = ring.submit_and_wait(final_commits);
    } else {
        // No final commits: still flush anything a truncated last pass (or
        // an EINTR'd submit) left in the SQ — a no-op enter when none.
        let _ = ring.submit();
    }
    debug!("fuse-over-uring qid={qid} worker exit");
    Ok(())
}

/// A bare 16-byte `fuse_out_header` carrying `-errno` (0 = success).
/// Every synthesized reply in the transport has exactly this shape: it
/// touches only the ent's separately-allocated header struct, so it is
/// legal even while a payload lease is live (§5.4).
fn error_out_header(unique: u64, errno: i32) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&16u32.to_le_bytes());
    out[4..8].copy_from_slice(&(-errno).to_le_bytes());
    out[8..16].copy_from_slice(&unique.to_le_bytes());
    out
}

/// Push the COMMIT_AND_FETCH for `ent_idx` and move its slot to
/// [`SlotState::Replied`] — the ONE place a slot leaves `Delivered`.
///
/// The reply bytes must already be in the ent (`apply_reply`).
#[allow(clippy::too_many_arguments)] // ring + batch + slot state + the ent's address
fn submit_commit(
    ring: &mut Ring,
    batch: &mut SubmitBatch,
    slots: &mut SlotTable,
    watch: Option<&SlotWatch>,
    qid: u16,
    ent_idx: usize,
    commit_id: u64,
) -> io::Result<()> {
    push_cmd_batched(
        ring,
        batch,
        FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
        qid,
        commit_id,
        None,
        encode_user_data(RingOp::Commit, ent_idx),
        0,
        0,
    )?;
    slots.on_commit_submitted(ent_idx, commit_id);
    if let Some(w) = watch {
        w.clear();
    }
    Ok(())
}

/// **The one non-reply exit** (FUSE-2). Every path that would otherwise
/// consume a request without answering it routes here: CQE-error
/// reclaim, a failed inbound push, a re-REGISTER over a live request,
/// and the teardown drain.
///
/// Returns `true` when a synthesized commit was submitted (the slot owed
/// a reply), `false` when the slot owed nothing. The reply is the
/// header-only `-errno` shape, so it is safe even against a live payload
/// lease — the ent's payload region is never written.
#[allow(clippy::too_many_arguments)] // ring + batch + slot state + the ent's address
fn fail_ent(
    ring: &mut Ring,
    batch: &mut SubmitBatch,
    slots: &mut SlotTable,
    ent: &mut Ent,
    lease: &EntLeaseState,
    watch: Option<&SlotWatch>,
    qid: u16,
    ent_idx: usize,
    errno: i32,
) -> io::Result<bool> {
    let FailOutcome::Synthesize { unique, commit_id } = slots.fail_ent(ent_idx) else {
        return Ok(false);
    };
    // A live lease means a handler still holds the payload; the
    // header-only reply below never touches it (§5.4), so the
    // synthesized error is always legal.
    let _ = lease;
    warn!(
        "fuse-over-uring qid={qid} ent={ent_idx}: synthesizing errno={errno} for \
         unique={unique} (no handler reply is coming)"
    );
    xport_dbg!("[XPORT] fail-ent qid={qid} ent={ent_idx} unique={unique} errno={errno}");
    apply_reply(ent, &error_out_header(unique, errno), &Bytes::new());
    match submit_commit(ring, batch, slots, watch, qid, ent_idx, commit_id) {
        Ok(()) => Ok(true),
        Err(e) => {
            // The commit could not even be pushed: this request will
            // never be answered — the must-stay-0 tripwire's honest case
            // (`fail_ent` already moved the slot out of `Delivered`, so
            // the count is made here rather than by `abandon`).
            note_request_abandoned();
            slots.abandon(ent_idx);
            if let Some(w) = watch {
                w.clear();
            }
            Err(e)
        }
    }
}

/// Place a classical fuse reply (`fuse_out_header` || body) into the ring entry
/// the way libfuse/`send_reply_uring` does: header in `in_out`, body in payload.
///
/// §5.4 aliasing contract: any call that can write the payload region
/// (header > 16 bytes or a non-empty body) requires the ent's lease
/// refs == 0, proven by the caller via `CommitGate::Ready` / `try_unpark`.
/// A 16-byte header-only reply touches only the (separately allocated)
/// header struct and is safe even while a lease lives — the shutdown drain
/// relies on exactly that.
fn apply_reply(ent: &mut Ent, header: &[u8], body: &Bytes) {
    const OUT_HDR: usize = 16; // sizeof(fuse_out_header)
                               // Clear header region so stale request bytes cannot leak into the reply.
    ent.hdr_mut().in_out = [0; FUSE_URING_IN_OUT_HEADER_SZ];
    if header.len() < OUT_HDR {
        // Degenerate — treat as IO error header.
        ent.hdr_mut().in_out[..4].copy_from_slice(&((OUT_HDR as u32).to_le_bytes()));
        ent.hdr_mut().in_out[4..8].copy_from_slice(&(-libc::EIO).to_le_bytes());
        ent.hdr_mut().ring_ent_in_out.payload_sz = 0;
        return;
    }
    ent.hdr_mut().in_out[..OUT_HDR].copy_from_slice(&header[..OUT_HDR]);

    if (header.len() > OUT_HDR || !body.is_empty()) && !ent.has_payload_buf() {
        // kmbuf mode: a body-carrying reply on an ent with no attached
        // buffer is structurally unreachable (the kernel attaches a
        // buffer to every request with out args); if it ever fires, the
        // reply degrades to a loud header-only EIO — never an OOB write.
        error!(
            "fuse-over-uring: body-carrying reply on an ent with no              payload buffer (kmbuf attachment law violated) — EIO"
        );
        ent.hdr_mut().in_out[..4].copy_from_slice(&((OUT_HDR as u32).to_le_bytes()));
        ent.hdr_mut().in_out[4..8].copy_from_slice(&(-libc::EIO).to_le_bytes());
        ent.hdr_mut().ring_ent_in_out.payload_sz = 0;
        return;
    }

    let mut payload_len = 0;
    if header.len() > OUT_HDR {
        let extra = &header[OUT_HDR..];
        let n = extra.len().min(ent.payload_len);
        ent.payload_mut()[..n].copy_from_slice(&extra[..n]);
        payload_len = n;
    }

    let body_len = body.len().min(ent.payload_len - payload_len);
    if body_len > 0 && body.as_ptr() != unsafe { ent.payload_ptr.add(payload_len) as *const u8 } {
        ent.payload_mut()[payload_len..payload_len + body_len].copy_from_slice(&body[..body_len]);
        // Reply-serve locality (the worker executes this copy; the
        // zero-copy serve-into-payload path above elides it and is
        // deliberately NOT counted — no CPU pass happened here).
        numa_classify_pass(
            crate::numa_core::topology().current_node(),
            ent.node,
            body_len,
        );
    }
    payload_len += body_len;

    ent.hdr_mut().ring_ent_in_out.payload_sz = payload_len as u32;
}

/// Build one FUSE uring-cmd SQE (SQE128) — extracted from [`push_cmd`] so
/// the wire encoding is unit-testable byte-for-byte (the kmbuf REGISTER
/// shape: `init.flags` inside the 80-byte cmd area, `sqe->buf_index` at
/// offset 40, no iovecs).
fn build_cmd_entry(
    cmd_op: u32,
    qid: u16,
    commit_id: u64,
    iov: Option<(*const libc::iovec, u32)>,
    user_data: u64,
    init_flags: u16,
    buf_index: u16,
) -> Entry128 {
    let mut cmd = [0u8; 80];
    let req = FuseUringCmdReq {
        flags: 0,
        commit_id,
        qid,
        init_flags,
        init_queue_depth: 0,
        padding: [0; 2],
    };
    // SAFETY: FuseUringCmdReq is repr(C), 24 bytes; rest of cmd stays zero.
    unsafe {
        std::ptr::write(cmd.as_mut_ptr().cast::<FuseUringCmdReq>(), req);
    }

    let mut entry: Entry128 = opcode::UringCmd80::new(types::Fixed(0), cmd_op)
        .cmd(cmd)
        .build()
        .user_data(user_data);

    if let Some((ptr, len)) = iov {
        // libfuse fuse_uring_register_ent:
        //   sqe->addr = (uint64_t)ent->iov;  sqe->len = 2;
        // io_uring_sqe layout: addr @ +16, len @ +24 (first 64-byte SQE half of Entry128).
        //
        // SAFETY: Entry128 is (Entry, [u8;64]); Entry is the first 64 bytes of the SQE.
        unsafe {
            let base = (&mut entry as *mut Entry128 as *mut u8).add(16);
            std::ptr::write_unaligned(base as *mut u64, ptr as u64);
            std::ptr::write_unaligned(base.add(8) as *mut u32, len);
        }
    }

    if buf_index != 0 {
        // kmbuf REGISTER: `sqe->buf_index = ent->fixed_buf_id` (offset 40,
        // u16 — the `{ buf_index | buf_group }` union slot). Ent 0 needs
        // no write (the SQE is zeroed).
        //
        // SAFETY: offset 40 lies in the first 64-byte half of Entry128.
        unsafe {
            let base = &mut entry as *mut Entry128 as *mut u8;
            std::ptr::write_unaligned(base.add(40) as *mut u16, buf_index);
        }
    }

    entry
}

#[allow(clippy::too_many_arguments)] // one wire word per SQE field (see push_cmd_batched)
fn push_cmd(
    ring: &mut Ring,
    cmd_op: u32,
    qid: u16,
    commit_id: u64,
    iov: Option<(*const libc::iovec, u32)>,
    user_data: u64,
    init_flags: u16,
    buf_index: u16,
) -> io::Result<()> {
    let entry = build_cmd_entry(
        cmd_op, qid, commit_id, iov, user_data, init_flags, buf_index,
    );
    unsafe {
        ring.submission()
            .push(&entry)
            .map_err(|_| io::Error::other("submission queue full"))?;
    }
    Ok(())
}

/// FUSE-2 — the exactly-one-reply invariant, one leg per in-process
/// reachable row of the spec's eleven-row table (rows 1/2/3 live in
/// `session.rs`; row 11 — `fuse_resend` / `fiq->ops` switchover
/// double-delivery — needs a live mount and is a documented repro-port
/// exception, but its *shape* is pinned here as row 10's twin: a second
/// delivery onto a slot that still owes a reply).
#[cfg(test)]
mod slot_state_tests {
    use super::*;

    fn table(depth: usize) -> SlotTable {
        SlotTable::new(depth)
    }

    /// The happy path IS the invariant: a delivered request leaves
    /// `Delivered` only by a submitted commit.
    #[test]
    fn deliver_then_commit_walks_the_state_machine() {
        let mut t = table(4);
        assert_eq!(t.state(0), SlotState::Registered);
        assert_eq!(t.on_deliver(0, 100, 7), DeliverOutcome::Accepted);
        assert_eq!(
            t.state(0),
            SlotState::Delivered {
                unique: 100,
                commit_id: 7
            }
        );
        assert_eq!(t.admit_commit(0, 7), CommitAdmit::Accept);
        t.on_commit_submitted(0, 7);
        assert_eq!(
            t.state(0),
            SlotState::Replied { commit_id: 7 },
            "a submitted commit is the ONLY exit from Delivered"
        );
    }

    /// FUSE-2 row 10 (and row 11's in-process shape): a delivery onto a
    /// slot that still owes a reply must be refused-loud — the displaced
    /// request is counted on the must-stay-0 tripwire, never silently
    /// overwritten the way `pending.insert` did.
    #[test]
    fn second_delivery_on_an_owing_slot_reports_the_displaced_request() {
        let mut t = table(2);
        t.on_deliver(1, 500, 11);
        let before = transport_reply_integrity_stats().1;
        assert_eq!(
            t.on_deliver(1, 502, 12),
            DeliverOutcome::DisplacedRequest { unique: 500 },
            "the overwritten request must be reported, not lost silently"
        );
        assert!(
            transport_reply_integrity_stats().1 > before,
            "a displaced request must land on transport_requests_abandoned"
        );
        assert_eq!(
            t.state(1),
            SlotState::Delivered {
                unique: 502,
                commit_id: 12
            }
        );
    }

    /// FUSE-3e: a second commit for one ent must never overwrite the
    /// first. Loud-never-fatal — refuse and count.
    #[test]
    fn double_reply_is_refused_not_overwritten() {
        let mut t = table(2);
        t.on_deliver(0, 90, 3);
        assert_eq!(t.admit_commit(0, 3), CommitAdmit::Accept);
        t.on_commit_submitted(0, 3);
        let before = transport_reply_integrity_stats().2;
        assert_eq!(
            t.admit_commit(0, 3),
            CommitAdmit::RefuseStale,
            "the request was already answered — the second commit is stale"
        );
        assert!(transport_reply_integrity_stats().2 > before);
    }

    /// A commit whose `commit_id` is not the one the slot owes addresses
    /// a request that is gone (the misdelivery class the unique map
    /// could not detect).
    #[test]
    fn commit_for_a_foreign_commit_id_is_refused() {
        let mut t = table(2);
        t.on_deliver(0, 90, 3);
        assert_eq!(t.admit_commit(0, 4), CommitAdmit::RefuseStale);
        assert_eq!(
            t.state(0),
            SlotState::Delivered {
                unique: 90,
                commit_id: 3
            },
            "the refusal must leave the owed request intact"
        );
    }

    /// A parked commit (payload lease live, §5.4) still owes its reply,
    /// and a second commit arriving while it is parked is refused rather
    /// than overwriting the parked message (FUSE-3e's release-build
    /// silent-overwrite).
    #[test]
    fn parked_commit_still_owes_and_refuses_a_second() {
        let mut t = table(2);
        t.on_deliver(0, 77, 5);
        assert_eq!(t.admit_commit(0, 5), CommitAdmit::Accept);
        t.on_commit_parked(0);
        assert_eq!(
            t.state(0),
            SlotState::Parked {
                unique: 77,
                commit_id: 5
            }
        );
        assert_eq!(t.admit_commit(0, 5), CommitAdmit::RefuseStale);
        assert_eq!(
            t.owing().collect::<Vec<_>>(),
            vec![(0, 77, 5)],
            "a parked commit has not been submitted — the slot still owes"
        );
    }

    /// FUSE-2 row 5: the CQE-error reclaim path must synthesize a reply
    /// before re-REGISTERing. `fail_ent` is the ONE helper every
    /// non-reply exit routes through.
    #[test]
    fn fail_ent_synthesizes_for_an_owing_slot() {
        let mut t = table(2);
        t.on_deliver(0, 42, 9);
        let before = transport_reply_integrity_stats().0;
        assert_eq!(
            t.fail_ent(0),
            FailOutcome::Synthesize {
                unique: 42,
                commit_id: 9
            }
        );
        assert_eq!(
            t.state(0),
            SlotState::Replied { commit_id: 9 },
            "the synthesized commit is the slot's one exit from Delivered"
        );
        assert!(transport_reply_integrity_stats().0 > before);
        assert_eq!(
            t.fail_ent(0),
            FailOutcome::Nothing,
            "a slot that owes nothing must never produce a second commit"
        );
    }

    /// A parked slot fails the same way (teardown drain / lease wedge).
    #[test]
    fn fail_ent_synthesizes_for_a_parked_slot() {
        let mut t = table(1);
        t.on_deliver(0, 8, 8);
        t.admit_commit(0, 8);
        t.on_commit_parked(0);
        assert_eq!(
            t.fail_ent(0),
            FailOutcome::Synthesize {
                unique: 8,
                commit_id: 8
            }
        );
    }

    /// FUSE-2 row 8: teardown (`pending.clear()`) must not drop owed
    /// requests silently — the drain fails every owing slot, and what
    /// genuinely cannot be committed lands on the tripwire.
    #[test]
    fn abandon_counts_the_tripwire_and_clears_the_debt() {
        let mut t = table(2);
        t.on_deliver(0, 3, 3);
        let before = transport_reply_integrity_stats().1;
        t.abandon(0);
        assert!(
            transport_reply_integrity_stats().1 > before,
            "row 8: a request dropped at teardown must be counted, not vanish"
        );
        assert_eq!(t.owing().count(), 0);
    }

    /// FUSE-2 row 5 sibling: re-REGISTER must never be the exit from an
    /// owing slot. The worker has to `fail_ent` first — the state machine
    /// makes the omission observable.
    #[test]
    fn register_over_an_owing_slot_is_an_abandonment() {
        let mut t = table(1);
        t.on_deliver(0, 21, 4);
        let before = transport_reply_integrity_stats().1;
        t.on_register_submitted(0);
        assert_eq!(t.state(0), SlotState::Registered);
        assert!(
            transport_reply_integrity_stats().1 > before,
            "re-REGISTERing over a delivered request is exactly row 5's loss"
        );
    }

    /// FUSE-3a: bounded exponential backoff, then retirement.
    #[test]
    fn register_failures_back_off_then_retire() {
        let mut t = table(1);
        let t0 = Instant::now();
        let mut last = Duration::ZERO;
        for i in 1..REGISTER_RETRY_MAX {
            match t.note_register_failure(0, t0) {
                RegisterAction::Retry { after } => {
                    assert!(
                        after >= REGISTER_BACKOFF_BASE,
                        "failure {i} must back off at least one base step"
                    );
                    assert!(after >= last, "backoff must be monotonic");
                    assert!(after <= REGISTER_BACKOFF_MAX, "backoff must stay bounded");
                    last = after;
                }
                RegisterAction::Retire => panic!("retired early at failure {i}"),
            }
        }
        assert_eq!(
            t.note_register_failure(0, t0),
            RegisterAction::Retire,
            "the retry budget must be finite"
        );
        assert_eq!(t.state(0), SlotState::Retired);
        assert!(
            t.all_retired(),
            "a queue whose every ent retired can no longer serve — the session must fail"
        );
    }

    /// FUSE-3a: a success clears the failure history (so an occasional
    /// EBUSY never accumulates toward retirement).
    #[test]
    fn register_success_resets_the_backoff() {
        let mut t = table(1);
        let t0 = Instant::now();
        t.note_register_failure(0, t0);
        t.note_register_failure(0, t0);
        t.note_register_success(0);
        assert!(
            t.next_retry_deadline().is_none(),
            "a successful REGISTER clears the pending backoff"
        );
        match t.note_register_failure(0, t0) {
            RegisterAction::Retry { after } => assert_eq!(
                after, REGISTER_BACKOFF_BASE,
                "the next failure restarts at the base step"
            ),
            RegisterAction::Retire => panic!("one failure must not retire an ent"),
        }
    }

    /// FUSE-2 row 6: REGISTER and COMMIT CQEs must be distinguishable.
    /// `user_data == ent_idx` for both is why an `EAGAIN` COMMIT is
    /// re-REGISTERed today, discarding the reply already written into the
    /// ent.
    #[test]
    fn user_data_distinguishes_register_from_commit() {
        for ent in [0usize, 1, 31, 255] {
            let reg = encode_user_data(RingOp::Register, ent);
            let com = encode_user_data(RingOp::Commit, ent);
            assert_ne!(
                reg, com,
                "ent {ent}: the two op classes must not share a user_data word"
            );
            assert_eq!(decode_user_data(reg), Some((RingOp::Register, ent)));
            assert_eq!(decode_user_data(com), Some((RingOp::Commit, ent)));
            assert_ne!(reg, UD_POLL);
            assert_ne!(com, UD_POLL);
        }
        assert_eq!(
            decode_user_data(UD_POLL),
            None,
            "the wake-fd poll marker must stay distinct from every ent op"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flags2_bit() {
        assert_eq!(FUSE_OVER_IO_URING_FLAGS2, 1u32 << 9);
    }

    /// PERF-2 pool-level teardown law, restated on slot addressing
    /// (FUSE-2 ⊕ PERF-16): a pre-teardown reply commits (Ok); after
    /// teardown the queue worker's drain owns every owed slot, so a
    /// late reply reports NotFound — the drop-not-classical-write arm in
    /// `write_vectored`, which then counts it apart from delivered
    /// replies (FUSE-3b).
    #[test]
    fn teardown_hands_owed_slots_to_the_drain_so_late_replies_report() {
        let (pool, _rxs) = FuseOverUring::sim_inert_with_commit_rx(1);
        let slot = ReplySlot::Ring {
            qid: 0,
            ent_idx: 3,
            commit_id: 7,
        };
        pool.mark_ready();
        pool.submit_reply(slot, vec![0u8; 16], bytes::Bytes::new())
            .expect("pre-teardown reply must commit");
        pool.shutdown();
        let err = pool
            .submit_reply(
                ReplySlot::Ring {
                    qid: 0,
                    ent_idx: 4,
                    commit_id: 8,
                },
                vec![0u8; 16],
                bytes::Bytes::new(),
            )
            .expect_err("post-teardown reply must not race the worker's drain");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "the miss must be the NotFound drop shape (never a classical fallback)"
        );
    }

    /// A reply carrying no ring slot can never be committed — it is a
    /// classical delivery and belongs on the device write.
    #[test]
    fn classical_slot_never_commits() {
        let (pool, _rxs) = FuseOverUring::sim_inert_with_commit_rx(1);
        pool.mark_ready();
        let err = pool
            .submit_reply(ReplySlot::Classical, vec![0u8; 16], bytes::Bytes::new())
            .expect_err("a classical reply has no ring slot to address");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// PERF-2 pool-level idempotence law: `shutdown` gates its side
    /// effects on the `active` swap — racing teardowns from every worker
    /// clone decrement the session gauge exactly once and leave the
    /// liveness word stable.
    #[test]
    fn shutdown_is_idempotent() {
        let pool = FuseOverUring::sim_inert(2);
        pool.mark_ready();
        assert!(pool.is_ready() && pool.is_active());
        pool.shutdown();
        assert!(!pool.is_ready() && !pool.is_active());
        // Second (and racing Nth) shutdown: no panic, flags stable.
        pool.shutdown();
        pool.shutdown();
        assert!(!pool.is_ready() && !pool.is_active());
    }

    /// The venue predicates' truth table on the pool's own atomics
    /// (contract 2's pool half): `is_ready` = ready && active — never
    /// true pre-ready, never true post-shutdown.
    #[test]
    fn is_ready_requires_ready_and_active() {
        let pool = FuseOverUring::sim_inert(1);
        assert!(!pool.is_ready(), "fresh pool: active but not ready");
        assert!(pool.is_active());
        pool.mark_ready();
        assert!(pool.is_ready());
        pool.shutdown();
        assert!(!pool.is_ready());
        assert!(!pool.is_active());
    }

    #[test]
    fn test_header_sizes() {
        assert_eq!(std::mem::size_of::<FuseUringReqHeader>(), 128 + 128 + 32);
        assert_eq!(std::mem::size_of::<FuseUringCmdReq>(), 24);
    }

    #[test]
    fn test_write_opcode_matches_abi() {
        assert_eq!(FUSE_WRITE_OPCODE, 16, "linux/fuse.h FUSE_WRITE");
    }

    /// The kmbuf REGISTER wire shape, byte-for-byte (2026-08-04): the
    /// 80-byte cmd area starts at SQE offset 48 (SQE128), so
    /// `fuse_uring_cmd_req.qid` sits at 48+16, `init.flags` at 48+18,
    /// `init.queue_depth` at 48+20; `sqe->buf_index` is the u16 at
    /// offset 40; kmbuf REGISTERs carry NO iovecs (addr/len zero).
    /// Classical REGISTERs keep today's encoding exactly (flags 0,
    /// buf_index 0, iov at addr/len).
    #[test]
    fn test_cmd_entry_wire_encoding() {
        let iovs = [libc::iovec {
            iov_base: 0x1234_5000 as *mut libc::c_void,
            iov_len: 2,
        }];

        let read_u16 = |e: &Entry128, off: usize| -> u16 {
            // SAFETY: reading inside the 128-byte entry.
            unsafe {
                std::ptr::read_unaligned((e as *const Entry128 as *const u8).add(off) as *const u16)
            }
        };
        let read_u32 = |e: &Entry128, off: usize| -> u32 {
            // SAFETY: as above.
            unsafe {
                std::ptr::read_unaligned((e as *const Entry128 as *const u8).add(off) as *const u32)
            }
        };
        let read_u64 = |e: &Entry128, off: usize| -> u64 {
            // SAFETY: as above.
            unsafe {
                std::ptr::read_unaligned((e as *const Entry128 as *const u8).add(off) as *const u64)
            }
        };

        // Classical REGISTER: iovs at addr/len, no init flags, no buf_index.
        let e = build_cmd_entry(
            FUSE_IO_URING_CMD_REGISTER,
            3,
            0,
            Some((iovs.as_ptr(), 2)),
            7,
            0,
            0,
        );
        assert_eq!(
            read_u64(&e, 16),
            iovs.as_ptr() as u64,
            "sqe->addr = the iov ARRAY pointer (libfuse fuse_uring_register_ent)"
        );
        assert_eq!(read_u32(&e, 24), 2, "sqe->len = 2 iovecs");
        assert_eq!(read_u16(&e, 40), 0, "no buf_index");
        assert_eq!(read_u16(&e, 48 + 16), 3, "cmd_req.qid");
        assert_eq!(read_u16(&e, 48 + 18), 0, "cmd_req.init.flags empty");

        // kmbuf REGISTER: no iovecs, FUSE_URING_BUF_RING, buf_index = ent.
        let e = build_cmd_entry(
            FUSE_IO_URING_CMD_REGISTER,
            5,
            0,
            None,
            9,
            super::kmbuf::init_flags(true, false),
            11,
        );
        assert_eq!(read_u64(&e, 16), 0, "kmbuf REGISTER carries no iov ptr");
        assert_eq!(read_u32(&e, 24), 0, "kmbuf REGISTER carries no iov len");
        assert_eq!(read_u16(&e, 40), 11, "sqe->buf_index = fixed_buf_id");
        assert_eq!(read_u16(&e, 48 + 16), 5, "cmd_req.qid");
        assert_eq!(
            read_u16(&e, 48 + 18),
            super::kmbuf::FUSE_URING_BUF_RING,
            "cmd_req.init.flags = FUSE_URING_BUF_RING"
        );
        assert_eq!(read_u16(&e, 48 + 20), 0, "init.queue_depth zero (no zc)");

        // COMMIT_AND_FETCH is unchanged in every mode: commit_id at
        // cmd+8, zeros in the init union (wire-compatible with
        // pre-series kernels).
        let e = build_cmd_entry(
            FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
            2,
            0xdead_beef,
            None,
            1,
            0,
            0,
        );
        assert_eq!(read_u64(&e, 48 + 8), 0xdead_beef, "cmd_req.commit_id");
        assert_eq!(read_u16(&e, 48 + 18), 0, "init union zero on commits");
        assert_eq!(read_u16(&e, 40), 0, "no buf_index on commits");
    }

    // -----------------------------------------------------------------
    // L1 transport-concurrency policy (pure core). MiB payload = the
    // SqueezeFS shape (max_write 1 MiB = 256 kernel pages), planned at
    // the kernel-default `fs.fuse.max_pages_limit` (256) unless a test
    // says otherwise.
    // -----------------------------------------------------------------
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    const PAGE: usize = 4096;

    fn plan(
        nq: usize,
        env_q: Option<usize>,
        env_d: Option<usize>,
        cap: u64,
        mb: Option<u16>,
        ct: Option<u16>,
    ) -> TransportGeometry {
        TransportGeometry::plan(nq, env_q, env_d, 1024 * 1024, 256, PAGE, cap, mb, ct)
    }

    /// Full-input planner for the sysctl-geometry tests: desired
    /// max_write + `fs.fuse.max_pages_limit` explicit.
    fn plan_mw(
        nq: usize,
        desired_max_write: usize,
        max_pages_limit: usize,
        cap: u64,
    ) -> TransportGeometry {
        TransportGeometry::plan(
            nq,
            None,
            None,
            desired_max_write,
            max_pages_limit,
            PAGE,
            cap,
            None,
            None,
        )
    }

    /// The kernel's exact REGISTER acceptance bound, mirrored from
    /// fs/fuse/dev_uring.c (`fuse_uring_create` + the `Invalid req
    /// payload len` refusal at ent REGISTER):
    ///
    /// ```c
    /// fc->max_pages   = min(fc->max_pages_limit, max(arg->max_pages, 1));
    /// fc->max_write   = max(4096, arg->max_write);
    /// ring->max_payload_sz = max(FUSE_MIN_READ_BUFFER, fc->max_write,
    ///                            fc->max_pages * PAGE_SIZE);
    /// // REGISTER refuses iov[1].iov_len < ring->max_payload_sz
    /// ```
    ///
    /// Every planned geometry must satisfy
    /// `payload_sz >= kernel_ring_max_payload_sz(limit, geom)` — this
    /// mirror IS the geometry-bug contract (2026-08-04 campaign; the
    /// pre-fix planner hardcoded 256 pages while the INIT reply
    /// advertised `max_pages = u16::MAX`, so any raised sysctl made the
    /// kernel bound exceed the ents and every REGISTER refused ⇒ mount
    /// failed).
    fn kernel_ring_max_payload_sz(max_pages_limit: usize, g: &TransportGeometry) -> usize {
        const FUSE_MIN_READ_BUFFER: usize = 8192;
        let fc_max_pages = max_pages_limit.min((g.max_pages as usize).max(1));
        FUSE_MIN_READ_BUFFER
            .max(g.max_write.max(4096))
            .max(fc_max_pages * PAGE)
    }

    // -----------------------------------------------------------------
    // Geometry-bug contracts (fuse3 transport geometry + zc adoption
    // campaign, 2026-08-04): the planner derives ent payload size from
    // the NEGOTIATED max_write/max_pages — no hardcoded 256-page
    // constant — and the INIT reply advertises the max_pages the plan
    // stands on, so the kernel-side REGISTER bound can never exceed the
    // registered ents.
    // -----------------------------------------------------------------

    /// THE bug shape: default daemon (desired max_write 1 MiB) on a box
    /// whose operator raised `fs.fuse.max_pages_limit` past 256. The
    /// pre-fix planner kept 1 MiB ents while advertising
    /// `max_pages = 65535`, so the kernel bound became `sysctl × 4 KiB`
    /// and every REGISTER refused. The plan must keep the kernel bound
    /// and the ents EQUAL by advertising max_pages consistent with the
    /// negotiated max_write.
    #[test]
    fn test_plan_raised_sysctl_register_bound_holds() {
        for limit in [512usize, 1024, 4096, 65535] {
            let g = plan_mw(32, MIB as usize, limit, 2 * GIB);
            assert_eq!(
                g.payload_sz,
                kernel_ring_max_payload_sz(limit, &g),
                "limit={limit}: ent payload must exactly satisfy the kernel \
                 REGISTER bound (refused REGISTER = failed mount)"
            );
            assert_eq!(g.max_write, MIB as usize, "desired 1 MiB stands");
            assert_eq!(
                g.max_pages, 256,
                "advertised max_pages must describe the negotiated max_write, \
                 never a blanket u16::MAX"
            );
            assert_eq!(g.payload_sz, MIB as usize, "1 MiB ents stay 1 MiB");
        }
    }

    /// The today-shape pin: at the kernel-default sysctl (256) and the
    /// shipped 1 MiB desired max_write, the resolved geometry keeps the
    /// pre-campaign ents/depth; `max_background` = the delivered ring
    /// capacity (32×32 = 1024 — the 2026-08-04 derivation sweep retired
    /// the fixed 256 ceiling; `-o max_background=256` is the A0 lever).
    #[test]
    fn test_plan_default_sysctl_shape_is_byte_identical() {
        let g = plan_mw(32, MIB as usize, 256, 2 * GIB);
        assert_eq!(g.nqueues, 32);
        assert_eq!(g.depth, 32);
        assert_eq!(g.payload_sz, MIB as usize);
        assert_eq!(g.max_write, MIB as usize);
        assert_eq!(g.max_pages, 256);
        assert_eq!(g.max_background, 1024, "ring capacity 32×32");
        assert_eq!(g.congestion_threshold, 768);
        assert_eq!(g.total_payload_bytes(), GIB);
    }

    /// 4 MiB negotiation (candidate 1): a 4 MiB desired max_write rides
    /// verbatim when the sysctl admits it (1024+), and degrades to the
    /// sysctl ceiling gracefully when it does not (fleet kernels at the
    /// 256 default keep today's 1 MiB shape — never a refused mount).
    #[test]
    fn test_plan_4mib_max_write_sysctl_gated() {
        // sqz-host posture: fs.fuse.max_pages_limit=1024.
        let g = plan_mw(32, 4 * MIB as usize, 1024, 2 * GIB);
        assert_eq!(g.max_write, 4 * MIB as usize);
        assert_eq!(g.max_pages, 1024);
        assert_eq!(g.payload_sz, 4 * MIB as usize);
        assert_eq!(g.payload_sz, kernel_ring_max_payload_sz(1024, &g));
        assert_eq!(
            g.depth, 16,
            "the L1 ladder re-derived for 4 MiB ents: 2 GiB / (32 × 4 MiB) = 16"
        );
        assert_eq!(g.total_payload_bytes(), 2 * GIB);
        assert_eq!(
            g.max_background, 512,
            "ring capacity 32×16 (no fixed ceiling)"
        );

        // Fleet kernel at the default sysctl: the desire degrades to the
        // 256-page ceiling — today's shape, mount succeeds.
        let g = plan_mw(32, 4 * MIB as usize, 256, 2 * GIB);
        assert_eq!(g.max_write, MIB as usize, "sysctl 256 gates 4 MiB to 1 MiB");
        assert_eq!(g.max_pages, 256);
        assert_eq!(g.payload_sz, MIB as usize);
        assert_eq!(g.depth, 32, "1 MiB ents keep the measured depth-32 class");
        assert_eq!(g.payload_sz, kernel_ring_max_payload_sz(256, &g));

        // A sysctl LOWERED below the default gates the same way (the
        // writable range is 1..65535) — the bound law is symmetric.
        let g = plan_mw(32, MIB as usize, 64, 2 * GIB);
        assert_eq!(g.max_write, 64 * PAGE);
        assert_eq!(g.max_pages, 64);
        assert_eq!(g.payload_sz, kernel_ring_max_payload_sz(64, &g));
    }

    /// The variable-ent budget ladder (the L1 depth-degradation policy
    /// re-derived — design amendment §degradation table): depth degrades
    /// 32→4 first; only when the floor-4 arena still exceeds the cap
    /// does the payload leg engage, degrading max_write toward the
    /// 1 MiB base (yesterday's shipped ent size) — never below it, so no
    /// box regresses below the pre-campaign posture.
    #[test]
    fn test_plan_budget_ladder_degrades_payload_before_floor_violation() {
        // 4 MiB target, cap fits floor-4 at 4 MiB: depth leg only.
        // 32q × 4 × 4 MiB = 512 MiB ≤ 819 MiB ⇒ depth = 819M/(32×4M) = 6.
        let g = plan_mw(32, 4 * MIB as usize, 1024, 819 * MIB);
        assert_eq!(g.max_write, 4 * MIB as usize, "payload leg must not engage");
        assert_eq!(g.depth, 6);

        // Cap below the floor-4 arena at 4 MiB (32q × 4 × 4 MiB =
        // 512 MiB > 256 MiB): the payload leg degrades the ent size to
        // fit floor 4 — 256 MiB / (32 × 4) = 2 MiB.
        let g = plan_mw(32, 4 * MIB as usize, 1024, 256 * MIB);
        assert_eq!(g.depth, 4, "floor holds");
        assert_eq!(g.max_write, 2 * MIB as usize, "ents degrade to fit the cap");
        assert_eq!(g.max_pages, 512);
        assert_eq!(g.payload_sz, kernel_ring_max_payload_sz(1024, &g));

        // Cap below even the floor-4 × 1 MiB base arena: payload pins at
        // the base and depth pins at the floor — exactly the pre-L1
        // posture, the standing never-regress law.
        let g = plan_mw(32, 4 * MIB as usize, 1024, 64 * MIB);
        assert_eq!(g.depth, 4);
        assert_eq!(g.max_write, MIB as usize, "base = yesterday's 1 MiB ents");
        assert_eq!(g.max_pages, 256);

        // Cap 0 (unknown RAM): base + floor, never below.
        let g = plan_mw(32, 4 * MIB as usize, 1024, 0);
        assert_eq!(g.depth, 4);
        assert_eq!(g.max_write, MIB as usize);
    }

    /// Small desired max_write: the planner sizes ents to the actual
    /// negotiated geometry — no 256-page inflation (the pre-fix planner
    /// paid 1 MiB ents for an 8 KiB max_write because the kernel bound
    /// it mirrored was pinned at the sysctl default it hardcoded).
    #[test]
    fn test_plan_small_max_write_stops_inflating_to_256_pages() {
        let g = plan_mw(8, 4096, 256, 32 * MIB);
        assert_eq!(g.max_write, 4096, "kernel max_write floor is 4096");
        assert_eq!(g.max_pages, 1);
        assert_eq!(
            g.payload_sz, 8192,
            "FUSE_MIN_READ_BUFFER floors the ent, not 256 pages"
        );
        assert_eq!(g.payload_sz, kernel_ring_max_payload_sz(256, &g));
        assert_eq!(g.depth, 32, "small ents leave the whole depth budget open");
    }

    /// Env depth override semantics are unchanged by the variable-ent
    /// ladder: explicit operator intent wins verbatim and bypasses the
    /// budget entirely (payload stays at the sysctl-gated target).
    #[test]
    fn test_plan_env_depth_bypasses_payload_ladder() {
        let g = TransportGeometry::plan(
            32,
            None,
            Some(8),
            4 * MIB as usize,
            1024,
            PAGE,
            64 * MIB, // would force base + floor without the override
            None,
            None,
        );
        assert_eq!(g.depth, 8, "env depth wins verbatim");
        assert_eq!(g.max_write, 4 * MIB as usize, "payload target stands");
        assert_eq!(g.payload_sz, kernel_ring_max_payload_sz(1024, &g));
    }

    /// Ample budget ⇒ the measured 316k-class depth with the
    /// ring-capacity `max_background` (derivation sweep 2026-08-04: the
    /// fixed 256 ceiling retired — the measured row scales with
    /// max_background once depth is open; `-o max_background=256` is
    /// the A0 lever).
    #[test]
    fn test_plan_default_ample_budget_is_measured_class() {
        let g = plan(32, None, None, 2 * GIB, None, None);
        assert_eq!(g.nqueues, 32);
        assert_eq!(g.depth, 32, "desired depth under an ample cap");
        assert_eq!(g.payload_sz, 1024 * 1024);
        assert_eq!(g.total_payload_bytes(), GIB);
        assert_eq!(g.max_background, 1024, "clamp(32×32, 64, u16::MAX)");
        assert_eq!(g.congestion_threshold, 768, "¾ of max_background");
    }

    /// The budget degrades depth exactly (integer division), floor 4 —
    /// the pre-L1 shipped posture even when the cap is smaller than the
    /// floor's arena.
    #[test]
    fn test_plan_budget_degrades_depth_gracefully() {
        // 819 MiB cap on 32 queues × 1 MiB ⇒ depth 25 (the 8G-cage row).
        let g = plan(32, None, None, 819 * MIB, None, None);
        assert_eq!(g.depth, 25);
        assert_eq!(g.max_background, 800, "ring capacity 32×25");
        // 128 MiB cap ⇒ exactly the floor.
        let g = plan(32, None, None, 128 * MIB, None, None);
        assert_eq!(g.depth, 4, "floor = pre-L1 default");
        assert_eq!(g.max_background, 128, "32×4 within [64, u16::MAX]");
        assert_eq!(g.congestion_threshold, 96);
        // Cap 0 (unknown RAM) ⇒ still the floor, never below.
        let g = plan(32, None, None, 0, None, None);
        assert_eq!(g.depth, 4);
        // Few-CPU box, small cap: 4 queues, 179 MiB ⇒ desired 32 fits.
        let g = plan(4, None, None, 179 * MIB, None, None);
        assert_eq!(g.depth, 32);
        assert_eq!(g.max_background, 128, "4×32 = 128");
        // Huge-CPU box on a 2 GiB budget: 256 queues ⇒ depth 8 (the
        // depth ladder is now driven by the budget fraction alone — a
        // bigger budget restores depth 32 where the retired 2 GiB
        // ceiling used to pin it).
        let g = plan(256, None, None, 2 * GIB, None, None);
        assert_eq!(g.depth, 8);
        assert_eq!(g.total_payload_bytes(), 2 * GIB);
        assert_eq!(g.max_background, 2048, "ring capacity 256×8");
        // The same huge-CPU box with a big-RAM budget fraction (the
        // shape the fixed ceiling used to bind): depth restores to the
        // measured 32 and the arena is the structural demand cap.
        let g = plan(256, None, None, 9 * GIB + 512 * MIB, None, None);
        assert_eq!(
            g.depth, 32,
            "no fixed ceiling: 9.5 GiB budget opens depth 32"
        );
        assert_eq!(
            g.total_payload_bytes(),
            8 * GIB,
            "the demand cap: 256×32×1 MiB"
        );
    }

    /// Env depth wins verbatim over the cap (explicit operator intent),
    /// with the existing 1..32 clamp semantics unchanged.
    #[test]
    fn test_plan_env_depth_override_wins() {
        let g = plan(32, None, Some(6), 0, None, None);
        assert_eq!(g.depth, 6, "env bypasses the budget cap");
        assert_eq!(g.max_background, 192, "32×6 = 192 within [64, u16::MAX]");
        assert_eq!(plan(32, None, Some(64), 2 * GIB, None, None).depth, 32);
        assert_eq!(plan(32, None, Some(0), 2 * GIB, None, None).depth, 1);
    }

    /// Queue-count semantics unchanged: kernel possible CPUs by default,
    /// env override clamped 1..512; max_background floor holds on tiny
    /// geometries.
    #[test]
    fn test_plan_queues_and_background_floor() {
        let g = plan(32, Some(1), Some(4), 2 * GIB, None, None);
        assert_eq!(g.nqueues, 1);
        assert_eq!(g.max_background, 64, "1×4 = 4 floors to 64");
        assert_eq!(g.congestion_threshold, 48);
        assert_eq!(plan(32, Some(4096), None, 2 * GIB, None, None).nqueues, 512);
        assert_eq!(plan(32, Some(0), None, 2 * GIB, None, None).nqueues, 1);
    }

    /// Explicit INIT-limit overrides win; 0 means "not set" (kernel
    /// semantics) and falls through to policy.
    #[test]
    fn test_plan_background_overrides() {
        let g = plan(32, None, None, 2 * GIB, Some(96), Some(80));
        assert_eq!(g.max_background, 96);
        assert_eq!(g.congestion_threshold, 80);
        // Override mb only: ct derives from the OVERRIDDEN mb.
        let g = plan(32, None, None, 2 * GIB, Some(100), None);
        assert_eq!(g.max_background, 100);
        assert_eq!(g.congestion_threshold, 75);
        // Zero overrides are ignored (policy default = ring capacity).
        let g = plan(32, None, None, 2 * GIB, Some(0), Some(0));
        assert_eq!(g.max_background, 1024);
        assert_eq!(g.congestion_threshold, 768);
    }

    /// Arena buffers: one stable, 4096-aligned, zeroed allocation per ring
    /// ent; out-of-range indexes refused; the dup'ed wake fd is distinct
    /// from (but signals) the original eventfd.
    #[test]
    fn test_payload_arena_buffers() {
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(efd >= 0);
        let efd_owned = unsafe { OwnedFd::from_raw_fd(efd) };
        let arena = PayloadArena::new(
            4,
            8192,
            efd_owned.as_raw_fd(),
            Arc::new(WakeCoalescer::new()),
            None,
        )
        .unwrap();

        let mut seen = std::collections::HashSet::new();
        for idx in 0..4 {
            let p = arena.buf(idx).expect("in-range ent");
            assert_eq!(p as usize % 4096, 0, "payload buffers must be page-aligned");
            assert!(seen.insert(p as usize), "ent buffers must not alias");
            // Born zeroed (fresh arena; the kernel owns content afterwards).
            let s = unsafe { std::slice::from_raw_parts(p, 8192) };
            assert!(s.iter().all(|&b| b == 0));
        }
        assert!(arena.buf(4).is_none(), "out-of-range ent must be refused");

        // The arena wake fd is a dup: writing it must signal the original.
        let one: u64 = 1;
        let w = unsafe { libc::write(arena.wake.as_raw_fd(), &one as *const u64 as *const _, 8) };
        assert_eq!(w, 8);
        let mut buf = [0u8; 8];
        let r = unsafe { libc::read(efd_owned.as_raw_fd(), buf.as_mut_ptr().cast(), 8) };
        assert_eq!(r, 8, "dup'ed wake fd must signal the queue eventfd");
    }

    /// D3.a batch accounting: exact buckets 1–8, then ≤16 / ≤32 / >32,
    /// snapshot order matching [`COMMIT_BATCH_LABELS`].
    #[test]
    fn test_commit_batch_histogram_buckets() {
        let h = CommitBatchHistogram::new();
        h.record(1);
        h.record(1);
        h.record(4);
        h.record(8);
        h.record(9);
        h.record(16);
        h.record(17);
        h.record(32);
        h.record(33);
        h.record(4096);
        let snap = h.snapshot();
        assert_eq!(COMMIT_BATCH_LABELS.len(), snap.len());
        let idx = |l: &str| {
            COMMIT_BATCH_LABELS
                .iter()
                .position(|&x| x == l)
                .expect("label")
        };
        assert_eq!(snap[idx("1")], 2, "two singleton batches");
        assert_eq!(snap[idx("4")], 1);
        assert_eq!(snap[idx("8")], 1);
        assert_eq!(snap[idx("<=16")], 2, "9 and 16 share the ≤16 bucket");
        assert_eq!(snap[idx("<=32")], 2, "17 and 32 share the ≤32 bucket");
        assert_eq!(snap[idx(">32")], 2, "33 and 4096 overflow to >32");
        assert_eq!(snap.iter().sum::<u64>(), 10, "every record lands once");
    }

    /// Build a worker-shaped SQE128 ring with an eventfd registered at
    /// Fixed(0) — uring cmds against it complete as error CQEs, which is
    /// all the submit-accounting tests need (submission itself succeeds).
    fn batch_test_ring(sq_entries: u32) -> (Ring, OwnedFd) {
        let ring: Ring = IoUring::<squeue::Entry128, cqueue::Entry>::builder()
            .setup_cqsize(64)
            .build(sq_entries)
            .expect("SQE128 ring (need IORING_SETUP_SQE128)");
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(efd >= 0);
        let owned = unsafe { OwnedFd::from_raw_fd(efd) };
        ring.submitter()
            .register_files(&[owned.as_raw_fd()])
            .expect("register eventfd as Fixed(0)");
        (ring, owned)
    }

    /// The S2 contract (design-metadata-throughput §5.3 D3.a): a drain pass
    /// PUSHES its COMMIT_AND_FETCH SQEs — no per-message submit — and ONE
    /// flush syscall carries the whole batch, recording its size in the
    /// `transport_commit_batch` histogram.
    #[test]
    fn test_commit_pushes_defer_to_one_flush() {
        let _guard = sqpoll_test_guard();
        let (mut ring, _efd) = batch_test_ring(16);
        let (fl0, cm0, snap0) = over_uring_commit_batch_stats();
        let mut batch = SubmitBatch::default();
        for i in 0..4u64 {
            push_cmd_batched(
                &mut ring,
                &mut batch,
                FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
                0,
                i + 1,
                None,
                i,
                0,
                0,
            )
            .expect("batched push");
        }
        assert_eq!(
            ring.submission().len(),
            4,
            "batched pushes must stay queued in the SQ — a drained SQ here \
             means the worker is still burning one io_uring_enter per commit \
             message (the 31-syscalls/create shape)"
        );
        let submitted = flush_submit(&mut ring, &mut batch).expect("flush");
        assert_eq!(submitted, 4, "one flush submits the whole batch");
        assert_eq!(ring.submission().len(), 0, "flush drained the SQ");
        let (fl1, cm1, snap1) = over_uring_commit_batch_stats();
        assert_eq!(fl1 - fl0, 1, "one commit-carrying flush recorded");
        assert_eq!(cm1 - cm0, 4, "four COMMIT_AND_FETCH SQEs recorded");
        let b4 = COMMIT_BATCH_LABELS.iter().position(|&l| l == "4").unwrap();
        assert_eq!(
            snap1[b4] - snap0[b4],
            1,
            "the flush lands one sample in the exact '4' bucket"
        );

        // A flush with no pending commits records nothing (pure waits and
        // poll re-arms must not dilute the batch histogram).
        let submitted = flush_submit(&mut ring, &mut batch).expect("empty flush");
        assert_eq!(submitted, 0);
        let (fl2, cm2, _) = over_uring_commit_batch_stats();
        assert_eq!(fl2, fl1, "commit-less flush not recorded");
        assert_eq!(cm2, cm1);
    }

    /// SQ-full during a batched push flushes what's queued and continues
    /// (§5.3 D3.a SQ-full rule: submit-and-continue) — every push succeeds,
    /// every commit is counted exactly once, and the intermediate flushes
    /// are recorded as their own (partial) batches.
    #[test]
    fn test_batched_push_sq_full_submits_and_continues() {
        let _guard = sqpoll_test_guard();
        let (mut ring, _efd) = batch_test_ring(4); // deliberately tiny SQ
        let (fl0, cm0, _) = over_uring_commit_batch_stats();
        let mut batch = SubmitBatch::default();
        for i in 0..10u64 {
            push_cmd_batched(
                &mut ring,
                &mut batch,
                FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
                0,
                i + 1,
                None,
                i,
                0,
                0,
            )
            .expect("SQ-full must flush-and-continue, never error");
        }
        let tail = flush_submit(&mut ring, &mut batch).expect("final flush");
        assert!(tail >= 1, "the tail flush carries the remainder");
        assert_eq!(ring.submission().len(), 0);
        let (fl1, cm1, _) = over_uring_commit_batch_stats();
        assert_eq!(
            cm1 - cm0,
            10,
            "all 10 commits submitted and counted exactly once across \
             intermediate SQ-full flushes + the tail flush"
        );
        assert!(
            fl1 - fl0 >= 3,
            "10 pushes through a 4-deep SQ need ≥ 3 flushes (got {})",
            fl1 - fl0
        );

        // The kernel really consumed them: 10 CQEs arrive (as error
        // completions — an eventfd has no ->uring_cmd — which is exactly
        // what the accounting must be indifferent to).
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = 0usize;
        while seen < 10 && Instant::now() < deadline {
            let mut cq = ring.completion();
            cq.sync();
            seen += cq.count();
        }
        assert_eq!(seen, 10, "every batched SQE reached the kernel");
    }

    /// A dropped payload lease releases its ref, records the outstanding
    /// gauge, and fires the queue eventfd when (and only when) a commit is
    /// parked.
    #[test]
    fn test_lease_drop_wakes_parked_worker() {
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(efd >= 0);
        let efd_owned = unsafe { OwnedFd::from_raw_fd(efd) };
        let arena = PayloadArena::new(
            1,
            8192,
            efd_owned.as_raw_fd(),
            Arc::new(WakeCoalescer::new()),
            None,
        )
        .unwrap();
        let state = Arc::new(EntLeaseState::new());

        assert_eq!(state.acquire(), 0);
        TRANSPORT_LEASES_OUTSTANDING.fetch_add(1, Ordering::Relaxed);
        let lease = EntPayloadLease {
            arena: Arc::clone(&arena),
            state: Arc::clone(&state),
            ptr: arena.buf(0).unwrap() as *const u8,
            len: 16,
            born: Instant::now(),
        };
        let bytes = Bytes::from_owner(lease);
        assert_eq!(bytes.len(), 16);
        let clone = bytes.clone();
        drop(bytes);
        // A clone keeps the single owner (and its ref) alive.
        assert!(state.leased(), "clone dropped the owner early");

        // Reply while leased: gate parks.
        assert_eq!(state.try_commit(), CommitGate::Parked);
        drop(clone);
        assert!(!state.leased());
        // The drop must have fired the wake (parked was set).
        let mut buf = [0u8; 8];
        let r = unsafe { libc::read(efd_owned.as_raw_fd(), buf.as_mut_ptr().cast(), 8) };
        assert_eq!(
            r, 8,
            "lease drop with a parked commit must fire the eventfd"
        );
        assert!(state.try_unpark(), "commit releasable after the drop");
    }

    /// MEM-1: dest-DMA leases park the commit gate exactly like payload
    /// leases, and multi-SQE reads compose — the gate stays parked until
    /// the LAST token drops, and only that drop fires the wake.
    #[test]
    fn test_dest_dma_lease_multi_token_parks_until_last_drop() {
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(efd >= 0);
        let efd_owned = unsafe { OwnedFd::from_raw_fd(efd) };
        let arena = PayloadArena::new(
            2,
            8192,
            efd_owned.as_raw_fd(),
            Arc::new(WakeCoalescer::new()),
            None,
        )
        .unwrap();
        let state = Arc::new(EntLeaseState::new());

        // Two in-flight SQEs against one ent (kernel-split multi-block
        // read): one token each.
        state.acquire_dest();
        let t1 = DestDmaLease {
            arena: Arc::clone(&arena),
            state: Arc::clone(&state),
        };
        state.acquire_dest();
        let t2 = DestDmaLease {
            arena: Arc::clone(&arena),
            state: Arc::clone(&state),
        };

        // Reply arrives while both SQEs fly: gate parks (no re-arm).
        assert_eq!(state.try_commit(), CommitGate::Parked);

        // First CQE: still one SQE in flight — no unpark, no wake.
        drop(t1);
        assert!(state.leased(), "one token must keep the ent leased");
        assert!(!state.try_unpark(), "must not unpark with a live token");
        let mut buf = [0u8; 8];
        let r = unsafe { libc::read(efd_owned.as_raw_fd(), buf.as_mut_ptr().cast(), 8) };
        assert!(
            r < 0,
            "non-final token drop must not fire the wake (read must EAGAIN)"
        );

        // Last CQE: token drop releases the gate and fires the wake.
        drop(t2);
        assert!(!state.leased());
        let r = unsafe { libc::read(efd_owned.as_raw_fd(), buf.as_mut_ptr().cast(), 8) };
        assert_eq!(
            r, 8,
            "final token drop with a parked commit must fire the eventfd"
        );
        assert!(state.try_unpark(), "commit releasable after the last drop");
    }

    /// MEM-1 window math: in-buffer windows resolve to their buffer index;
    /// straddling windows are refused (never mis-leased).
    #[test]
    fn test_dest_window_index_resolution_and_straddle_refusal() {
        let stride = 4096usize;
        let base = 1 << 20;
        assert_eq!(dest_window_index(base, stride, base, 4096), Some(0));
        assert_eq!(dest_window_index(base, stride, base + 4096, 1), Some(1));
        assert_eq!(
            dest_window_index(base, stride, base + 2 * 4096 + 512, 512),
            Some(2)
        );
        // Last byte of buffer 0 — still buffer 0.
        assert_eq!(dest_window_index(base, stride, base + 4095, 1), Some(0));
        // Straddle: starts in buffer 0, ends in buffer 1 — refused.
        assert_eq!(dest_window_index(base, stride, base + 4095, 2), None);
        assert_eq!(dest_window_index(base, stride, base, 4097), None);
    }

    // ---- §5.3 D3.b (S3): SQPOLL on the over-uring queue rings ----

    /// Tests that observe process-global transport state serialize here:
    /// `cargo test` runs tests on parallel threads by default, and both the
    /// `iou-sqp-*` poller population (D3.b) and the `transport_commit_batch`
    /// counters (D3.a) are process-wide — concurrent tests cross-contaminate
    /// each other's deltas. (The repo gate's `--test-threads=1` never races;
    /// this keeps a bare `cargo test` honest too.)
    static PROCESS_GLOBAL_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn sqpoll_test_guard() -> std::sync::MutexGuard<'static, ()> {
        PROCESS_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Count this process's kernel SQPOLL poller threads (`iou-sqp-<tgid>`
    /// comm) — the one-poller policy's observable.
    fn count_sqpoll_pollers() -> usize {
        std::fs::read_dir("/proc/self/task")
            .map(|tasks| {
                tasks
                    .flatten()
                    .filter(|t| {
                        std::fs::read_to_string(t.path().join("comm"))
                            .map(|c| c.trim_start().starts_with("iou-sqp"))
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// Ring-ctx teardown is asynchronous in the kernel, so a poller from a
    /// just-dropped test ring can linger briefly. Bounded-poll until the
    /// count is stable (0 short-circuits) and return it as the baseline.
    fn settle_pollers() -> usize {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut last = count_sqpoll_pollers();
        let mut stable_since = Instant::now();
        while Instant::now() < deadline && last != 0 {
            std::thread::sleep(Duration::from_millis(10));
            let now = count_sqpoll_pollers();
            if now != last {
                last = now;
                stable_since = Instant::now();
            } else if stable_since.elapsed() > Duration::from_millis(200) {
                break;
            }
        }
        last
    }

    /// Wait (bounded) for the poller population to REACH `expected`.
    ///
    /// `io_uring_setup` returning does not mean the kernel's `iou-sqp`
    /// task is already visible in `/proc/self/task`: thread creation and
    /// its `comm` publication race the returning syscall. An instant
    /// assert therefore flaked (~2/15 quiet-box runs: "left 0 right 1")
    /// — a scan artifact, never a poller-count bug. Waiting for the
    /// count is the honest observation; the deadline keeps a REAL
    /// missing poller a failure rather than a hang.
    fn await_pollers(expected: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let now = count_sqpoll_pollers();
            if now == expected || Instant::now() >= deadline {
                return now;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Assert the poller population STAYS at `expected` for a settle
    /// window. The attach contract is a must-NOT-grow property, so it is
    /// pinned by watching for a while rather than by one instant sample
    /// (which could pass simply by looking before a second poller
    /// appeared).
    fn assert_pollers_stay(expected: usize, window: Duration, ctx: &str) {
        let deadline = Instant::now() + window;
        loop {
            let now = count_sqpoll_pollers();
            assert_eq!(now, expected, "{ctx}");
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn sqpoll_group(idle_ms: u32, cpu: Option<u32>) -> SqpollGroup {
        SqpollGroup {
            cfg: SqpollConfig { idle_ms, cpu },
            leader: OnceLock::new(),
        }
    }

    /// Knob parsing carries the classical rings' semantics verbatim
    /// (`tokio.rs`): idle must parse > 0 to enable; a bad `_CPU` degrades
    /// to unpinned, never to off; `_CPU` alone never enables SQPOLL.
    #[test]
    fn test_sqpoll_config_parse_matches_classical_knob_semantics() {
        assert_eq!(SqpollConfig::parse(None, None), None, "unset ⇒ off");
        assert_eq!(
            SqpollConfig::parse(Some("0"), Some("2")),
            None,
            "idle 0 ⇒ off (the tokio.rs `> 0` filter)"
        );
        assert_eq!(
            SqpollConfig::parse(Some("nope"), None),
            None,
            "unparsable idle ⇒ off"
        );
        assert_eq!(
            SqpollConfig::parse(None, Some("3")),
            None,
            "_CPU alone never enables SQPOLL"
        );
        assert_eq!(
            SqpollConfig::parse(Some("50"), None),
            Some(SqpollConfig {
                idle_ms: 50,
                cpu: None
            })
        );
        assert_eq!(
            SqpollConfig::parse(Some(" 50 "), Some("3")),
            Some(SqpollConfig {
                idle_ms: 50,
                cpu: Some(3)
            }),
            "whitespace-tolerant, pin honored"
        );
        assert_eq!(
            SqpollConfig::parse(Some("50"), Some("0")),
            Some(SqpollConfig {
                idle_ms: 50,
                cpu: Some(0)
            }),
            "CPU 0 is a real CPU (the CLI's '0 disables' runs before the env reaches fuse3)"
        );
        assert_eq!(
            SqpollConfig::parse(Some("50"), Some("x")),
            Some(SqpollConfig {
                idle_ms: 50,
                cpu: None
            }),
            "bad _CPU ⇒ unpinned, idle still honored"
        );
    }

    /// PERF-5 fallback contract on the injectable setup seam
    /// (`build_plain_queue_ring_with`): a probed-modern kernel attempts
    /// SINGLE_ISSUER+DEFER_TASKRUN first, a Modern refusal degrades to
    /// the Plain build (never fails the mount), and a probe-miss kernel
    /// builds Plain directly and SILENTLY — today's setup with zero
    /// extra build attempts (portable-by-default: the probe is the only
    /// capability source, never a version check).
    mod queue_ring_posture {
        use super::*;

        fn real_plain_ring(sq: u32) -> io::Result<Ring> {
            IoUring::<squeue::Entry128, cqueue::Entry>::builder()
                .setup_cqsize(sq * 2)
                .build(sq)
                .map_err(io::Error::other)
        }

        /// Probe hit ⇒ the Modern posture is attempted FIRST and its
        /// success is the delivered ring (exactly one build attempt).
        #[test]
        fn probed_modern_attempts_modern_first() {
            let mut seq = Vec::new();
            let ring = build_plain_queue_ring_with(16, true, |sq, posture| {
                seq.push(posture);
                real_plain_ring(sq)
            })
            .expect("modern build accepted");
            drop(ring);
            assert_eq!(
                seq,
                vec![QueueRingPosture::Modern],
                "a probed-modern kernel must get the Modern setup flags \
                 on the first (and only) build attempt"
            );
        }

        /// Probe hit but the Modern build refuses (e.g. a seccomp/lockdown
        /// surprise past the probe) ⇒ warn-and-degrade to Plain; the
        /// mount still gets its ring.
        #[test]
        fn modern_refusal_lands_on_plain() {
            let mut seq = Vec::new();
            let ring = build_plain_queue_ring_with(16, true, |sq, posture| {
                seq.push(posture);
                match posture {
                    QueueRingPosture::Modern => Err(io::Error::from_raw_os_error(libc::EINVAL)),
                    QueueRingPosture::Plain => real_plain_ring(sq),
                }
            })
            .expect("the Plain fallback must deliver the ring");
            drop(ring);
            assert_eq!(
                seq,
                vec![QueueRingPosture::Modern, QueueRingPosture::Plain],
                "a Modern refusal must land on the Plain build — an \
                 accelerator never fails the mount"
            );
        }

        /// Probe miss (pre-6.1 kernels) ⇒ Plain directly, silently:
        /// today's setup, byte-identical, no Modern attempt ever.
        #[test]
        fn probe_miss_builds_plain_silently() {
            let mut seq = Vec::new();
            let ring = build_plain_queue_ring_with(16, false, |sq, posture| {
                seq.push(posture);
                real_plain_ring(sq)
            })
            .expect("plain build");
            drop(ring);
            assert_eq!(
                seq,
                vec![QueueRingPosture::Plain],
                "a probe-miss kernel must degrade to today's setup \
                 SILENTLY — exactly one Plain build, no Modern attempt"
            );
        }

        /// Plain-arm errors still propagate loudly (the pre-PERF-5
        /// failure surface is unchanged: no ring ⇒ the worker ⇒ the
        /// mount fails, never a silent no-transport session).
        #[test]
        fn plain_refusal_stays_loud() {
            match build_plain_queue_ring_with(16, false, |_sq, _posture| {
                Err(io::Error::from_raw_os_error(libc::ENOMEM))
            }) {
                Err(err) => assert_eq!(err.raw_os_error(), Some(libc::ENOMEM)),
                Ok(_) => panic!("a Plain refusal must propagate"),
            }
        }

        /// LIVE engagement: on a kernel where the probe finds the flags,
        /// the shipped `build_plain_queue_ring` must deliver a ring with
        /// SINGLE_ISSUER actually set (DEFER_TASKRUN rides the same
        /// build call — the seam's Modern posture is the pair by
        /// construction); on a probe-miss kernel it must deliver today's
        /// plain ring. Either way the branch it takes IS the capability
        /// lattice (the kmbuf-probe convention).
        #[test]
        fn shipped_builder_engages_probed_flags() {
            let ring = build_plain_queue_ring(16).expect("queue ring builds");
            assert_eq!(
                ring.params().is_setup_single_issuer(),
                modern_queue_ring_flags_probed(),
                "the shipped builder must engage SINGLE_ISSUER exactly \
                 when the runtime probe found the flags"
            );
        }
    }

    /// Knob unset ⇒ every queue ring builds exactly as today: no SQPOLL
    /// flag, no poller thread, no leader coordination — the D3.b
    /// "default off = byte-identical" contract.
    #[test]
    fn test_sqpoll_default_off_builds_plain_rings() {
        let _guard = sqpoll_test_guard();
        let active = AtomicBool::new(true);
        let before = settle_pollers();
        for qid in [0u16, 1, 7] {
            let ring = build_queue_ring(16, qid, None, &active).expect("plain build");
            assert!(
                !ring.params().is_setup_sqpoll(),
                "qid={qid}: knob unset must not set IORING_SETUP_SQPOLL"
            );
        }
        assert_pollers_stay(
            before,
            Duration::from_millis(200),
            "knob unset must not spawn poller threads",
        );
    }

    /// The D3.b multi-queue policy: with the knobs set, qid 0 is the
    /// LEADER — it creates the single kernel poller and publishes its ring
    /// fd — and every other queue ATTACHES to that poller via
    /// `IORING_SETUP_ATTACH_WQ`. N queue rings share exactly ONE
    /// `iou-sqp-*` thread (a per-queue poller would burn up to 32 cores),
    /// and `register_files` (the worker's Fixed(0)/Fixed(1) prerequisite)
    /// keeps working on leader and attached rings alike.
    #[test]
    fn test_sqpoll_one_shared_poller_across_queue_rings() {
        let _guard = sqpoll_test_guard();
        let group = sqpoll_group(50, None);
        let active = AtomicBool::new(true);
        let before = settle_pollers();

        let leader = build_queue_ring(16, 0, Some(&group), &active).expect("leader build");
        if !leader.params().is_setup_sqpoll() {
            // Kernel declined SQPOLL (EPERM on locked-down boxes). The
            // decline contract still holds: the leader must have published
            // None so followers fall back plain instead of waiting.
            eprintln!("kernel declined SQPOLL; skipping one-poller assertions");
            assert_eq!(
                group.leader.get(),
                Some(&None),
                "declined leader must publish None for the followers"
            );
            return;
        }
        assert_eq!(
            await_pollers(before + 1),
            before + 1,
            "leader creates exactly one poller"
        );
        assert_eq!(
            group.leader.get().copied(),
            Some(Some(leader.as_raw_fd())),
            "leader must publish its ring fd for the followers to attach to"
        );

        let f1 = build_queue_ring(16, 1, Some(&group), &active).expect("follower 1");
        let f2 = build_queue_ring(16, 2, Some(&group), &active).expect("follower 2");
        assert!(
            f1.params().is_setup_sqpoll() && f2.params().is_setup_sqpoll(),
            "followers must ride SQPOLL (attached), not silently build plain"
        );
        assert_pollers_stay(
            before + 1,
            Duration::from_millis(300),
            "followers ATTACH to the leader's poller — one iou-sqp thread \
             total, never one per queue",
        );

        // The worker registers /dev/fuse + wake fd on every ring right
        // after build; pin that registration works on SQPOLL rings.
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(efd >= 0);
        let efd_owned = unsafe { OwnedFd::from_raw_fd(efd) };
        for (name, ring) in [("leader", &leader), ("f1", &f1), ("f2", &f2)] {
            ring.submitter()
                .register_files(&[efd_owned.as_raw_fd()])
                .unwrap_or_else(|e| panic!("register_files on SQPOLL ring {name}: {e}"));
        }
    }

    /// Leader declined (published `None`) ⇒ followers build plain rings —
    /// never a private poller, never an error: the knob is
    /// warn-and-degrade like the classical rings.
    #[test]
    fn test_sqpoll_followers_fall_back_plain_when_leader_declined() {
        let _guard = sqpoll_test_guard();
        let group = sqpoll_group(50, None);
        group.leader.set(None).expect("fresh slot");
        let active = AtomicBool::new(true);
        let before = settle_pollers();
        let ring = build_queue_ring(16, 3, Some(&group), &active)
            .expect("fallback must not fail the mount");
        assert!(
            !ring.params().is_setup_sqpoll(),
            "declined group ⇒ plain follower rings"
        );
        assert_pollers_stay(
            before,
            Duration::from_millis(200),
            "a declined group must never spawn a private poller",
        );
    }

    /// A follower whose ATTACH_WQ target is not an io_uring (leader ring
    /// gone, fd reused) falls back to a plain ring — warn-and-degrade,
    /// never a mount failure, never a private poller.
    #[test]
    fn test_sqpoll_attach_failure_falls_back_plain() {
        let _guard = sqpoll_test_guard();
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(efd >= 0);
        let efd_owned = unsafe { OwnedFd::from_raw_fd(efd) };
        let group = sqpoll_group(50, None);
        group
            .leader
            .set(Some(efd_owned.as_raw_fd()))
            .expect("fresh slot");
        let active = AtomicBool::new(true);
        let before = settle_pollers();
        let ring = build_queue_ring(16, 1, Some(&group), &active)
            .expect("attach failure must degrade, not error");
        assert!(
            !ring.params().is_setup_sqpoll(),
            "un-attachable leader fd ⇒ plain follower ring"
        );
        assert_pollers_stay(
            before,
            Duration::from_millis(200),
            "an un-attachable leader fd must never spawn a private poller",
        );
    }

    /// Pool shutdown while a follower is still waiting for the leader
    /// outcome ⇒ immediate plain build — a mount torn down mid-setup must
    /// not park worker threads on the leader deadline.
    #[test]
    fn test_sqpoll_follower_shutdown_builds_plain_without_wait() {
        let group = sqpoll_group(50, None); // leader never publishes
        let active = AtomicBool::new(false); // pool already shut down
        let t0 = Instant::now();
        let ring = build_queue_ring(16, 1, Some(&group), &active).expect("plain build");
        assert!(!ring.params().is_setup_sqpoll());
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "inactive pool must short-circuit the leader wait"
        );
    }
}

/// L3 lever C (transport economy): the session's inbound pull must be pure
/// event-driven — no poll cadence, no timer registration per pull. The
/// M4→M10 hand-off measured the 200 ms `pop_timeout` churn at ~2.2 % clock
/// (per-pull timer registration + time-driver park, one `epoll_wait`/op).
#[cfg(test)]
mod inbound_queue_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn req(unique: u64) -> InboundUringReq {
        InboundUringReq {
            header_and_op: vec![0; 40],
            payload: Bytes::new(),
            unique,
            slot: ReplySlot::Ring {
                qid: 0,
                ent_idx: 0,
                commit_id: unique,
            },
            arrived_ns: crate::raw::read_phase::transport_now_ns(),
        }
    }

    /// Push→pop delivery order and payload identity (pin: the pull-path
    /// rework must not reorder or drop).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pop_delivers_pushed_requests_in_order() {
        let q = InboundQueue::new();
        let active = AtomicBool::new(true);
        let shutdown = tokio::sync::Notify::new();
        q.push(req(7)).expect("live receiver accepts");
        q.push(req(8)).expect("live receiver accepts");
        let a = q
            .pop(&active, &shutdown)
            .await
            .expect("first pushed request");
        let b = q
            .pop(&active, &shutdown)
            .await
            .expect("second pushed request");
        assert_eq!((a.unique, b.unique), (7, 8), "FIFO delivery");
    }

    /// A parked popper is woken by a push promptly (event-driven, not on a
    /// poll boundary).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pop_wakes_promptly_for_late_push() {
        let q = Arc::new(InboundQueue::new());
        let active = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let popper = tokio::spawn({
            let q = Arc::clone(&q);
            let active = Arc::clone(&active);
            let shutdown = Arc::clone(&shutdown);
            async move {
                let t0 = Instant::now();
                let r = q.pop(&active, &shutdown).await;
                (r, t0.elapsed())
            }
        });
        // Wait until the popper holds the rx lock (= it is inside pop).
        while q.rx.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
        q.push(req(42)).expect("live receiver accepts");
        let (r, elapsed) = popper.await.expect("popper task");
        assert_eq!(r.expect("pushed request").unique, 42);
        assert!(
            elapsed < Duration::from_millis(100),
            "push-to-delivery took {elapsed:?} — the pull path is parked on a \
             poll cadence instead of the channel wake"
        );
    }

    /// THE lever-C contract: shutdown wakes a parked popper immediately —
    /// `active = false` + `notify_waiters` must produce `None` in
    /// event-time, never after sleeping out a poll interval. (The retired
    /// 200 ms `pop_timeout` failed this by construction: its shutdown
    /// "wake" was a no-op, so a parked popper always slept the full
    /// timeout — 180 ms measured RED on this exact test.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pop_returns_none_promptly_on_shutdown() {
        let q = Arc::new(InboundQueue::new());
        let active = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let popper = tokio::spawn({
            let q = Arc::clone(&q);
            let active = Arc::clone(&active);
            let shutdown = Arc::clone(&shutdown);
            async move { q.pop(&active, &shutdown).await }
        });
        // Wait until the popper holds the rx lock (= it is inside pop)…
        while q.rx.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
        // …then a scheduling grace so it is PARKED (past its active check)
        // before the shutdown fires. Not synchronization — the assertion
        // clock starts after it, and it only makes the test stricter: a
        // poll-based pull now provably sleeps out its interval.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let t0 = Instant::now();
        // Exactly what FuseOverUring::shutdown does for the session path.
        active.store(false, Ordering::Release);
        shutdown.notify_waiters();
        let r = popper.await.expect("popper task");
        let elapsed = t0.elapsed();
        assert!(r.is_none(), "shutdown pop must drain to None");
        assert!(
            elapsed < Duration::from_millis(100),
            "shutdown-to-None took {elapsed:?} — the session pull path is \
             poll-based (timer park), not event-driven"
        );
    }
}
